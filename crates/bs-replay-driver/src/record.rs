// SPDX-License-Identifier: MIT
//! One-shot Tier 2 → Tier 3 capture helper.
//!
//! Linux-only, since Tier 2 fork-checkpoint capture lives in
//! `bs_replay::linux`. The function composes:
//!
//! 1. `Tier2Capture::capture` — fork+SIGSTOP+SEIZE+memory+regs.
//! 2. `tier2::to_payload` — serialize the captured state.
//! 3. `TraceWriter::create` — fresh on-disk trace dir.
//! 4. `take_checkpoint(payload)` — stash inside the trace.
//! 5. `finish` — close.
//! 6. `Tier2Capture::kill` — SIGKILL the captured fork (we now
//!    have the bytes on disk; no need to keep the live ring entry).
//!
//! The result is a `CaptureReport` summarising the size of what
//! was written. A future DAP `bs/replayCapture` handler is a thin
//! wrapper around this function.

#![cfg(target_os = "linux")]

use std::path::Path;

use bs_replay::linux::fork_self::LinuxForkSelfMechanism;
use bs_replay::linux::tier2::{self, Tier2Capture, Tier2Error};
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::{TraceWriteError, TraceWriter};

/// Summary of one capture call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CaptureReport {
    /// 1-based index of the checkpoint inside the trace.
    pub checkpoint_index: u64,
    /// Size of the encoded Tier 2 state payload, in bytes.
    pub payload_bytes: u64,
}

/// Capture a single Tier 2 checkpoint and write it to a *fresh*
/// trace directory at `trace_path`. The path must not already
/// exist (reuses `TraceWriter::create`'s policy of refusing to
/// overwrite).
///
/// `key` is a caller-defined u64 — typically the event index at
/// the moment of capture. Phase 5 doesn't interpret it; future
/// recorders use it for ordering and replay-seek.
pub fn capture_one_shot(
    trace_path: impl AsRef<Path>,
    manifest: &Manifest,
    key: u64,
) -> Result<CaptureReport, RecordError> {
    let mut mech = LinuxForkSelfMechanism::new();
    let cap = Tier2Capture::capture(&mut mech, key).map_err(RecordError::Tier2)?;
    let payload = tier2::to_payload(&cap.state);
    let payload_bytes = payload.len() as u64;

    let mut writer = TraceWriter::create(&trace_path, manifest)
        .map_err(RecordError::Trace)?;
    let checkpoint_index = 1; // first checkpoint in a fresh trace
    writer
        .take_checkpoint(payload)
        .map_err(RecordError::Trace)?;
    writer.finish().map_err(RecordError::Trace)?;

    cap.kill(&mut mech).map_err(RecordError::Tier2)?;

    Ok(CaptureReport { checkpoint_index, payload_bytes })
}

/// Errors arising from `capture_one_shot`.
#[derive(thiserror::Error, Debug)]
pub enum RecordError {
    /// Tier 2 capture or kill failed.
    #[error("tier2: {0}")]
    Tier2(Tier2Error),
    /// Trace writer failed.
    #[error("trace: {0}")]
    Trace(TraceWriteError),
}

// ---------------------------------------------------------------------------
// Tier 3 — full-program recorder
// ---------------------------------------------------------------------------

use std::ffi::CString;

use bs_replay_engine::record::linux::ptrace_driver::ProcMemReader;
use bs_replay_engine::record::linux::record_session::{
    self, record_to_completion, spawn_recorded_child, RecordSessionError, RecordSummary,
    SpawnError, Terminal,
};

/// How a recorded program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Tracee called `exit` / `exit_group`.
    Exited(i32),
    /// Tracee was killed by a signal.
    Signalled(i32),
    /// Recorder hit its iteration cap before the tracee exited
    /// — the trace is well-formed but truncated.
    IterationCap(u64),
}

/// Summary of one [`record_program`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReport {
    /// `Event::Syscall` events written.
    pub syscall_events: u64,
    /// `Event::Signal` events written. Always 0 in step 70 —
    /// the signal-stop dispatcher (next commit) bumps this.
    pub signal_events: u64,
    /// `Event::InstructionTrap` events written. Always 0 in
    /// step 70.
    pub instruction_traps: u64,
    /// Loop iterations executed (every event + every routed
    /// non-event stop counts).
    pub iterations: u64,
    /// How the tracee left the recorder loop.
    pub exit_status: ExitStatus,
}

/// Tunables for [`record_program`].
#[derive(Debug, Clone, Copy)]
pub struct RecordOptions {
    /// Hard cap on loop iterations. Defends against runaway
    /// programs in tests; production callers can set this to
    /// `u64::MAX`. Default `2_000_000` (covers a 30-minute
    /// session at 1000 syscalls/sec).
    pub max_iterations: u64,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self { max_iterations: 2_000_000 }
    }
}

/// Errors arising from [`record_program`].
#[derive(thiserror::Error, Debug)]
pub enum RecordProgramError {
    /// `spawn_recorded_child` failed.
    #[error("spawn: {0}")]
    Spawn(#[from] SpawnError),
    /// Couldn't open `/proc/<pid>/mem`. yama (kernel.yama.
    /// ptrace_scope) sometimes denies this even after a
    /// successful PTRACE_SEIZE.
    #[error("open /proc/<pid>/mem: {0}")]
    ProcMem(std::io::Error),
    /// Trace writer failed (almost always disk-full or
    /// permission-denied at create time).
    #[error("trace: {0}")]
    Trace(TraceWriteError),
    /// Recorder loop produced an error mid-session.
    #[error("recorder: {0}")]
    Session(RecordSessionError),
}

/// Record the program at `argv[0]` with the supplied `envp`,
/// writing the trace to a freshly-created directory at
/// `trace_dir`. Linux only.
///
/// Composes:
///
/// 1. `TraceWriter::create(trace_dir, manifest)`.
/// 2. `record_session::spawn_recorded_child(argv, envp)` —
///    PTRACE-only fork+exec (no seccomp filter; that's the
///    replay path's job).
/// 3. `ProcMemReader::open(child.pid())`.
/// 4. `record_to_completion` drives `step_until_event` until
///    the tracee exits or the iteration cap fires; emits
///    Event::Syscall / Event::Signal / Event::InstructionTrap
///    along the way.
///
/// Returns per-event counts + how the recording ended.
pub fn record_program(
    trace_dir: impl AsRef<Path>,
    manifest: &Manifest,
    argv: Vec<CString>,
    envp: Vec<CString>,
    options: RecordOptions,
) -> Result<RecordReport, RecordProgramError> {
    let mut writer =
        TraceWriter::create(&trace_dir, manifest).map_err(RecordProgramError::Trace)?;

    let mut child = spawn_recorded_child(argv, envp)?;
    let pid = child.pid();
    let reader = ProcMemReader::open(pid).map_err(RecordProgramError::ProcMem)?;

    let summary: RecordSummary =
        record_to_completion(&mut child, &reader, &mut writer, options.max_iterations)
            .map_err(RecordProgramError::Session)?;

    let _ = child.detach();
    writer.finish().map_err(RecordProgramError::Trace)?;

    Ok(RecordReport {
        syscall_events: summary.syscalls,
        signal_events: summary.signals,
        instruction_traps: summary.instruction_traps,
        iterations: summary.steps,
        exit_status: match summary.terminal {
            Some(Terminal::Exited(code)) => ExitStatus::Exited(code),
            Some(Terminal::Signalled(sig)) => ExitStatus::Signalled(sig),
            Some(Terminal::IterationCap(n)) => ExitStatus::IterationCap(n),
            None => ExitStatus::IterationCap(summary.steps),
        },
    })
}

// Suppress unused warning on the legacy step 7b record helper
// re-export — it's still in the public surface for the replay
// path's reference.
#[allow(unused_imports)]
use bs_replay_engine::record::linux::record_child::{self as _legacy_record_child};

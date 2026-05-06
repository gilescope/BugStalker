// SPDX-License-Identifier: MIT
//! `replay_program` — the high-level replay entry point.
//!
//! Mirrors [`crate::record::record_program`]: open a trace,
//! spawn a NOTIF-trapped child, drive
//! `recv_notif → apply_recorded_event → respond_intercept`
//! until the trace is exhausted or the tracee exits.
//!
//! Linux only.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::path::Path;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::{TraceReadError, TraceReader};
use bs_replay_engine::record::linux::ptrace_driver::{recv_notif, respond_intercept};
use bs_replay_engine::replay::linux::replay_child::{
    spawn_replay_child, ReplayChild, ReplaySpawnError,
};
use bs_replay_engine::replay::linux::shim::{
    apply_recorded_event, MemoryWriter, ProcMemWriter, ReplayError as ReplayShimError,
};

/// How a replay session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayExit {
    /// Tracee exited normally.
    Exited(i32),
    /// Tracee was killed by a signal.
    Signalled(i32),
    /// Trace ran out of `Event::Syscall` events before the
    /// tracee asked for one. The tracee is left running; the
    /// supervisor SIGKILLs it.
    TraceExhausted {
        /// How many events were applied before exhaustion.
        applied: u64,
    },
    /// The replay shim refused an event (mismatch, decode
    /// error, …). The tracee was SIGKILL'd.
    ShimRefused(ShimRefusedReason),
    /// Recorder loop hit the iteration cap.
    IterationCap(u64),
}

/// Why the shim refused — surfaced to the caller for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimRefusedReason {
    /// The shim returned `SyscallMismatch` — observed args
    /// differ from recorded.
    Mismatch(String),
    /// `RESULT_NOT_CAPTURED_YET` sentinel (legacy step 7b
    /// trace replayed against the strict shim).
    ResultNotCaptured,
    /// Captured-output blob failed to decode.
    Decode(String),
    /// Trace had a non-Syscall event where the shim expected
    /// one (Signal, InstructionTrap — these need their own
    /// replay handlers, not yet wired).
    UnsupportedEvent(String),
}

/// Per-event counts from a [`replay_program`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayReport {
    /// `Event::Syscall` events successfully applied.
    pub syscalls_applied: u64,
    /// `Event::Signal` events skipped (not yet replayed —
    /// step 71's signal replay needs cross-process
    /// PTRACE_SETSIGINFO; lands in a follow-up).
    pub signals_skipped: u64,
    /// `Event::InstructionTrap` events skipped (replay-side
    /// RAX rewrite + step-past lands with the signal replay).
    pub instruction_traps_skipped: u64,
    /// Total ioctl turns the supervisor executed.
    pub iterations: u64,
    /// Total bytes the shim wrote into the tracee via
    /// `ProcMemWriter`.
    pub bytes_written: u64,
    /// How the session ended.
    pub exit: Option<ReplayExit>,
}

/// Tunables for [`replay_program`].
#[derive(Debug, Clone, Copy)]
pub struct ReplayOptions {
    /// Hard cap on supervisor loop iterations. Defaults to
    /// `2_000_000`. Same shape as
    /// [`crate::record::RecordOptions`] for symmetry.
    pub max_iterations: u64,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self { max_iterations: 2_000_000 }
    }
}

/// Errors arising from [`replay_program`] outside the
/// per-event happy path.
#[derive(thiserror::Error, Debug)]
pub enum ReplayProgramError {
    /// Couldn't open the trace at `trace_dir`.
    #[error("trace open: {0}")]
    TraceOpen(#[from] TraceReadError),
    /// `spawn_replay_child` failed.
    #[error("spawn: {0}")]
    Spawn(#[from] ReplaySpawnError),
}

/// Replay the recorded program. Opens the trace, spawns a
/// fresh child with the same `argv`/`envp`, and supplies the
/// recorded results through the seccomp-NOTIF intercept path.
///
/// The supervisor stops on:
/// - tracee exit / kill (clean shutdown, return ReplayExit)
/// - shim refusal (SIGKILL the tracee; return ShimRefused)
/// - iteration cap reached (SIGKILL; return IterationCap)
///
/// Returns a [`ReplayReport`] with per-event counts and an
/// `exit` discriminator. The session is consumed exactly once;
/// repeated replays of the same trace need a fresh tracee.
pub fn replay_program(
    trace_dir: impl AsRef<Path>,
    argv: Vec<CString>,
    envp: Vec<CString>,
    options: ReplayOptions,
) -> Result<ReplayReport, ReplayProgramError> {
    let reader = TraceReader::open(&trace_dir)?;
    let child = spawn_replay_child(argv, envp)?;
    let pid = child.pid();
    let mut writer = ProcMemWriter::new(pid);

    let mut cursor = reader.cursor();
    let mut report = ReplayReport::default();

    'replay: for _ in 0..options.max_iterations {
        report.iterations += 1;

        // Pull the next syscall notification. If recv_notif
        // fails, the tracee likely exited under us.
        let notif = match recv_notif(child.listener()) {
            Ok(n) => n,
            Err(e) => {
                // Reap and stamp exit status.
                report.exit = Some(reap_exit(pid).unwrap_or_else(|| {
                    ReplayExit::Exited(0) // best-effort default
                }));
                tracing::debug!("replay_program: recv_notif ended ({e})");
                break 'replay;
            }
        };

        // Walk the trace until we find the next Event::Syscall;
        // intermediate Signal/InstructionTrap events are
        // counted but not replayed (yet).
        let event = loop {
            match cursor.next() {
                Ok(Some(e)) => match e {
                    Event::Syscall { .. } => break Some(e),
                    Event::Signal { .. } => {
                        report.signals_skipped += 1;
                        continue;
                    }
                    Event::InstructionTrap { .. } => {
                        report.instruction_traps_skipped += 1;
                        continue;
                    }
                    _ => continue, // Marker / PcMarker — nothing to replay
                },
                Ok(None) => break None,
                Err(e) => {
                    report.exit = Some(ReplayExit::ShimRefused(
                        ShimRefusedReason::Decode(format!("{e}")),
                    ));
                    let _ = child.shutdown();
                    return Ok(report);
                }
            }
        };
        let event = match event {
            Some(e) => e,
            None => {
                report.exit = Some(ReplayExit::TraceExhausted {
                    applied: report.syscalls_applied,
                });
                // SIGKILL the tracee; we can't satisfy further notifs.
                let _ = child.shutdown();
                return Ok(report);
            }
        };

        match apply_recorded_event(&notif, &event, &mut writer) {
            Ok(resp) => {
                let bytes = match &event {
                    Event::Syscall { output, .. } => {
                        // Each successful intercept may have
                        // written out-buffer bytes; track total.
                        match bs_replay_engine::record::syscall_capture::CapturedSyscall::decode_output(
                            0, [0; 6], 0, output,
                        ) {
                            Ok(cap) => cap
                                .regions
                                .iter()
                                .map(|r| r.bytes.len() as u64)
                                .sum::<u64>(),
                            Err(_) => 0,
                        }
                    }
                    _ => 0,
                };
                report.bytes_written += bytes;
                report.syscalls_applied += 1;
                if let Err(e) = respond_intercept(
                    child.listener(),
                    resp.notif_id,
                    resp.result,
                    /*err=*/ 0,
                ) {
                    tracing::debug!(
                        "replay_program: respond_intercept failed ({e}); reaping"
                    );
                    report.exit = Some(reap_exit(pid).unwrap_or(ReplayExit::Exited(0)));
                    break 'replay;
                }
            }
            Err(shim_err) => {
                let reason = classify_shim_error(&shim_err);
                report.exit = Some(ReplayExit::ShimRefused(reason));
                let _ = child.shutdown();
                return Ok(report);
            }
        }
    }

    if report.exit.is_none() {
        // Hit the iteration cap.
        report.exit = Some(ReplayExit::IterationCap(report.iterations));
        let _ = child.shutdown();
    }

    Ok(report)
}

fn classify_shim_error(err: &ReplayShimError) -> ShimRefusedReason {
    use bs_replay_engine::replay::linux::shim::ReplayError as RE;
    match err {
        RE::Mismatch(m) => ShimRefusedReason::Mismatch(m.to_string()),
        RE::ResultNotCaptured { .. } => ShimRefusedReason::ResultNotCaptured,
        RE::Decode(d) => ShimRefusedReason::Decode(format!("{d}")),
        RE::UnexpectedEvent { got } => ShimRefusedReason::UnsupportedEvent(got.clone()),
    }
}

fn reap_exit(pid: i32) -> Option<ReplayExit> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r > 0 {
        if libc::WIFEXITED(status) {
            return Some(ReplayExit::Exited(libc::WEXITSTATUS(status)));
        }
        if libc::WIFSIGNALED(status) {
            return Some(ReplayExit::Signalled(libc::WTERMSIG(status)));
        }
    }
    None
}

/// Suppress unused warning on the deliberately-not-used
/// MemoryWriter trait re-export — kept in scope for future
/// custom-writer plumbing.
#[allow(dead_code)]
fn _writer_lifeline<W: MemoryWriter>() {}

/// Re-export the inner child handle for callers that want to
/// inspect the live tracee outside the helper (e.g. attach a
/// debugger to the running replay).
#[allow(unused_imports)]
pub use bs_replay_engine::replay::linux::replay_child::ReplayChild as ReplayChildHandle;

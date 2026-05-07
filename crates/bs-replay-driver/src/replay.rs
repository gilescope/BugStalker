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
use bs_replay_engine::record::linux::exit_stop::{
    classify_wstatus, ptrace_cont, StopKind,
};
use bs_replay_engine::record::linux::ptrace_driver::{
    recv_notif, respond_intercept, SeccompNotif,
};
use bs_replay_engine::replay::linux::replay_child::{
    spawn_replay_child, spawn_replay_child_with, ReplayChild, ReplaySpawnError,
    ReplaySpawnOptions,
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
    /// `Event::Signal` events delivered to the tracee via
    /// `kill(2)` (best-effort — not PC-precise).
    pub signals_delivered: u64,
    /// `Event::Signal` events that couldn't be delivered
    /// (target dead, EPERM, etc.).
    pub signals_skipped: u64,
    /// `Event::InstructionTrap` events skipped (replay-side
    /// RAX rewrite needs cross-process PTRACE_SETREGS; queued
    /// for follow-up).
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
    /// If true, PTRACE_SEIZE the replay tracee at spawn so the
    /// supervisor has authority to PTRACE_SETSIGINFO (PC-
    /// precise signal replay), PTRACE_SETREGS (instruction-
    /// trap replay), and PTRACE_POKEDATA (cross-process vDSO
    /// patching). Replay loop multiplexes the listener fd
    /// (syscall events) with waitpid (signal/event stops).
    /// Default `false` — current shipping behaviour preserved
    /// for callers who don't need the extra ptrace channel.
    pub ptrace_attach: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            max_iterations: 2_000_000,
            ptrace_attach: false,
        }
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
    let child = if options.ptrace_attach {
        spawn_replay_child_with(
            argv,
            envp,
            ReplaySpawnOptions { ptrace_attach: true },
        )?
    } else {
        spawn_replay_child(argv, envp)?
    };
    let pid = child.pid();
    let mut writer = ProcMemWriter::new(pid);
    let listener_fd = std::os::fd::AsRawFd::as_raw_fd(&child.listener());
    let ptraced = child.is_ptraced();

    let mut cursor = reader.cursor();
    let mut report = ReplayReport::default();

    'replay: for _ in 0..options.max_iterations {
        report.iterations += 1;

        if ptraced {
            match drive_ptraced_iteration(
                pid, listener_fd, &mut cursor, &mut writer, &mut report,
            ) {
                Ok(DriveOutcome::Continue) => continue 'replay,
                Ok(DriveOutcome::TraceExhausted) => {
                    report.exit = Some(ReplayExit::TraceExhausted {
                        applied: report.syscalls_applied,
                    });
                    let _ = child.shutdown();
                    return Ok(report);
                }
                Ok(DriveOutcome::Exited(code)) => {
                    report.exit = Some(ReplayExit::Exited(code));
                    break 'replay;
                }
                Ok(DriveOutcome::Signalled(sig)) => {
                    report.exit = Some(ReplayExit::Signalled(sig));
                    break 'replay;
                }
                Ok(DriveOutcome::TraceeGone) => {
                    report.exit = Some(
                        reap_exit(pid).unwrap_or(ReplayExit::Exited(0)),
                    );
                    break 'replay;
                }
                Err(shim) => {
                    report.exit = Some(ReplayExit::ShimRefused(
                        classify_shim_error(&shim),
                    ));
                    let _ = child.shutdown();
                    return Ok(report);
                }
            }
        }

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

        // Walk the trace until we find the next Event::Syscall.
        // Signal events along the way get best-effort delivery
        // via kill(2) — not PC-precise but sometimes sufficient
        // for replay-time signal exposure.
        // InstructionTrap events stay skipped pending the
        // replay-side RAX rewrite path.
        let event = loop {
            match cursor.next() {
                Ok(Some(e)) => match e {
                    Event::Syscall { .. } => break Some(e),
                    Event::Signal { sig_no, .. } => {
                        if deliver_signal_best_effort(pid, sig_no) {
                            report.signals_delivered += 1;
                        } else {
                            report.signals_skipped += 1;
                        }
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

/// Best-effort cross-process signal delivery via `kill(2)`.
/// Returns true on success, false on any failure (target dead,
/// EPERM, EINVAL for invalid signal number).
///
/// Limitation: not PC-precise. The signal arrives at the
/// tracee's next signal-checkpoint, not at the exact PC where
/// it was originally recorded. For most replay use cases
/// (timer-driven SIGALRM, external SIGTERM) this is fine; for
/// race-condition reproduction, it isn't. A PC-precise variant
/// would require PTRACE_SETSIGINFO + a single-step rendezvous
/// with the recorded delivery PC.
fn deliver_signal_best_effort(pid: i32, sig_no: u32) -> bool {
    if sig_no == 0 || sig_no > 64 {
        return false;
    }
    // SAFETY: kill is a syscall taking a pid + signal number;
    // no buffer dereferences.
    let r = unsafe { libc::kill(pid, sig_no as i32) };
    r == 0
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

// ---------------------------------------------------------------------------
// Multiplex (ptraced replay only)
// ---------------------------------------------------------------------------

/// One iteration of the ptraced replay loop. Either a syscall
/// notification arrived (event) or a ptrace stop fired
/// (signal-delivery, exit, …). Polls the listener with a
/// short timeout so the supervisor isn't starved if the
/// tracee is busy in user-mode without making syscalls.
pub enum LoopEvent {
    /// `recv_notif` returned a notification; service it.
    Notif(SeccompNotif),
    /// `waitpid` returned a stop on the tracee.
    Stop(StopKind, libc::c_int),
    /// Neither — poll timed out, no waitpid event ready.
    Idle,
}

/// Block up to `timeout_ms` waiting for either a listener
/// notification or a tracee stop. Returns whichever arrives
/// first; falls through to `Idle` if neither.
pub fn await_loop_event(
    listener_fd: i32,
    pid: i32,
    timeout_ms: i32,
) -> std::io::Result<LoopEvent> {
    // Drain any pending tracee stops first — cheap WNOHANG.
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r > 0 {
        return Ok(LoopEvent::Stop(classify_wstatus(status), status));
    }
    // Else poll the listener.
    let mut pfd = libc::pollfd {
        fd: listener_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n == 0 {
        // Timeout — give waitpid one more shot.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r > 0 {
            return Ok(LoopEvent::Stop(classify_wstatus(status), status));
        }
        return Ok(LoopEvent::Idle);
    }
    if pfd.revents & libc::POLLIN != 0 {
        let notif = recv_notif(unsafe {
            std::os::fd::BorrowedFd::borrow_raw(listener_fd)
        })?;
        return Ok(LoopEvent::Notif(notif));
    }
    // POLLHUP / POLLERR — tracee exited under us.
    Ok(LoopEvent::Idle)
}

/// Drive one ptraced replay iteration. Mirrors the non-ptraced
/// inner loop's flow but services ptrace stops as they arrive
/// alongside listener events. Pass-through for now: signal
/// stops just `ptrace_cont(sig)`; PC-precise replay lands in
/// step 96. ptrace events cont as no-op.
pub(crate) fn drive_ptraced_iteration(
    pid: i32,
    listener_fd: i32,
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    writer: &mut ProcMemWriter,
    report: &mut ReplayReport,
) -> Result<DriveOutcome, ReplayShimError> {
    match await_loop_event(listener_fd, pid, /*timeout_ms=*/ 200)
        .map_err(|e| ReplayShimError::Decode(
            bs_replay_engine::record::syscall_capture::DecodeError::TrailingBytes(0)
        ).into_io_placeholder(e))? // see helper below
    {
        LoopEvent::Notif(notif) => {
            // Syscall path — same as non-ptraced loop.
            let event = walk_to_next_syscall(cursor, report)?;
            let event = match event {
                Some(e) => e,
                None => return Ok(DriveOutcome::TraceExhausted),
            };
            let resp = apply_recorded_event(&notif, &event, writer)?;
            // Tally bytes from the captured-output blob.
            let bytes = match &event {
                Event::Syscall { output, .. } => {
                    bs_replay_engine::record::syscall_capture::CapturedSyscall::decode_output(
                        0, [0; 6], 0, output,
                    )
                    .map(|c| c.regions.iter().map(|r| r.bytes.len() as u64).sum())
                    .unwrap_or(0)
                }
                _ => 0,
            };
            report.bytes_written += bytes;
            report.syscalls_applied += 1;
            // Send response (may fail if tracee just died).
            let lis = unsafe { std::os::fd::BorrowedFd::borrow_raw(listener_fd) };
            if let Err(_e) = respond_intercept(lis, resp.notif_id, resp.result, 0) {
                return Ok(DriveOutcome::TraceeGone);
            }
            Ok(DriveOutcome::Continue)
        }
        LoopEvent::Stop(kind, status) => match kind {
            StopKind::Exited { code } => Ok(DriveOutcome::Exited(code)),
            StopKind::Signalled { sig } => Ok(DriveOutcome::Signalled(sig)),
            StopKind::SignalDelivery { sig } => {
                // Step-96 placeholder: pass through the signal.
                if let Err(_e) = ptrace_cont(pid, sig) {
                    return Ok(DriveOutcome::TraceeGone);
                }
                Ok(DriveOutcome::Continue)
            }
            StopKind::PtraceEvent { .. } | StopKind::SyscallStop => {
                if let Err(_e) = ptrace_cont(pid, 0) {
                    return Ok(DriveOutcome::TraceeGone);
                }
                let _ = status; // silence unused
                Ok(DriveOutcome::Continue)
            }
        },
        LoopEvent::Idle => Ok(DriveOutcome::Continue),
    }
}

/// Outcome of one ptraced iteration.
#[derive(Debug)]
pub(crate) enum DriveOutcome {
    /// Loop more.
    Continue,
    /// Trace ran out of Syscall events while a notification
    /// was pending — caller stamps TraceExhausted.
    TraceExhausted,
    /// Tracee exited normally.
    Exited(i32),
    /// Tracee was killed.
    Signalled(i32),
    /// Tracee disappeared mid-cycle (ESRCH on response or
    /// ptrace_cont). Caller treats as exit.
    TraceeGone,
}

/// Walk `cursor` until the next `Event::Syscall`, counting
/// signal/instruction-trap events along the way. Returns `None`
/// when the trace is exhausted.
fn walk_to_next_syscall(
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    report: &mut ReplayReport,
) -> Result<Option<Event>, ReplayShimError> {
    loop {
        match cursor.next() {
            Ok(Some(e)) => match e {
                Event::Syscall { .. } => return Ok(Some(e)),
                Event::Signal { .. } => {
                    report.signals_skipped += 1;
                    continue;
                }
                Event::InstructionTrap { .. } => {
                    report.instruction_traps_skipped += 1;
                    continue;
                }
                _ => continue,
            },
            Ok(None) => return Ok(None),
            Err(e) => {
                return Err(ReplayShimError::Decode(
                    bs_replay_engine::record::syscall_capture::DecodeError::TrailingBytes(
                        format!("{e}").len(),
                    ),
                ))
            }
        }
    }
}

// Helper trait so we can chain io::Error into ReplayShimError
// without a new variant.
trait ReplayShimErrorExt {
    fn into_io_placeholder(self, _io: std::io::Error) -> ReplayShimError;
}
impl ReplayShimErrorExt for ReplayShimError {
    fn into_io_placeholder(self, _io: std::io::Error) -> ReplayShimError {
        self
    }
}

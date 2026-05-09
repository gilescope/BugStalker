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
use bs_replay_engine::format::event::InstructionTrapKind;
use bs_replay_engine::format::{TraceReadError, TraceReader};
use bs_replay_engine::record::linux::exit_stop::{
    StopKind, UserRegsX86_64, classify_wstatus, ptrace_cont, ptrace_singlestep,
};
#[cfg(target_arch = "x86_64")]
use bs_replay_engine::record::linux::exit_stop::{get_regs, set_regs};
use bs_replay_engine::record::linux::instrs::{InstrKind, classify_at_pc};
use bs_replay_engine::record::linux::ptrace_driver::{
    ProcMemReader, SeccompNotif, recv_notif, respond_intercept,
};
use bs_replay_engine::record::linux::signals::ptrace_setsiginfo;
use bs_replay_engine::record::syscall_capture::MemoryReader;
use bs_replay_engine::replay::linux::replay_child::{
    ReplayChild, ReplaySpawnError, ReplaySpawnOptions, spawn_replay_child, spawn_replay_child_with,
};
use bs_replay_engine::replay::linux::shim::{
    MemoryWriter, ProcMemWriter, ReplayError as ReplayShimError, apply_recorded_event,
};

/// How a replay session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayReport {
    /// `Event::Syscall` events successfully applied.
    pub syscalls_applied: u64,
    /// `Event::Signal` events delivered to the tracee via
    /// `kill(2)` (best-effort) or `PTRACE_SETSIGINFO`
    /// (content-precise, ptraced replays). Includes
    /// `signals_pc_precise` as a strict subset.
    pub signals_delivered: u64,
    /// `Event::Signal` events delivered at the *recorded* PC
    /// via single-step rendezvous (subset of
    /// `signals_delivered`). The remainder were content-precise
    /// but delivered at the tracee's current PC.
    pub signals_pc_precise: u64,
    /// `Event::Signal` events that couldn't be delivered
    /// (target dead, EPERM, etc.).
    pub signals_skipped: u64,
    /// `Event::InstructionTrap` events replayed via
    /// PTRACE_SETREGS (ptraced mode only).
    pub instruction_traps_replayed: u64,
    /// `Event::InstructionTrap` events skipped (non-ptraced
    /// mode, or no matching trap fired during replay).
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
    /// supervisor has authority to PTRACE_SETSIGINFO (content-
    /// precise signal replay), PTRACE_SETREGS (instruction-
    /// trap replay), and PTRACE_POKEDATA (cross-process vDSO
    /// patching). Replay loop multiplexes the listener fd
    /// (syscall events) with waitpid (signal/event stops).
    /// Default `false` — current shipping behaviour preserved
    /// for callers who don't need the extra ptrace channel.
    pub ptrace_attach: bool,
    /// If true, patch the replay tracee's vDSO at startup so
    /// libc's gettimeofday/clock_gettime/time/getcpu fast paths
    /// route through real syscalls — matching what the recorder
    /// captured (provided the recording also had
    /// `RecordOptions::patch_vdso = true`). Implies
    /// `ptrace_attach = true` since the patcher uses
    /// `PTRACE_POKEDATA`. Default `false`.
    pub patch_vdso: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            max_iterations: 2_000_000,
            ptrace_attach: false,
            patch_vdso: false,
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
    let need_ptrace = options.ptrace_attach || options.patch_vdso;

    // Compute fd-table fixups from the trace's recorded fd set
    // (V2 traces) vs the supervisor's current fd-table. V1 traces
    // present an empty `initial_fds` and fall back to
    // "inherit whatever the supervisor has open" — same behaviour
    // as before step 112. The list_open_fds call on /proc/self/fd
    // is best-effort: any I/O failure yields an empty supervisor
    // set, which `fd_diff_actions` interprets as "open everything
    // the recorded child had". The actions land in the replay
    // child between fork and execve.
    let supervisor_fds =
        bs_replay_engine::record::linux::proc_fd::list_open_fds(unsafe { libc::getpid() })
            .unwrap_or_default();
    let recorded_fds = &reader.manifest().initial_fds;
    let file_actions = bs_replay_engine::replay::linux::file_actions::fd_diff_actions(
        &supervisor_fds,
        recorded_fds,
    );

    let child = if need_ptrace {
        spawn_replay_child_with(
            argv,
            envp,
            ReplaySpawnOptions {
                ptrace_attach: true,
                file_actions: file_actions.clone(),
            },
        )?
    } else if !file_actions.is_empty() {
        // V2 trace without ptrace: still need the file actions.
        spawn_replay_child_with(
            argv,
            envp,
            ReplaySpawnOptions {
                ptrace_attach: false,
                file_actions,
            },
        )?
    } else {
        spawn_replay_child(argv, envp)?
    };
    let pid = child.pid();
    let mut writer = ProcMemWriter::new(pid);

    #[cfg(target_arch = "x86_64")]
    if options.patch_vdso && child.is_ptraced() {
        // The vDSO patcher runs from the supervisor side via
        // PTRACE_POKEDATA; tracee must be ptraced. Errors are
        // soft-fail (log + continue) — patching is opt-in and
        // a missing patch only means time-related calls won't
        // route through the recorder, not that replay breaks.
        match bs_replay_engine::record::linux::vdso_patch::scan_remote_vdso(pid) {
            Ok(symbols) if !symbols.is_empty() => {
                if let Err(e) = bs_replay_engine::record::linux::vdso_patch::apply_vdso_trampolines(
                    pid, &symbols,
                ) {
                    tracing::warn!("replay_program: vDSO patch failed (non-fatal): {e}");
                }
            }
            Ok(_) => {} // no symbols — host kernel without vDSO
            Err(e) => {
                tracing::warn!("replay_program: vDSO scan failed (non-fatal): {e}");
            }
        }
    }
    // On aarch64 the patch payload isn't ported yet; the
    // option is accepted but no-op.
    #[cfg(not(target_arch = "x86_64"))]
    let _ = options.patch_vdso;
    let listener_fd = std::os::fd::AsRawFd::as_raw_fd(&child.listener());
    let ptraced = child.is_ptraced();

    let mut cursor = reader.cursor();
    let mut report = ReplayReport::default();

    'replay: for _ in 0..options.max_iterations {
        report.iterations += 1;

        if ptraced {
            match drive_ptraced_iteration(pid, listener_fd, &mut cursor, &mut writer, &mut report) {
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
                    report.exit = Some(reap_exit(pid).unwrap_or(ReplayExit::Exited(0)));
                    break 'replay;
                }
                Err(shim) => {
                    report.exit = Some(ReplayExit::ShimRefused(classify_shim_error(&shim)));
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
                    report.exit = Some(ReplayExit::ShimRefused(ShimRefusedReason::Decode(
                        format!("{e}"),
                    )));
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
                    tracing::debug!("replay_program: respond_intercept failed ({e}); reaping");
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
pub fn await_loop_event(listener_fd: i32, pid: i32, timeout_ms: i32) -> std::io::Result<LoopEvent> {
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
        let notif = recv_notif(unsafe { std::os::fd::BorrowedFd::borrow_raw(listener_fd) })?;
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
            // Syscall path — same as non-ptraced loop, but
            // signal events along the way are delivered with
            // PC-precise rendezvous when possible
            // (single-step until RIP matches recorded.pc),
            // falling back to content-precise SETSIGINFO and
            // finally best-effort kill on failure.
            let event = walk_to_next_syscall_full(
                cursor, report, Some(pid), Some(listener_fd),
            )?;
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
            #[cfg(target_arch = "x86_64")]
            StopKind::SignalDelivery { sig }
                if sig == libc::SIGSEGV || sig == libc::SIGILL =>
            {
                // Could be a non-deterministic instruction
                // (RDTSC/RDTSCP/RDRAND/RDSEED/CPUID) that
                // PR_SET_TSC or CPUID-mask is trapping. If
                // we recognise the PC, install the recorded
                // result via PTRACE_SETREGS, advance RIP,
                // swallow the signal.
                match try_replay_instruction_trap(pid, cursor, report) {
                    Ok(true) => {
                        if let Err(_e) = ptrace_cont(pid, /*sig=*/ 0) {
                            return Ok(DriveOutcome::TraceeGone);
                        }
                        Ok(DriveOutcome::Continue)
                    }
                    Ok(false) => {
                        // Not a recognised instruction trap;
                        // pass the signal through.
                        if let Err(_e) = ptrace_cont(pid, sig) {
                            return Ok(DriveOutcome::TraceeGone);
                        }
                        Ok(DriveOutcome::Continue)
                    }
                    Err(_) => {
                        // Read failure (tracee gone?); pass
                        // signal through best-effort.
                        if let Err(_e) = ptrace_cont(pid, sig) {
                            return Ok(DriveOutcome::TraceeGone);
                        }
                        Ok(DriveOutcome::Continue)
                    }
                }
            }
            StopKind::SignalDelivery { sig } => {
                // Other signals — pass through unchanged.
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

/// Walk `cursor` until the next `Event::Syscall`. Signal
/// events along the way get PC-not-precise-but-content-precise
/// delivery via PTRACE_SETSIGINFO; instruction-trap events
/// stay skipped (replay landing in step 97).
fn walk_to_next_syscall(
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    report: &mut ReplayReport,
) -> Result<Option<Event>, ReplayShimError> {
    walk_to_next_syscall_with(cursor, report, None)
}

fn walk_to_next_syscall_with(
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    report: &mut ReplayReport,
    inject_pid: Option<i32>,
) -> Result<Option<Event>, ReplayShimError> {
    walk_to_next_syscall_full(cursor, report, inject_pid, None)
}

/// Three-mode signal delivery:
/// - `inject_pid = None`                          : best-effort
///   `kill(2)`, no SETSIGINFO, no PC rendezvous.
/// - `inject_pid = Some(pid)`, `listener_fd = None`: content-
///   precise (PTRACE_SETSIGINFO), no PC rendezvous.
/// - both `Some(_)`                               : PC-precise
///   (single-step rendezvous + PTRACE_SETSIGINFO).
fn walk_to_next_syscall_full(
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    report: &mut ReplayReport,
    inject_pid: Option<i32>,
    listener_fd: Option<i32>,
) -> Result<Option<Event>, ReplayShimError> {
    loop {
        match cursor.next() {
            Ok(Some(e)) => match e {
                Event::Syscall { .. } => return Ok(Some(e)),
                Event::Signal {
                    sig_no,
                    pc,
                    siginfo,
                    ..
                } => {
                    let outcome = match (inject_pid, listener_fd) {
                        (Some(pid), Some(lfd)) => {
                            deliver_recorded_signal_at_pc(pid, lfd, sig_no, pc, &siginfo).map(Some)
                        }
                        (Some(pid), None) => deliver_recorded_signal(pid, sig_no, &siginfo)
                            .map(|_| Some(DeliveryFidelity::ContentOnly)),
                        (None, _) => Ok(None),
                    };
                    match outcome {
                        Ok(Some(DeliveryFidelity::PcPrecise { .. })) => {
                            report.signals_delivered += 1;
                            report.signals_pc_precise += 1;
                        }
                        Ok(Some(DeliveryFidelity::ContentOnly)) => {
                            report.signals_delivered += 1;
                        }
                        Ok(None) => {
                            // Best-effort kill — already accounted
                            // for in the non-ptraced path.
                            report.signals_skipped += 1;
                        }
                        Err(_) => {
                            report.signals_skipped += 1;
                        }
                    }
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
                ));
            }
        }
    }
}

/// Synchronously deliver a recorded signal to a ptraced tracee
/// at the recorded PC where possible. PC-precise variant:
/// before delivering, single-step the tracee until RIP matches
/// the recorded delivery PC. Bounded to avoid runaway stepping;
/// falls back to deliver-at-current-PC if the rendezvous is
/// abandoned.
///
/// x86-64 only — the rendezvous reads `RIP` via `PTRACE_GETREGS`
/// and the aarch64 register-read primitive isn't yet wired
/// through the driver. On aarch64 the call falls through to
/// content-precise delivery (no rendezvous).
fn deliver_recorded_signal_at_pc(
    pid: i32,
    listener_fd: i32,
    sig_no: u32,
    pc: u64,
    siginfo: &[u8],
) -> std::io::Result<DeliveryFidelity> {
    #[cfg(target_arch = "x86_64")]
    let fidelity = match rendezvous_at_pc(pid, listener_fd, pc, MAX_RENDEZVOUS_STEPS) {
        Ok(RendezvousOutcome::Reached { steps }) => DeliveryFidelity::PcPrecise { steps },
        Ok(RendezvousOutcome::AlreadyThere) => DeliveryFidelity::PcPrecise { steps: 0 },
        // PastIt / CapHit / AbortedSyscall / AbortedStop / Timeout
        // → fall back to deliver-at-current-PC.
        _ => DeliveryFidelity::ContentOnly,
    };
    #[cfg(not(target_arch = "x86_64"))]
    let fidelity = {
        let _ = (listener_fd, pc);
        DeliveryFidelity::ContentOnly
    };
    deliver_recorded_signal(pid, sig_no, siginfo)?;
    Ok(fidelity)
}

/// Outcome of a single-step rendezvous attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendezvousOutcome {
    /// RIP already matched on entry.
    AlreadyThere,
    /// Single-stepped to the target PC.
    Reached {
        /// How many single-steps it took.
        steps: u32,
    },
    /// Tracee's RIP is past the target — can't go back.
    PastIt,
    /// Listener became readable mid-step — a syscall is queued
    /// and we'd hang if we tried to single-step over it.
    AbortedSyscall,
    /// Got a non-SIGTRAP stop mid-step (e.g. SIGSEGV at an
    /// instruction trap).
    AbortedStop,
    /// Step cap exceeded.
    CapHit,
    /// Tracee exited or was killed during the rendezvous.
    TraceeGone,
}

/// Fidelity tier of a single signal delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryFidelity {
    /// `siginfo_t` matches the recording but the tracee
    /// receives the signal at its current PC, not the
    /// recorded delivery PC.
    ContentOnly,
    /// `siginfo_t` matches AND the tracee was rendezvoused to
    /// the recorded PC via single-step before delivery.
    PcPrecise {
        /// Number of single-steps it took to rendezvous.
        steps: u32,
    },
}

/// Cap on single-step iterations. 64 is enough for sync
/// signals where the recorded PC is the immediately-next
/// instruction; async signals (timer SIGALRM, external
/// SIGTERM) typically can't be PC-rendezvoused at all and
/// hit AbortedSyscall fast.
const MAX_RENDEZVOUS_STEPS: u32 = 64;

/// Bounded poll-readable check on the listener fd.
fn listener_is_readable(fd: i32, timeout_ms: i32) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pfd.revents & libc::POLLIN != 0)
}

#[cfg(target_arch = "x86_64")]
fn rendezvous_at_pc(
    pid: i32,
    listener_fd: i32,
    target_pc: u64,
    max_steps: u32,
) -> std::io::Result<RendezvousOutcome> {
    // Initial position check.
    let regs = get_regs(pid)?;
    if regs.rip == target_pc {
        return Ok(RendezvousOutcome::AlreadyThere);
    }
    if regs.rip > target_pc {
        return Ok(RendezvousOutcome::PastIt);
    }

    for steps in 1..=max_steps {
        // Don't step into a syscall — the tracee would block
        // in seccomp NOTIF wait and waitpid would hang. Bail
        // out so the caller services the listener first.
        if listener_is_readable(listener_fd, 0)? {
            return Ok(RendezvousOutcome::AbortedSyscall);
        }
        ptrace_singlestep(pid, /*sig=*/ 0)?;
        // Wait for the SIGTRAP that PTRACE_SINGLESTEP raises.
        // Use a polling waitpid with a per-step deadline so
        // we don't hang if the tracee blocked unexpectedly.
        let mut status: libc::c_int = 0;
        let mut waited_ms = 0i32;
        let step_timeout_ms = 200;
        let kind = loop {
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r > 0 {
                break classify_wstatus(status);
            }
            if r < 0 {
                return Ok(RendezvousOutcome::TraceeGone);
            }
            if waited_ms >= step_timeout_ms {
                // Defensive — assume the tracee is stuck.
                return Ok(RendezvousOutcome::AbortedSyscall);
            }
            // Sleep 1ms and retry.
            unsafe {
                let ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 1_000_000,
                };
                libc::nanosleep(&ts, std::ptr::null_mut());
            }
            waited_ms += 1;
        };
        match kind {
            StopKind::SignalDelivery { sig } if sig == libc::SIGTRAP => {
                let regs = get_regs(pid)?;
                if regs.rip == target_pc {
                    return Ok(RendezvousOutcome::Reached { steps });
                }
                if regs.rip > target_pc {
                    return Ok(RendezvousOutcome::PastIt);
                }
                continue;
            }
            StopKind::Exited { .. } | StopKind::Signalled { .. } => {
                return Ok(RendezvousOutcome::TraceeGone);
            }
            _ => return Ok(RendezvousOutcome::AbortedStop),
        }
    }
    Ok(RendezvousOutcome::CapHit)
}

/// Synchronously deliver a recorded signal to a ptraced tracee.
/// (Content-precise — `siginfo_t` matches the recording, but
/// the tracee receives it at its *current* PC.) Used as the
/// fallback path when a PC-precise rendezvous can't complete.
fn deliver_recorded_signal(pid: i32, sig_no: u32, siginfo: &[u8]) -> std::io::Result<()> {
    if sig_no == 0 || sig_no > 64 {
        return Err(std::io::Error::other(format!(
            "deliver_recorded_signal: sig_no {sig_no} out of range"
        )));
    }
    // Step 1: queue the signal.
    let r = unsafe { libc::kill(pid, sig_no as i32) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Step 2: wait for the signal-delivery stop. Block here —
    // we expect the kernel to deliver promptly.
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let kind = classify_wstatus(status);
    let actual_sig = match kind {
        StopKind::SignalDelivery { sig } => sig,
        StopKind::Exited { .. } | StopKind::Signalled { .. } => {
            return Err(std::io::Error::other(
                "tracee exited before signal delivery stop",
            ));
        }
        _ => {
            return Err(std::io::Error::other(format!(
                "unexpected stop kind awaiting signal: {kind:?}"
            )));
        }
    };
    // Step 3: install the recorded siginfo.
    ptrace_setsiginfo(pid, siginfo)?;
    // Step 4: resume with the signal pending.
    ptrace_cont(pid, actual_sig)?;
    Ok(())
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

/// Attempt instruction-trap replay. Returns `Ok(true)` if the
/// stop was a classified non-deterministic instruction at a
/// PC matched against the next `Event::InstructionTrap` and
/// the recorded result was installed. `Ok(false)` if the PC
/// isn't a classified instruction or no matching event in the
/// trace. `Err` on ptrace I/O failure.
///
/// x86-64 only — uses iced-x86 to classify the instruction +
/// PTRACE_GETREGS/SETREGS to write the recorded result. The
/// aarch64 analogue (CNTVCT_EL0 / MRS / AT) is queued.
#[cfg(target_arch = "x86_64")]
fn try_replay_instruction_trap(
    pid: i32,
    cursor: &mut bs_replay_engine::format::EventCursor<'_>,
    report: &mut ReplayReport,
) -> std::io::Result<bool> {
    // Read the tracee's RIP and the bytes there.
    let regs = get_regs(pid)?;
    let pc = regs.rip;

    // Open /proc/<pid>/mem for the bytes-at-PC read.
    let reader = ProcMemReader::open(pid)?;
    let bytes = reader.read(pc, 16);
    let kind = match classify_at_pc(pc, &bytes) {
        Some(k) => k,
        None => return Ok(false),
    };

    // Walk the cursor for the next Event::InstructionTrap.
    // If the very next interesting event is a non-trap, we
    // skip it (count as "skipped") rather than consume it
    // out of order.
    loop {
        match cursor.next() {
            Ok(Some(e)) => match e {
                Event::InstructionTrap {
                    pc: tpc,
                    kind: tkind,
                    result,
                } => {
                    if pc != tpc {
                        // PC mismatch — stash the count and
                        // pass through.
                        report.instruction_traps_skipped += 1;
                        return Ok(false);
                    }
                    install_recorded_trap(pid, &regs, kind, tkind, &result)?;
                    report.instruction_traps_replayed += 1;
                    return Ok(true);
                }
                Event::Syscall { .. } => {
                    // The next event in the trace is a syscall,
                    // not an instruction trap. Means the
                    // recording didn't trap at this PC; pass
                    // through and let the cursor stay at the
                    // syscall for the syscall-handling path.
                    // (We can't put it back, so we lose ordering;
                    // count as skip.)
                    report.instruction_traps_skipped += 1;
                    return Ok(false);
                }
                Event::Signal { .. } => {
                    // Defer signals for the next walk.
                    report.signals_skipped += 1;
                    continue;
                }
                _ => continue,
            },
            Ok(None) => return Ok(false),
            Err(_) => return Ok(false),
        }
    }
}

/// Write the recorded result vector into the tracee's
/// registers per the format crate's per-kind contract, then
/// advance RIP past the instruction. Mirrors the recorder's
/// `synthesise_trap_result` shape.
#[cfg(target_arch = "x86_64")]
fn install_recorded_trap(
    pid: i32,
    regs: &UserRegsX86_64,
    observed_kind: InstrKind,
    recorded_kind: InstructionTrapKind,
    result: &[u64],
) -> std::io::Result<()> {
    // Mismatched kind is suspicious but not fatal (instruction
    // encoding may overlap across CPU generations); honour the
    // observed kind for register placement.
    let _ = recorded_kind;
    let mut new_regs = *regs;
    match observed_kind {
        InstrKind::Rdtsc | InstrKind::Rdtscp => {
            // result[0] is the 64-bit TSC. EDX:EAX = high:low.
            let tsc = result.first().copied().unwrap_or(0);
            new_regs.rax = tsc & 0xFFFF_FFFF;
            new_regs.rdx = tsc >> 32;
            // RDTSCP also writes IA32_TSC_AUX into ECX —
            // recorded format doesn't carry it; leave RCX
            // unchanged.
        }
        InstrKind::Rdrand | InstrKind::Rdseed => {
            // result format (step 101): [value, success, dest_id].
            // Older traces have just [value, success] — fall
            // back to RAX (id=0) when result.len() == 2.
            let value = result.first().copied().unwrap_or(0);
            let dest_id = result.get(2).copied().unwrap_or(0);
            write_to_register(&mut new_regs, dest_id, value);
        }
        InstrKind::Cpuid => {
            // result = [eax, ebx, ecx, edx].
            let eax = result.first().copied().unwrap_or(0);
            let ebx = result.get(1).copied().unwrap_or(0);
            let ecx = result.get(2).copied().unwrap_or(0);
            let edx = result.get(3).copied().unwrap_or(0);
            new_regs.rax = eax & 0xFFFF_FFFF;
            new_regs.rbx = ebx & 0xFFFF_FFFF;
            new_regs.rcx = ecx & 0xFFFF_FFFF;
            new_regs.rdx = edx & 0xFFFF_FFFF;
        }
    }
    // Advance RIP past the trapped instruction.
    new_regs.rip = regs.rip.saturating_add(instruction_byte_len(observed_kind));
    set_regs(pid, &new_regs)?;
    Ok(())
}

/// Bytes per encoding for the five trapped instructions.
/// Mirrors the recorder's `instruction_byte_len`.
#[cfg(target_arch = "x86_64")]
fn instruction_byte_len(kind: InstrKind) -> u64 {
    match kind {
        InstrKind::Rdtsc => 2,
        InstrKind::Rdtscp => 3,
        InstrKind::Rdrand => 3,
        InstrKind::Rdseed => 3,
        InstrKind::Cpuid => 2,
    }
}

/// Map a 0..=15 destination-register id to the matching
/// [`UserRegsX86_64`] field and write `value` into its low 64
/// bits. Out-of-range ids fall back to RAX (matches the
/// recorder's `x86_64_register_id` clamp).
#[cfg(target_arch = "x86_64")]
fn write_to_register(regs: &mut UserRegsX86_64, dest_id: u64, value: u64) {
    match dest_id {
        0 => regs.rax = value,
        1 => regs.rcx = value,
        2 => regs.rdx = value,
        3 => regs.rbx = value,
        4 => regs.rsp = value,
        5 => regs.rbp = value,
        6 => regs.rsi = value,
        7 => regs.rdi = value,
        8 => regs.r8 = value,
        9 => regs.r9 = value,
        10 => regs.r10 = value,
        11 => regs.r11 = value,
        12 => regs.r12 = value,
        13 => regs.r13 = value,
        14 => regs.r14 = value,
        15 => regs.r15 = value,
        _ => regs.rax = value,
    }
}

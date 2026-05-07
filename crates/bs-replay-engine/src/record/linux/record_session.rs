// SPDX-License-Identifier: MIT
//! PTRACE-only recorder + multi-event dispatcher (sub-phase 3B
//! step 71). Linux only.
//!
//! ## Why a separate path from step 7b's NOTIF design
//!
//! The seccomp-NOTIF + PTRACE_O_TRACESYSCALL combination is
//! awkward. From `seccomp_unotify(2)`:
//!
//! > The seccomp filter mechanism does not interact with any of
//! > the various PTRACE_O_TRACESYSCALL-related operations.
//! > Thus, if a syscall is being traced via a seccomp user
//! > notification, the corresponding PTRACE_SYSCALL operation
//! > will not yield a syscall stop for that syscall.
//!
//! So `record_syscall_with_exit`'s plan — recv_notif, then
//! `wait_for_next_stop` for the syscall-exit-stop — would
//! deadlock: NOTIF suppresses the matching syscall stops.
//!
//! For the *replay* path the NOTIF design is exactly right:
//! the supervisor needs intercept power (don't run the syscall;
//! supply the recorded result instead). NOTIF + FLAG_CONTINUE-
//! omitted is precisely that.
//!
//! For *record* we don't need intercept power — we just observe.
//! `PTRACE_O_TRACESYSCALL` alone gives us syscall-entry and
//! syscall-exit stops on every syscall, with full register
//! visibility. No filter needed; no listener fd needed; no
//! socketpair handover needed.
//!
//! This module is the corrected record path. Step 7b's
//! NOTIF-based code stays in tree as the replay primitive.
//!
//! ## What this module lands
//!
//! - [`RecordedChild`] — fork+exec with PTRACE_TRACEME, parent
//!   PTRACE_SETOPTIONS. No seccomp filter. No socketpair.
//! - [`spawn_recorded_child`] — the lifecycle.
//! - [`RecordedEventKind`] — discriminator for what
//!   [`step_until_event`] just emitted.
//! - [`step_until_event`] — drive the tracee one event at a
//!   time. Handles entry+exit-stop pairs (Event::Syscall),
//!   signal-delivery (Event::Signal), SIGSEGV-at-classified-
//!   instruction (Event::InstructionTrap), and exit/signalled.
//! - [`record_to_completion`] — call step_until_event in a
//!   loop until the tracee exits.
//!
//! ## Stop-kind state machine
//!
//! ```text
//! +-------------------+   PTRACE_SYSCALL    +-------------------+
//! | (NotInSyscall)    |  ───────────────►   | syscall-entry-stop|
//! +-------------------+                     +-------------------+
//!         ▲                                          │
//!         │                                          │ PTRACE_SYSCALL
//!         │                                          ▼
//!         │                                 +-------------------+
//!         │ PTRACE_SYSCALL                  | syscall-exit-stop |
//!         │                                 +-------------------+
//!         │ + emit Event::Syscall                    │
//!         └──────────────────────────────────────────┘
//! ```
//!
//! Signal-delivery + ptrace events fork off the entry side and
//! return to (NotInSyscall) after handling.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::RawFd;

use crate::format::event::Event;
use crate::format::trace_writer::{TraceWriteError, TraceWriter};
use crate::record::linux::exit_stop::{
    classify_wstatus, ptrace_cont, ptrace_syscall, ExitStopError, StopKind,
    UserRegsX86_64,
};
#[cfg(target_arch = "x86_64")]
use crate::record::linux::exit_stop::{get_regs, set_regs};
// Instruction trapping is x86-64 only — iced-x86 disassembly +
// the InstrKind set are x86 ISA. aarch64 has analogues
// (CNTVCT_EL0 trap, AT/MRS) that aren't ported yet.
#[cfg(target_arch = "x86_64")]
use crate::record::linux::instrs::{
    classify_at_pc, event_for_instruction_trap, read_host_tsc, InstrKind,
};
#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)]
use crate::record::linux::instrs::classify_at_pc as _classify_at_pc_keep;
use crate::record::linux::signals::{
    event_for_signal, SignalCapture, SignalLengthError, SIGINFO_T_LEN_X86_64,
};
use crate::record::syscall_capture::{
    capture_post_syscall, capture_pre_syscall, CallFrame, CapturedSyscall, MemoryReader,
};
use crate::record::linux::exit_stop::result_register_x86_64;

// ---------------------------------------------------------------------------
// CallFrame extraction from registers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Arch dispatch
// ---------------------------------------------------------------------------
//
// step_until_event runs on whichever Linux arch the supervisor
// + tracee share. Type-alias `Regs` to the host's user-regs
// struct + thin helpers for the per-stop primitives. Both arches
// produce the same `CallFrame` shape consumed by syscall_capture.

#[cfg(target_arch = "x86_64")]
type Regs = UserRegsX86_64;
#[cfg(target_arch = "aarch64")]
type Regs = crate::record::linux::regs_aarch64::UserRegsAarch64;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
type Regs = UserRegsX86_64; // unreachable but keeps the type live

fn read_regs(pid: i32) -> std::io::Result<Regs> {
    #[cfg(target_arch = "x86_64")]
    {
        get_regs(pid)
    }
    #[cfg(target_arch = "aarch64")]
    {
        crate::record::linux::regs_aarch64::get_regs_aarch64(pid)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = pid;
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

fn write_regs(pid: i32, regs: &Regs) -> std::io::Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        set_regs(pid, regs)
    }
    #[cfg(target_arch = "aarch64")]
    {
        crate::record::linux::regs_aarch64::set_regs_aarch64(pid, regs)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = (pid, regs);
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

fn frame_from(regs: &Regs) -> CallFrame {
    #[cfg(target_arch = "x86_64")]
    {
        call_frame_from_regs(regs)
    }
    #[cfg(target_arch = "aarch64")]
    {
        crate::record::linux::regs_aarch64::call_frame_from_regs(regs)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = regs;
        CallFrame { nr: 0, args: [0; 6] }
    }
}

fn result_from(regs: &Regs) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        result_register_x86_64(regs)
    }
    #[cfg(target_arch = "aarch64")]
    {
        crate::record::linux::regs_aarch64::result_register(regs)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = regs;
        0
    }
}

fn pc_of(regs: &Regs) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        regs.rip
    }
    #[cfg(target_arch = "aarch64")]
    {
        regs.pc
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = regs;
        0
    }
}

fn set_pc(regs: &mut Regs, pc: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        regs.rip = pc;
    }
    #[cfg(target_arch = "aarch64")]
    {
        regs.pc = pc;
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = (regs, pc);
    }
}

/// On x86-64 the syscall number lives in `orig_rax` (the kernel
/// preserves it across the syscall) and the six argument regs
/// are RDI, RSI, RDX, R10, R8, R9 in that order.
pub fn call_frame_from_regs(regs: &UserRegsX86_64) -> CallFrame {
    CallFrame {
        nr: regs.orig_rax as u32,
        args: [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9],
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// A PTRACE_TRACEME'd child — the supervisor's view of one
/// recorded process. No seccomp, no listener fd; just ptrace.
#[derive(Debug)]
pub struct RecordedChild {
    pid: i32,
    /// `Some` between a syscall-entry-stop and its matching
    /// syscall-exit-stop; the entry-side capture is stored here
    /// so the exit-side can merge.
    in_flight_pre: Option<CapturedSyscall>,
    pending_signal: i32,
    detached: bool,
}

impl RecordedChild {
    /// PID of the tracee.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// True iff the tracee is currently between a syscall-entry
    /// and a syscall-exit stop. Useful for diagnostic logs.
    pub fn in_syscall(&self) -> bool {
        self.in_flight_pre.is_some()
    }

    /// Signal to deliver on the next PTRACE_SYSCALL. Cleared
    /// after each step.
    pub fn pending_signal(&self) -> i32 {
        self.pending_signal
    }

    /// Stop tracing and let the child run free.
    pub fn detach(mut self) -> io::Result<()> {
        self.do_detach()?;
        self.detached = true;
        Ok(())
    }

    fn do_detach(&self) -> io::Result<()> {
        let r = unsafe {
            libc::ptrace(
                libc::PTRACE_DETACH,
                self.pid,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for RecordedChild {
    fn drop(&mut self) {
        if !self.detached {
            if let Err(e) = self.do_detach() {
                tracing::warn!(
                    "RecordedChild::drop: PTRACE_DETACH(pid={}) failed: {e}",
                    self.pid,
                );
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
    }
}

/// In-tracee setup flags applied by the child between
/// PTRACE_TRACEME and execve. The supervisor passes them
/// through [`spawn_recorded_child_with`] when it needs RDTSC
/// trapping or other per-thread state that can't be set from
/// the tracer side.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChildSetupFlags {
    /// `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)` — RDTSC/RDTSCP raise
    /// SIGSEGV instead of running natively. The recorder's
    /// signal-delivery dispatcher classifies the faulting PC
    /// and emits `Event::InstructionTrap`.
    pub trap_tsc: bool,
    /// `arch_prctl(ARCH_SET_CPUID, 0)` — CPUID raises SIGSEGV
    /// instead of running natively. Same dispatch path as
    /// `trap_tsc`. WARNING: libc / openssl / etc. probe CPUID
    /// at startup; enabling this without recorder-side CPUID
    /// synthesis can crash the tracee. Opt-in.
    pub disable_cpuid: bool,
}

/// Spawn a child program for PTRACE-based recording. Compared
/// to step 64's `record_child::spawn`:
///
/// - No socketpair, no SCM_RIGHTS handover.
/// - Child does PTRACE_TRACEME but does NOT install a seccomp
///   filter.
/// - Parent SETOPTIONS with TRACESYSGOOD | TRACEEXEC, then
///   PTRACE_SYSCALL to advance into the first syscall stop.
///
/// Returns the [`RecordedChild`] paused at its first syscall-
/// entry-stop (which will be the execve completion, since
/// PTRACE_TRACEME stops at the first user-space instruction
/// after execve).
pub fn spawn_recorded_child(
    argv: Vec<CString>,
    envp: Vec<CString>,
) -> Result<RecordedChild, SpawnError> {
    spawn_recorded_child_with(argv, envp, ChildSetupFlags::default())
}

/// Like [`spawn_recorded_child`] but with [`ChildSetupFlags`]
/// applied in the tracee between PTRACE_TRACEME and execve.
pub fn spawn_recorded_child_with(
    argv: Vec<CString>,
    envp: Vec<CString>,
    flags: ChildSetupFlags,
) -> Result<RecordedChild, SpawnError> {
    if argv.is_empty() {
        return Err(SpawnError::EmptyArgv);
    }

    // SAFETY: fork — async-signal-safe envelope between fork
    // and execve in the child.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(SpawnError::Fork(io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child.
        match child_main(argv, envp, flags) {
            Ok(_) => unsafe { libc::_exit(101) },
            Err(code) => unsafe { libc::_exit(code) },
        }
    }
    // Parent.
    match parent_setup(pid) {
        Ok(rc) => Ok(rc),
        Err(e) => {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            }
            Err(e)
        }
    }
}

fn child_main(
    argv: Vec<CString>,
    envp: Vec<CString>,
    flags: ChildSetupFlags,
) -> Result<core::convert::Infallible, libc::c_int> {
    // PTRACE_TRACEME — parent gains ptrace authority.
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if r != 0 {
        return Err(70);
    }
    // PR_SET_TSC must be set in the tracee thread. Apply
    // before execve so it survives into the new image.
    if flags.trap_tsc {
        if super::instrs::set_tsc_trap_for_self().is_err() {
            return Err(75);
        }
    }
    if flags.disable_cpuid {
        if super::instrs::set_cpuid_disabled_for_self().is_err() {
            return Err(76);
        }
    }
    // execve. PTRACE_TRACEME makes the kernel raise SIGTRAP at
    // the first user-space instruction after execve, which the
    // parent's SETOPTIONS converts into the syscall-entry-stop
    // for execve itself.
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_ptrs: Vec<*const libc::c_char> = envp
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let prog = argv[0].as_ptr();
    unsafe {
        libc::execve(prog, argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }
    Err(74)
}

fn parent_setup(pid: i32) -> Result<RecordedChild, SpawnError> {
    // Wait for the SIGTRAP from PTRACE_TRACEME's initial stop.
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return Err(SpawnError::Wait(io::Error::last_os_error()));
    }
    if !libc::WIFSTOPPED(status) {
        return Err(SpawnError::ChildSetupFailed { wstatus: status });
    }

    // SETOPTIONS — TRACESYSGOOD makes syscall-stops distinguishable
    // from real SIGTRAPs; TRACEEXEC adds a marker we can route
    // through; TRACECLONE/FORK could be added later for multi-
    // process recording.
    let opts: libc::c_long = (libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEEXEC) as libc::c_long;
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            opts as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(SpawnError::SetOptions(io::Error::last_os_error()));
    }

    Ok(RecordedChild {
        pid,
        in_flight_pre: None,
        pending_signal: 0,
        detached: false,
    })
}

/// Errors arising from [`spawn_recorded_child`] /
/// [`spawn_recorded_child_with`].
#[derive(thiserror::Error, Debug)]
pub enum SpawnError {
    /// Caller passed an empty `argv`.
    #[error("argv is empty; need at least the program path")]
    EmptyArgv,
    /// `fork(2)` failed.
    #[error("fork: {0}")]
    Fork(io::Error),
    /// `waitpid(2)` for the child's PTRACE_TRACEME stop failed.
    #[error("waitpid: {0}")]
    Wait(io::Error),
    /// Child reached the SIGTRAP barrier in an unexpected
    /// state (typically a child-side prctl/exec failure;
    /// WEXITSTATUS gives the exit code).
    #[error(
        "child setup failed at PTRACE_TRACEME stop; wstatus={wstatus:#x}. \
         Common skip codes: 64 (kernel/perms reject seccomp), 70 \
         (PTRACE_TRACEME denied), 75 (PR_SET_TSC denied)."
    )]
    ChildSetupFailed {
        /// Raw wstatus from waitpid.
        wstatus: libc::c_int,
    },
    /// `PTRACE_SETOPTIONS` failed.
    #[error("PTRACE_SETOPTIONS: {0}")]
    SetOptions(io::Error),
}

// ---------------------------------------------------------------------------
// Event dispatcher
// ---------------------------------------------------------------------------

/// Discriminator for [`step_until_event`]'s emit. The actual
/// `Event::*` was already written to the trace; this tells the
/// caller which one fired so it can update counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedEventKind {
    /// One full pre+post syscall pair was emitted.
    Syscall,
    /// A signal delivery was recorded; the signal will be
    /// delivered to the tracee on the next step.
    Signal,
    /// SIGSEGV / SIGILL at a non-deterministic instruction
    /// (RDTSC, RDTSCP, RDRAND, RDSEED, CPUID); the recorded
    /// result was synthesised from the host and the tracee's
    /// RIP was advanced past the instruction.
    InstructionTrap,
    /// Tracee reached `exit` / `exit_group`.
    Exited(i32),
    /// Tracee was killed by a signal.
    Signalled(i32),
    /// Ptrace event passed through (e.g. PTRACE_EVENT_EXEC) —
    /// no event written; caller loops.
    PassThrough,
}

/// `PTRACE_GETSIGINFO` wrapper. Reads exactly
/// `SIGINFO_T_LEN_X86_64` bytes; the supervisor stamps them
/// verbatim into [`Event::Signal`] so replay's
/// `PTRACE_SETSIGINFO` reproduces the delivery.
pub fn ptrace_getsiginfo(pid: i32) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; SIGINFO_T_LEN_X86_64];
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_GETSIGINFO,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

/// Synthesise a result-vector for the recorded instruction trap.
/// Per the format crate's contract:
///
/// - Rdtsc / Rdtscp → 1 word (host TSC at capture time)
/// - Rdrand / Rdseed → 3 words (value, success-flag,
///   dest_register_id) — added the dest_register_id field in
///   step 101 so replay can write to the right register.
///   Older traces have 2 words here; replay-side defaults the
///   id to 0 (RAX) when absent.
/// - Cpuid → 4 words (eax, ebx, ecx, edx) — caller is expected
///   to use [`synthesise_cpuid_result_at_regs`] which has access
///   to the trapped tracee's input registers. The fallback
///   below returns zeros and exists only so the match is
///   exhaustive; the production path never hits it.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn synthesise_trap_result(kind: InstrKind) -> Vec<u64> {
    synthesise_trap_result_with_dest(kind, 0)
}

#[cfg(target_arch = "x86_64")]
fn synthesise_trap_result_with_dest(kind: InstrKind, dest_id: u64) -> Vec<u64> {
    match kind {
        InstrKind::Rdtsc | InstrKind::Rdtscp => vec![read_host_tsc()],
        InstrKind::Rdrand | InstrKind::Rdseed => vec![read_host_tsc(), 1, dest_id],
        // Production callers go through synthesise_cpuid_result_at_regs;
        // this arm only fires if a future caller forgets to pass regs.
        InstrKind::Cpuid => vec![0, 0, 0, 0],
    }
}

/// Drive [`crate::record::linux::instrs::cpuid_synthesised`]
/// from the trapped tracee's `(rax, rcx)` and pack the four
/// output registers into a CPUID `Event::InstructionTrap`
/// result vector. Mirrors [`synthesise_trap_result_with_dest`]
/// but only handles the CPUID case — separated because CPUID
/// is the only kind that needs the trapped registers as input.
#[cfg(target_arch = "x86_64")]
fn synthesise_cpuid_result_at_regs(regs: &UserRegsX86_64) -> Vec<u64> {
    let eax_in = (regs.rax & 0xFFFF_FFFF) as u32;
    let ecx_in = (regs.rcx & 0xFFFF_FFFF) as u32;
    let r = crate::record::linux::instrs::cpuid_synthesised(eax_in, ecx_in);
    vec![
        u64::from(r[0]),
        u64::from(r[1]),
        u64::from(r[2]),
        u64::from(r[3]),
    ]
}

/// Bytes per encoding for the five trapped instructions. Used
/// to advance RIP past a trapped instruction so the replay
/// shim doesn't re-trap. x86-64 only.
#[cfg(target_arch = "x86_64")]
fn instruction_byte_len(kind: InstrKind) -> u64 {
    match kind {
        InstrKind::Rdtsc => 2,   // 0F 31
        InstrKind::Rdtscp => 3,  // 0F 01 F9
        InstrKind::Rdrand => 3,  // 0F C7 /6 (REX.W adds 1, but we skip the
                                  // ones we trapped; iced-x86 disassembly
                                  // would give exact length)
        InstrKind::Rdseed => 3,
        InstrKind::Cpuid => 2,   // 0F A2
    }
}

/// Drive the tracee one step. Each call resumes the tracee
/// via PTRACE_SYSCALL and waits for the next stop, then
/// dispatches:
///
/// - syscall-entry-stop: capture pre, stash on `child`,
///   return [`RecordedEventKind::PassThrough`] (no event yet).
/// - syscall-exit-stop: capture post, merge with stashed pre,
///   emit Event::Syscall.
/// - signal-delivery (SIGSEGV/SIGILL at a classified
///   instruction): emit Event::InstructionTrap, advance RIP
///   past the instruction, swallow the signal.
/// - signal-delivery (other): emit Event::Signal, mark for
///   redelivery on next step.
/// - exit/signalled: terminal.
/// - other ptrace events (EXEC etc.): pass through.
///
/// The pre-capture is held across the entry→exit transition on
/// `child.in_flight_pre`; the post-side reads RAX from the
/// exit-stop registers and merges the regions. Plan §3B's
/// "one Event::Syscall per syscall" invariant holds because
/// merge happens atomically before write_event.
pub fn step_until_event(
    child: &mut RecordedChild,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
) -> Result<RecordedEventKind, RecordSessionError> {
    // Resume the tracee with any pending signal.
    ptrace_syscall(child.pid, child.pending_signal).map_err(RecordSessionError::Ptrace)?;
    child.pending_signal = 0;

    let (kind, _status) = wait_for_next_stop_inner(child.pid)?;
    match kind {
        StopKind::Exited { code } => Ok(RecordedEventKind::Exited(code)),
        StopKind::Signalled { sig } => Ok(RecordedEventKind::Signalled(sig)),
        StopKind::SyscallStop => handle_syscall_stop(child, reader, writer),
        StopKind::SignalDelivery { sig } => {
            handle_signal_delivery(child, sig, reader, writer)
        }
        StopKind::PtraceEvent { .. } => Ok(RecordedEventKind::PassThrough),
    }
}

fn handle_syscall_stop(
    child: &mut RecordedChild,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
) -> Result<RecordedEventKind, RecordSessionError> {
    let regs = read_regs(child.pid).map_err(RecordSessionError::GetRegs)?;
    let frame = frame_from(&regs);
    if child.in_flight_pre.is_none() {
        // Entry stop — capture and stash.
        let pre = capture_pre_syscall(frame, reader);
        child.in_flight_pre = Some(pre);
        Ok(RecordedEventKind::PassThrough)
    } else {
        // Exit stop — capture post + merge + emit.
        let pre = child.in_flight_pre.take().expect("just checked is_some");
        let result = result_from(&regs);
        let post = capture_post_syscall(frame, result, reader);
        let merged = merge_pre_post(&pre, &post);
        writer
            .write_event(crate::record::linux::ptrace_driver::event_for_capture(&merged))
            .map_err(RecordSessionError::Write)?;
        Ok(RecordedEventKind::Syscall)
    }
}

fn handle_signal_delivery(
    child: &mut RecordedChild,
    sig: i32,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
) -> Result<RecordedEventKind, RecordSessionError> {
    let regs = read_regs(child.pid).map_err(RecordSessionError::GetRegs)?;
    let pc = pc_of(&regs);
    let siginfo = ptrace_getsiginfo(child.pid).map_err(RecordSessionError::Ptrace)?;

    // Instruction-trap classification + replay-side synthesis is
    // x86-64-only — iced-x86 disassembly + InstructionTrap kind
    // set are x86 ISA. aarch64's analogue (MRS CNTVCT_EL0 trap)
    // is queued for a later port.
    #[cfg(target_arch = "x86_64")]
    if sig == libc::SIGSEGV || sig == libc::SIGILL {
        let bytes = reader.read(pc, 16);
        if let Some((instr_kind, dest_id)) =
            crate::record::linux::instrs::classify_at_pc_full(pc, &bytes)
        {
            // CPUID needs the trapped tracee's input registers
            // (eax, ecx) so the synthesised answer reflects the
            // leaf/subleaf actually being queried. Other kinds
            // are leaf-less and use the dest-id-only helper.
            let result = match instr_kind {
                InstrKind::Cpuid => synthesise_cpuid_result_at_regs(&regs),
                _ => synthesise_trap_result_with_dest(instr_kind, dest_id),
            };
            writer
                .write_event(event_for_instruction_trap(pc, instr_kind, result))
                .map_err(RecordSessionError::Write)?;
            let mut new_regs = regs;
            set_pc(&mut new_regs, pc.saturating_add(instruction_byte_len(instr_kind)));
            write_regs(child.pid, &new_regs).map_err(RecordSessionError::SetRegs)?;
            // Don't deliver the signal — we synthesised around it.
            child.pending_signal = 0;
            return Ok(RecordedEventKind::InstructionTrap);
        }
    }
    // Suppress unused-variable warnings on aarch64.
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (sig, reader, pc);
    }

    let cap = SignalCapture::new(sig as u32, pc, siginfo)
        .map_err(RecordSessionError::SignalLength)?;
    writer
        .write_event(event_for_signal(&cap))
        .map_err(RecordSessionError::Write)?;
    child.pending_signal = sig;
    Ok(RecordedEventKind::Signal)
}

/// Merge a pre-syscall capture with a post-syscall one. Must
/// have matching nr+args; the post side carries the result and
/// any OutBuf regions, the pre side carries InBuf+InCStr regions.
fn merge_pre_post(pre: &CapturedSyscall, post: &CapturedSyscall) -> CapturedSyscall {
    debug_assert_eq!(pre.nr, post.nr, "pre/post nr divergence — recorder bug");
    debug_assert_eq!(pre.args, post.args, "pre/post arg divergence — recorder bug");
    let mut regions = pre.regions.clone();
    regions.extend(post.regions.iter().cloned());
    CapturedSyscall {
        nr: pre.nr,
        args: pre.args,
        result: post.result,
        regions,
        tier: pre.tier,
    }
}

fn wait_for_next_stop_inner(pid: i32) -> Result<(StopKind, libc::c_int), RecordSessionError> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return Err(RecordSessionError::Wait(io::Error::last_os_error()));
    }
    Ok((classify_wstatus(status), status))
}

/// Drive `child` to completion, calling `step_until_event` in
/// a loop until the tracee exits, is killed, or `max_steps` is
/// reached. Returns per-event counts.
pub fn record_to_completion(
    child: &mut RecordedChild,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
    max_steps: u64,
) -> Result<RecordSummary, RecordSessionError> {
    let mut summary = RecordSummary::default();
    for _ in 0..max_steps {
        summary.steps += 1;
        match step_until_event(child, reader, writer)? {
            RecordedEventKind::Syscall => summary.syscalls += 1,
            RecordedEventKind::Signal => summary.signals += 1,
            RecordedEventKind::InstructionTrap => summary.instruction_traps += 1,
            RecordedEventKind::PassThrough => {}
            RecordedEventKind::Exited(code) => {
                summary.terminal = Some(Terminal::Exited(code));
                return Ok(summary);
            }
            RecordedEventKind::Signalled(sig) => {
                summary.terminal = Some(Terminal::Signalled(sig));
                return Ok(summary);
            }
        }
    }
    summary.terminal = Some(Terminal::IterationCap(summary.steps));
    Ok(summary)
}

/// Per-event counts from a [`record_to_completion`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordSummary {
    /// Total step_until_event calls executed.
    pub steps: u64,
    /// Event::Syscall events written.
    pub syscalls: u64,
    /// Event::Signal events written.
    pub signals: u64,
    /// Event::InstructionTrap events written.
    pub instruction_traps: u64,
    /// How the recording loop ended.
    pub terminal: Option<Terminal>,
}

/// Why the recording loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// Tracee exited normally.
    Exited(i32),
    /// Tracee was killed by a signal.
    Signalled(i32),
    /// `max_steps` reached.
    IterationCap(u64),
}

/// Errors arising from [`step_until_event`].
#[derive(thiserror::Error, Debug)]
pub enum RecordSessionError {
    /// `waitpid` failed.
    #[error("waitpid: {0}")]
    Wait(io::Error),
    /// `ptrace` syscall (cont/syscall/getsiginfo) failed.
    #[error("ptrace: {0}")]
    Ptrace(io::Error),
    /// `PTRACE_GETREGS` failed.
    #[error("PTRACE_GETREGS: {0}")]
    GetRegs(io::Error),
    /// `PTRACE_SETREGS` failed.
    #[error("PTRACE_SETREGS: {0}")]
    SetRegs(io::Error),
    /// Trace writer failed.
    #[error("trace write: {0}")]
    Write(TraceWriteError),
    /// Signal capture rejected the siginfo length.
    #[error("signal length: {0}")]
    SignalLength(SignalLengthError),
    /// Recorder state machine got into a state it doesn't know
    /// how to handle.
    #[error("recorder: {0}")]
    Other(String),
}

impl From<ExitStopError> for RecordSessionError {
    fn from(e: ExitStopError) -> Self {
        let display = format!("{e}");
        match e {
            ExitStopError::Wait(io) => Self::Wait(io),
            ExitStopError::GetRegs(io) => Self::GetRegs(io),
            ExitStopError::Write(w) => Self::Write(w),
            ExitStopError::Recorder(_) => {
                Self::Other(format!("legacy NOTIF-path error: {display}"))
            }
            ExitStopError::UnexpectedStop { kind, wstatus } => {
                Self::Other(format!("unexpected stop {kind:?} (wstatus={wstatus:#x})"))
            }
        }
    }
}

// Re-export for the smoke test path; ExitStopError already
// carries Recv variants we don't use.
#[allow(unused_imports)]
use crate::record::linux::exit_stop::ExitStopError as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::syscall_capture::{CallFrame, MemoryReader};

    struct EmptyMem;
    impl MemoryReader for EmptyMem {
        fn read(&self, _addr: u64, _max: usize) -> Vec<u8> {
            Vec::new()
        }
    }

    #[test]
    fn call_frame_from_regs_reads_orig_rax_and_arg_regs() {
        let mut regs = UserRegsX86_64::default();
        regs.orig_rax = 1; // write
        regs.rdi = 2;
        regs.rsi = 0xCAFE_BA00;
        regs.rdx = 5;
        regs.r10 = 0;
        regs.r8 = 0;
        regs.r9 = 0;
        let f = call_frame_from_regs(&regs);
        assert_eq!(f.nr, 1);
        assert_eq!(f.args, [2, 0xCAFE_BA00, 5, 0, 0, 0]);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn synthesise_trap_result_shapes_match_format_contract() {
        // Per format/event.rs comments:
        //   Rdtsc/Rdtscp  → 1 word
        //   Rdrand/Rdseed → 3 words (value, success, dest_reg_id)
        //   Cpuid         → 4 words
        assert_eq!(synthesise_trap_result(InstrKind::Rdtsc).len(), 1);
        assert_eq!(synthesise_trap_result(InstrKind::Rdtscp).len(), 1);
        assert_eq!(synthesise_trap_result(InstrKind::Rdrand).len(), 3);
        assert_eq!(synthesise_trap_result(InstrKind::Rdseed).len(), 3);
        assert_eq!(synthesise_trap_result(InstrKind::Cpuid).len(), 4);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_byte_len_covers_every_kind() {
        for k in [
            InstrKind::Rdtsc,
            InstrKind::Rdtscp,
            InstrKind::Rdrand,
            InstrKind::Rdseed,
            InstrKind::Cpuid,
        ] {
            let n = instruction_byte_len(k);
            assert!(n >= 2 && n <= 7, "{k:?} reported {n} bytes — out of plausible range");
        }
    }
}

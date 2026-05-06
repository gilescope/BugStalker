// SPDX-License-Identifier: MIT
//! Sub-phase 3B step 7b — syscall-exit-stop result capture.
//!
//! Step 7 captured the *entry-side* of a syscall (args, curated
//! `InBuf` bytes, paths). The result + curated `OutBuf` bytes
//! (and catch-all post-state windows) need a second observation
//! after the kernel has run the syscall. The mechanism on Linux:
//!
//! 1. The supervisor is `PTRACE_SEIZE`'d to the tracee with
//!    `PTRACE_O_TRACESYSCALL` (and ideally
//!    `PTRACE_O_TRACESYSGOOD` for stop-kind discrimination).
//! 2. After the seccomp listener loop FLAG_CONTINUEs a
//!    notification, the kernel runs the syscall natively. Once
//!    it completes, `PTRACE_O_TRACESYSCALL` raises a
//!    syscall-exit-stop. The supervisor `waitpid`s for it.
//! 3. The supervisor reads the user-mode registers via
//!    `PTRACE_GETREGS`; on x86-64, the syscall return value is
//!    in `RAX` (sign-extended into 64 bits — kernel returns
//!    negative `-errno` on failure).
//! 4. With the result in hand, the supervisor re-runs the
//!    capture primitive in *post-syscall* mode (curated
//!    `OutBuf` bytes referenced by the return value, catch-all
//!    windows around any pointer-shaped arg).
//! 5. The pre + post observations merge into one
//!    [`CapturedSyscall`]; the trace writer emits one
//!    `Event::Syscall` for the pair.
//!
//! ## What this commit lands
//!
//! - [`UserRegsX86_64`] — Rust mirror of Linux's
//!   `struct user_regs_struct` for x86-64. Layout-asserted in
//!   tests so a libc/kernel drift doesn't desync the field
//!   offsets.
//! - [`get_regs`] / [`set_regs`] — `PTRACE_GETREGS` /
//!   `PTRACE_SETREGS` wrappers (replay needs SETREGS to write
//!   the recorded result back into RAX).
//! - [`result_register_x86_64`] — sign-extending RAX read.
//! - [`wait_for_syscall_exit`] — `waitpid` loop that filters
//!   for the syscall-exit-stop discriminator
//!   (`status == ((SIGTRAP | 0x80) << 8) | 0x7f` per
//!   `PTRACE_O_TRACESYSGOOD`).
//! - [`merge_pre_post`] — fuses the entry + exit captures into
//!   one [`CapturedSyscall`], stamping the real result and
//!   appending OutBuf regions.
//! - [`record_syscall_with_exit`] — supervisor turn that wires
//!   recv_notif + capture_pre + respond_continue +
//!   wait_for_syscall_exit + capture_post + merge + write_event.
//!
//! Linux-only; the Darwin equivalent uses Mach exception ports
//! and lives in a parallel module.

#![cfg(target_os = "linux")]

use std::io;
use std::mem;
use std::os::fd::BorrowedFd;

use crate::format::event::Event;
use crate::format::trace_writer::{TraceWriteError, TraceWriter};
use crate::record::linux::ptrace_driver::{
    capture_from_notif, event_for_capture, frame_from_notif, recv_notif, respond_continue,
    RecorderError, SeccompNotif,
};
use crate::record::syscall_capture::{
    capture_post_syscall, CapturedSyscall, MemoryReader,
};

// ---------------------------------------------------------------------------
// User-mode register layout
// ---------------------------------------------------------------------------

/// Linux x86-64 `struct user_regs_struct`. From
/// `<sys/user.h>` — 27 u64 fields, 216 bytes total. Layout is
/// stable across glibc versions; we reproduce it here so the
/// recorder doesn't have to depend on libc's `user_regs_struct`
/// (which isn't on every libc target).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // self-evident, mirrors kernel ABI
pub struct UserRegsX86_64 {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub orig_rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub eflags: u64,
    pub rsp: u64,
    pub ss: u64,
    pub fs_base: u64,
    pub gs_base: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
}

/// Sign-extending RAX read. The kernel's syscall ABI returns a
/// `long` — `-errno` on failure (`-1..=-MAX_ERRNO`), or the
/// success value (which can be negative for `lseek`-style
/// syscalls, but those use the full sign-extended low 64 bits).
pub fn result_register_x86_64(regs: &UserRegsX86_64) -> i64 {
    regs.rax as i64
}

/// `PTRACE_GETREGS` wrapper.
pub fn get_regs(pid: i32) -> io::Result<UserRegsX86_64> {
    let mut regs: UserRegsX86_64 = unsafe { mem::zeroed() };
    // SAFETY: PTRACE_GETREGS writes through &mut regs; the
    // local has the right ABI layout (asserted in tests).
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            &mut regs as *mut _ as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(regs)
}

/// `PTRACE_SETREGS` wrapper. The replay shim calls this with a
/// modified regs struct (typically rax overwritten with the
/// recorded result) before stepping the tracee past the
/// syscall-exit-stop.
pub fn set_regs(pid: i32, regs: &UserRegsX86_64) -> io::Result<()> {
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            regs as *const _ as *const libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wait helpers
// ---------------------------------------------------------------------------

/// Why `waitpid` returned. The recorder distinguishes a real
/// syscall-exit-stop from spurious wakes (signal-delivery,
/// group-stop, exit, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// `PTRACE_O_TRACESYSGOOD`-tagged syscall stop.
    SyscallStop,
    /// A non-syscall ptrace event (forks, exec, …).
    PtraceEvent {
        /// Top byte of `wstatus >> 8 >> 8` — the event id.
        event: i32,
    },
    /// Vanilla signal delivery.
    SignalDelivery {
        /// The signal number being delivered.
        sig: i32,
    },
    /// Tracee exited normally.
    Exited {
        /// The exit code.
        code: i32,
    },
    /// Tracee was killed by a signal.
    Signalled {
        /// The signal number that killed the tracee.
        sig: i32,
    },
}

/// Decode a `wstatus` returned by `waitpid` for a ptrace'd
/// child. The recorder uses the result to pick the next ptrace
/// command; replay too.
pub fn classify_wstatus(status: libc::c_int) -> StopKind {
    if libc::WIFEXITED(status) {
        return StopKind::Exited { code: libc::WEXITSTATUS(status) };
    }
    if libc::WIFSIGNALED(status) {
        return StopKind::Signalled { sig: libc::WTERMSIG(status) };
    }
    if libc::WIFSTOPPED(status) {
        let sig = libc::WSTOPSIG(status);
        // PTRACE_O_TRACESYSGOOD adds the high bit (0x80) to
        // `SIGTRAP` for syscall-stops. Without that option,
        // syscall-stops are indistinguishable from a regular
        // SIGTRAP — caller must enable the option.
        if sig == (libc::SIGTRAP | 0x80) {
            return StopKind::SyscallStop;
        }
        // PTRACE_EVENT_* show up in the upper byte.
        let event = (status >> 16) & 0xffff;
        if event != 0 {
            return StopKind::PtraceEvent { event };
        }
        return StopKind::SignalDelivery { sig };
    }
    // The kernel's wstatus space isn't exhaustive; "unknown"
    // is unreachable in practice but a defensive default.
    StopKind::SignalDelivery { sig: 0 }
}

/// Wait for the next syscall-exit-stop on `pid`. Other event
/// kinds (signal-delivery, ptrace events) are returned
/// verbatim so the caller can route them — the recorder routes
/// signal-delivery events into [`Event::Signal`] via the
/// signals module.
pub fn wait_for_next_stop(pid: i32) -> io::Result<(StopKind, libc::c_int)> {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid writes through &mut status; pid is a real
    // tracee.
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((classify_wstatus(status), status))
}

/// `PTRACE_SYSCALL`. Continue the tracee until the next
/// syscall-entry-or-exit-stop. `sig` is the signal to deliver
/// (or 0 for none).
pub fn ptrace_syscall(pid: i32, sig: i32) -> io::Result<()> {
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SYSCALL,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            sig as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `PTRACE_CONT`. Continue, but skip past the next syscall-
/// entry-stop (used after a `PTRACE_O_TRACESYSCALL` exit-stop
/// when the recorder doesn't need to see the next entry).
pub fn ptrace_cont(pid: i32, sig: i32) -> io::Result<()> {
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_CONT,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            sig as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre+post merge
// ---------------------------------------------------------------------------

/// Fuse a pre-syscall capture with a post-syscall one into a
/// single observation. The pre half supplies args + InBuf +
/// InCStr regions; the post half supplies the result + OutBuf
/// + catch-all post-state regions. Plan §3B contract: one
/// `Event::Syscall` per syscall, carrying both halves.
pub fn merge_pre_post(
    pre: &CapturedSyscall,
    post: &CapturedSyscall,
) -> CapturedSyscall {
    // Sanity — pre and post must agree on nr + args; if they
    // diverge the supervisor is processing the wrong stop.
    debug_assert_eq!(pre.nr, post.nr, "pre/post nr divergence");
    debug_assert_eq!(pre.args, post.args, "pre/post arg divergence");
    let mut regions = pre.regions.clone();
    regions.extend(post.regions.iter().cloned());
    CapturedSyscall {
        nr: pre.nr,
        args: pre.args,
        result: post.result,
        regions,
        tier: pre.tier, // tier is the same on both halves
    }
}

// ---------------------------------------------------------------------------
// End-to-end supervisor turn
// ---------------------------------------------------------------------------

/// Drive one full record turn against an attached tracee:
///
/// 1. `recv_notif` — pull entry-side notification.
/// 2. `capture_pre_syscall` via `reader`.
/// 3. `respond_continue` — kernel runs the syscall.
/// 4. `wait_for_next_stop` — expect SyscallStop (exit).
/// 5. `get_regs(pid)` — read RAX into `result`.
/// 6. `capture_post_syscall` — read OutBuf bytes the kernel
///    just wrote.
/// 7. `merge_pre_post` → one `CapturedSyscall`.
/// 8. `write_event` → one `Event::Syscall` on disk.
///
/// The function expects the tracee to already have been
/// `PTRACE_SEIZE`'d with `PTRACE_O_TRACESYSCALL` +
/// `PTRACE_O_TRACESYSGOOD` set (typically by `RecordChild::spawn`).
///
/// Returns the merged capture so the caller can log/inspect.
pub fn record_syscall_with_exit(
    tracee_pid: i32,
    listener: BorrowedFd<'_>,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
) -> Result<CapturedSyscall, ExitStopError> {
    let notif = recv_notif(listener).map_err(|e| {
        ExitStopError::Recorder(RecorderError::Recv(e))
    })?;
    let pre = capture_from_notif_raw(&notif, reader);
    respond_continue(listener, notif.id).map_err(|e| {
        ExitStopError::Recorder(RecorderError::Respond(e))
    })?;
    let (kind, status) = wait_for_next_stop(tracee_pid).map_err(ExitStopError::Wait)?;
    if kind != StopKind::SyscallStop {
        return Err(ExitStopError::UnexpectedStop {
            wstatus: status,
            kind,
        });
    }
    let regs = get_regs(tracee_pid).map_err(ExitStopError::GetRegs)?;
    let result = result_register_x86_64(&regs);
    let frame = frame_from_notif(&notif);
    let post = capture_post_syscall(frame, result, reader);
    let merged = merge_pre_post(&pre, &post);
    writer
        .write_event(event_for_capture(&merged))
        .map_err(ExitStopError::Write)?;
    Ok(merged)
}

/// Pre-side capture without the result-sentinel stamp. The
/// public [`capture_from_notif`] sets `result =
/// RESULT_NOT_CAPTURED_YET`; here we want the bare pre half
/// because [`merge_pre_post`] re-stamps the real result.
fn capture_from_notif_raw(
    notif: &SeccompNotif,
    reader: &dyn MemoryReader,
) -> CapturedSyscall {
    // Re-use the public path but immediately overwrite the
    // sentinel. We don't add a new public function because the
    // sentinel is exactly the contract we want for the
    // step-7-only callers; the merge case is internal.
    let mut pre = capture_from_notif(notif, reader);
    pre.result = 0;
    pre
}

/// Errors arising from [`record_syscall_with_exit`].
#[derive(thiserror::Error, Debug)]
pub enum ExitStopError {
    /// One of the seccomp ioctls failed.
    #[error("recorder: {0}")]
    Recorder(RecorderError),
    /// `waitpid` for the syscall-exit-stop failed.
    #[error("waitpid: {0}")]
    Wait(io::Error),
    /// `waitpid` returned, but the stop wasn't a syscall-exit-
    /// stop. Typically a signal delivery; the caller routes
    /// it (e.g. records an `Event::Signal` and re-runs).
    #[error("expected syscall-exit-stop, got {kind:?} (wstatus={wstatus:#x})")]
    UnexpectedStop {
        /// Raw wstatus from waitpid.
        wstatus: libc::c_int,
        /// Decoded stop kind.
        kind: StopKind,
    },
    /// `PTRACE_GETREGS` failed.
    #[error("PTRACE_GETREGS: {0}")]
    GetRegs(io::Error),
    /// Trace writer failed.
    #[error("trace write: {0}")]
    Write(TraceWriteError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::linux::ptrace_driver::SeccompData;
    use crate::record::syscall_capture::{
        CallFrame, CaptureTier, CapturedKind, CapturedRegion,
    };

    #[test]
    fn user_regs_layout_is_27_u64_fields() {
        // sizeof(struct user_regs_struct) on Linux x86-64 = 216
        // = 27 * 8. Anyone reordering fields hits this.
        assert_eq!(std::mem::size_of::<UserRegsX86_64>(), 27 * 8);
        // Field-offset spot-check: RAX at offset 80 (10 fields
        // before: r15, r14, r13, r12, rbp, rbx, r11, r10, r9, r8).
        let z: UserRegsX86_64 = unsafe { mem::zeroed() };
        let base = &z as *const _ as usize;
        let rax_off = &z.rax as *const _ as usize - base;
        assert_eq!(rax_off, 80, "rax expected at offset 80");
        let rip_off = &z.rip as *const _ as usize - base;
        // 16 fields before rip (… orig_rax is 15th).
        assert_eq!(rip_off, 16 * 8, "rip expected at offset {}", 16 * 8);
    }

    #[test]
    fn classify_wstatus_recognises_syscall_stop() {
        // PTRACE_O_TRACESYSGOOD-tagged syscall-stop wstatus:
        //   WIFSTOPPED + (sig = SIGTRAP | 0x80)
        // wstatus is `(stopsig << 8) | 0x7f`.
        let stopsig = libc::SIGTRAP | 0x80;
        let wstatus = (stopsig << 8) | 0x7f;
        assert_eq!(classify_wstatus(wstatus), StopKind::SyscallStop);
    }

    #[test]
    fn classify_wstatus_recognises_normal_signal() {
        // SIGINT delivered to the tracee:
        let wstatus = (libc::SIGINT << 8) | 0x7f;
        assert_eq!(
            classify_wstatus(wstatus),
            StopKind::SignalDelivery { sig: libc::SIGINT },
        );
    }

    #[test]
    fn classify_wstatus_recognises_normal_exit() {
        // exited normally with code 42 → wstatus = 42 << 8.
        let wstatus = 42 << 8;
        assert_eq!(classify_wstatus(wstatus), StopKind::Exited { code: 42 });
    }

    #[test]
    fn classify_wstatus_recognises_killed_by_signal() {
        // Signalled: low byte = sig (no 0x7f). Encode SIGKILL.
        let wstatus = libc::SIGKILL;
        assert_eq!(
            classify_wstatus(wstatus),
            StopKind::Signalled { sig: libc::SIGKILL },
        );
    }

    #[test]
    fn classify_wstatus_recognises_ptrace_event() {
        // PTRACE_EVENT_EXEC = 4. Encoding:
        //   (event << 16) | ((SIGTRAP | event-marker) << 8) | 0x7f
        let event = 4i32;
        let wstatus = (event << 16) | (libc::SIGTRAP << 8) | 0x7f;
        assert_eq!(
            classify_wstatus(wstatus),
            StopKind::PtraceEvent { event },
        );
    }

    #[test]
    fn result_register_sign_extends_negative_errno() {
        let mut regs = UserRegsX86_64::default();
        // -ENOENT on x86-64 is (u64::MAX - 1) = 0xFFFF_FFFF_FFFF_FFFE
        regs.rax = (-2i64) as u64;
        assert_eq!(result_register_x86_64(&regs), -2);
    }

    #[test]
    fn result_register_passes_through_positive() {
        let mut regs = UserRegsX86_64::default();
        regs.rax = 1234;
        assert_eq!(result_register_x86_64(&regs), 1234);
    }

    #[test]
    fn merge_pre_post_carries_pre_inbuf_then_post_outbuf() {
        let pre = CapturedSyscall {
            nr: 1,
            args: [2, 0xCAFE_BA00, 5, 0, 0, 0],
            result: 0,
            regions: vec![CapturedRegion {
                arg_idx: 1,
                addr: 0xCAFE_BA00,
                bytes: b"hello".to_vec(),
                requested_len: 5,
                kind: CapturedKind::InBuf,
            }],
            tier: CaptureTier::Curated,
        };
        let post = CapturedSyscall {
            nr: 1,
            args: [2, 0xCAFE_BA00, 5, 0, 0, 0],
            result: 5,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let merged = merge_pre_post(&pre, &post);
        assert_eq!(merged.result, 5);
        assert_eq!(merged.regions.len(), 1);
        assert_eq!(merged.regions[0].kind, CapturedKind::InBuf);
    }

    #[test]
    fn merge_pre_post_appends_post_outbufs() {
        let pre = CapturedSyscall {
            nr: 0,
            args: [3, 0xDEAD_BEEF_00, 4096, 0, 0, 0],
            result: 0,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let post = CapturedSyscall {
            nr: 0,
            args: [3, 0xDEAD_BEEF_00, 4096, 0, 0, 0],
            result: 11,
            regions: vec![CapturedRegion {
                arg_idx: 1,
                addr: 0xDEAD_BEEF_00,
                bytes: b"hello world".to_vec(),
                requested_len: 11,
                kind: CapturedKind::OutBuf,
            }],
            tier: CaptureTier::Curated,
        };
        let merged = merge_pre_post(&pre, &post);
        assert_eq!(merged.result, 11);
        assert_eq!(merged.regions.len(), 1);
        assert_eq!(merged.regions[0].kind, CapturedKind::OutBuf);
        assert_eq!(merged.regions[0].bytes, b"hello world");
    }

    #[test]
    #[should_panic(expected = "pre/post nr divergence")]
    fn merge_pre_post_panics_on_nr_mismatch() {
        // Hits a debug_assert. In release we'd quietly produce
        // a confused capture; the assert catches it early.
        let pre = CapturedSyscall {
            nr: 0,
            args: [0; 6],
            result: 0,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let post = CapturedSyscall {
            nr: 1,
            args: [0; 6],
            result: 0,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let _ = merge_pre_post(&pre, &post);
    }

    #[test]
    fn frame_extracted_from_synthetic_notif() {
        let n = SeccompNotif {
            id: 1,
            pid: 100,
            flags: 0,
            data: SeccompData {
                nr: 1,
                arch: 0xC000_003E,
                instruction_pointer: 0,
                args: [2, 0xCAFE_BA00, 5, 0, 0, 0],
            },
        };
        let f: CallFrame = frame_from_notif(&n);
        assert_eq!(f.nr, 1);
        assert_eq!(f.args[0], 2);
    }
}

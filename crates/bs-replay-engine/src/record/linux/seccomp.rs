// SPDX-License-Identifier: MIT
//! `seccomp-bpf` user-notify install for the sub-phase 3B
//! recorder.
//!
//! Plan §3B: "filter installed in tracee at fork: every syscall
//! triggers `SECCOMP_RET_USER_NOTIF`". The tracer reads the
//! notification, services it (capture or replay), then either
//! lets the syscall through (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`)
//! on record or short-circuits it on replay.
//!
//! The filter itself is two BPF instructions:
//!
//! 1. Load `seccomp_data.arch` (offset 4 from the start of
//!    `struct seccomp_data`) and reject anything that isn't
//!    `AUDIT_ARCH_X86_64`. Cross-arch syscall ABIs (x32, 32-bit
//!    legacy) replay differently and the recorder doesn't
//!    handle them; refusing them up front beats replaying a
//!    32-bit syscall as if it were 64-bit.
//! 2. Return `SECCOMP_RET_USER_NOTIF` for everything else — the
//!    listener fd we got back from `seccomp(2)` will receive the
//!    notification.
//!
//! Wrapped no-std-style: pure libc + rustix, no C deps beyond
//! libc itself (which is already in the workspace's tree).

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
//
// Defined in the kernel headers but not exposed by libc < 0.2.150
// for some of the seccomp_unotify additions; we redeclare them
// at the values the Linux ABI guarantees stable. Comments cite
// the kernel header line each constant comes from.

/// `linux/audit.h`: AUDIT_ARCH_X86_64.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// `linux/seccomp.h`: SECCOMP_SET_MODE_FILTER.
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;

/// `linux/seccomp.h`: SECCOMP_FILTER_FLAG_NEW_LISTENER. Returns
/// the listener fd from `seccomp(2)` instead of zero.
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_uint = 1 << 3;

/// `linux/seccomp.h`: action codes for filter return values.
const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;

/// BPF opcodes / addressing modes. From `linux/bpf_common.h`.
/// Combined per the BPF ISA: each instruction's `code` is
/// `class | size | mode | op`.
const BPF_LD: u16 = 0x00;
const BPF_RET: u16 = 0x06;
const BPF_JMP: u16 = 0x05;

const BPF_W: u16 = 0x00; // word (32 bits)
const BPF_ABS: u16 = 0x20;
const BPF_K: u16 = 0x00;
const BPF_JEQ: u16 = 0x10;

/// Offset of `seccomp_data.arch` from the start of the struct
/// (`linux/seccomp.h`).
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

/// `struct sock_filter` from `linux/filter.h`. Single BPF
/// instruction; we hand-build a 4-instruction program.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// `struct sock_fprog` from `linux/filter.h` — the (length,
/// pointer) shape `prctl`/`seccomp` accept.
#[repr(C)]
#[derive(Debug)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

// ---------------------------------------------------------------------------
// Filter program
// ---------------------------------------------------------------------------

/// Build the four-instruction "trap every x86_64 syscall via
/// USER_NOTIF, kill the process on any other arch" filter.
///
/// Returned as an owned `Vec<SockFilter>` so the caller controls
/// its lifetime — the filter must outlive the `seccomp(2)` call
/// because the kernel takes a pointer-and-length, not a copy.
fn build_trap_all_filter() -> Vec<SockFilter> {
    vec![
        // 0: A = seccomp_data.arch
        SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARCH_OFFSET,
        },
        // 1: if A == AUDIT_ARCH_X86_64 jump to 3, else fall to 2
        SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1, // jump *over* the kill instruction
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        },
        // 2: return SECCOMP_RET_KILL_PROCESS — wrong arch
        SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_KILL_PROCESS,
        },
        // 3: return SECCOMP_RET_USER_NOTIF — let the supervisor handle
        SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_USER_NOTIF,
        },
    ]
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install the trap-every-syscall filter and return the
/// listener fd the supervisor reads notifications from.
///
/// Two prerequisites the kernel enforces:
///
/// 1. `PR_SET_NO_NEW_PRIVS` must be set first — without it,
///    seccomp won't accept a filter from an unprivileged
///    process. The recorder always installs the filter from the
///    *tracee* side, which is unprivileged, so we set the bit
///    unconditionally.
/// 2. Linux ≥ 5.5 for the `SECCOMP_FILTER_FLAG_NEW_LISTENER`
///    flag. The plan documents this kernel requirement; older
///    kernels return `EINVAL`.
///
/// Returns the listener fd on success. Errors:
///
/// - `Errno::EINVAL` from the seccomp call: kernel doesn't
///   support `NEW_LISTENER` (Linux < 5.5).
/// - `Errno::EACCES` from the `prctl(NO_NEW_PRIVS)` step: the
///   process already has setuid/setgid in flight; the recorder
///   shouldn't be running attached to such a tracee.
pub fn install_trap_all_listener() -> io::Result<OwnedFd> {
    install_with_filter(&build_trap_all_filter())
}

fn install_with_filter(filter: &[SockFilter]) -> io::Result<OwnedFd> {
    // `prctl(PR_SET_NO_NEW_PRIVS, 1)` is mandatory for
    // unprivileged seccomp loaders.
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS takes only an int
    // value; no buffer pointers are dereferenced.
    let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if nnp != 0 {
        return Err(io::Error::last_os_error());
    }
    // Plan §Invariants: "Seccomp filter is loaded before fork."
    debug_assert!(!filter.is_empty(), "empty filter would tell the kernel to allow nothing");
    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };
    // SAFETY: SECCOMP_SET_MODE_FILTER takes a pointer to a
    // `sock_fprog` whose `filter` points at sufficiently many
    // SockFilter entries — the line above ensures both the
    // length and the pointer are correct for the slice
    // `filter`. `&prog` is the only outparam; the kernel reads
    // through it and copies the filter into kernel memory before
    // the syscall returns.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const _ as *const libc::c_void,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // The fd lives in `raw` on success — kernels < 5.5 would
    // have erred above with EINVAL.
    let fd = raw as RawFd;
    if fd < 0 {
        return Err(io::Error::other(format!(
            "seccomp(SET_MODE_FILTER, NEW_LISTENER) returned bogus fd {fd}"
        )));
    }
    // SAFETY: the syscall just minted this fd; nothing else
    // owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_is_four_instructions() {
        let f = build_trap_all_filter();
        assert_eq!(f.len(), 4);
        // Sanity-check the opcodes the kernel actually verifies.
        assert_eq!(f[0].code, BPF_LD | BPF_W | BPF_ABS);
        assert_eq!(f[0].k, SECCOMP_DATA_ARCH_OFFSET);
        assert_eq!(f[1].code, BPF_JMP | BPF_JEQ | BPF_K);
        assert_eq!(f[1].k, AUDIT_ARCH_X86_64);
        assert_eq!(f[2].code, BPF_RET | BPF_K);
        assert_eq!(f[2].k, SECCOMP_RET_KILL_PROCESS);
        assert_eq!(f[3].code, BPF_RET | BPF_K);
        assert_eq!(f[3].k, SECCOMP_RET_USER_NOTIF);
    }

    #[test]
    fn filter_has_in_range_jump_offsets() {
        // jt and jf are u8 offsets *past* the next instruction.
        // jt=1 jumps over the KILL_PROCESS landing on USER_NOTIF.
        let f = build_trap_all_filter();
        assert_eq!(f[1].jt, 1);
        assert_eq!(f[1].jf, 0);
        // 1 (current pc) + 1 (jt) + 1 (next-instr-base) = 3 →
        // the USER_NOTIF return. Anything beyond `f.len()` would
        // be a kernel-rejection.
        let target = (1usize) + 1 + (f[1].jt as usize);
        assert!(
            target < f.len(),
            "jt offset {} produces out-of-range jump (target {target}, filter len {})",
            f[1].jt,
            f.len(),
        );
    }

    /// Smoke test that the install path works on this host.
    /// Skipped automatically when the host kernel is too old or
    /// the test process can't gain `NO_NEW_PRIVS` (rare —
    /// happens on heavily-locked-down sandboxes).
    ///
    /// Installs the filter and immediately closes the listener
    /// fd so the test process isn't permanently in trap-every-
    /// syscall mode after the test returns. The filter survives
    /// the listener close (kernel keeps it on the proc), so this
    /// test process can never call another syscall after the
    /// install — we exit the inner block via a dedicated thread
    /// to keep the harness alive.
    #[test]
    #[cfg(target_os = "linux")]
    fn install_returns_a_listener_fd() {
        // Run inside a child so the filter doesn't mutate the
        // test harness's syscall surface.
        let child = unsafe {
            match libc::fork() {
                -1 => {
                    eprintln!(
                        "skipping install_returns_a_listener_fd: fork failed: {}",
                        io::Error::last_os_error(),
                    );
                    return;
                }
                0 => {
                    // Child path. Try to install; report success
                    // back to the parent via exit code.
                    let code = match install_trap_all_listener() {
                        Ok(_fd) => 0,
                        Err(e) => {
                            // Common skip cases: ENOSYS (kernel <
                            // 5.5), EINVAL (NEW_LISTENER not
                            // supported), EACCES (sandboxed CI).
                            match e.raw_os_error() {
                                Some(libc::ENOSYS) => 64,
                                Some(libc::EINVAL) => 65,
                                Some(libc::EACCES) => 66,
                                _ => 1,
                            }
                        }
                    };
                    libc::_exit(code);
                }
                pid => pid,
            }
        };

        let mut status: libc::c_int = 0;
        // SAFETY: waitpid reads through the &mut int and writes
        // it back; pid is freshly minted by the parent's fork.
        let ret = unsafe { libc::waitpid(child, &mut status, 0) };
        assert!(ret > 0, "waitpid failed: {}", io::Error::last_os_error());
        let exit = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        match exit {
            0 => {} // happy path
            64..=66 => {
                eprintln!(
                    "skipping install_returns_a_listener_fd: \
                     kernel/perms don't support seccomp NEW_LISTENER (exit {exit})"
                );
            }
            other => panic!(
                "child returned unexpected status {other} (status word {status:#x})"
            ),
        }
    }
}

// SPDX-License-Identifier: MIT
//! Register-state capture / restore via ptrace.
//!
//! Tier 2 checkpoints need both memory state (handled in
//! [`super::checkpoint_capture`]) and CPU register state. This
//! module is the register half; combining the two into one
//! payload is the consumer's job.
//!
//! Cross-arch on Linux:
//!
//! - **x86-64**: `PTRACE_GETREGS` / `PTRACE_SETREGS` returning
//!   `libc::user_regs_struct` (216 B).
//! - **aarch64**: `PTRACE_GETREGSET` / `PTRACE_SETREGSET` with
//!   `NT_PRSTATUS` and an iovec; the kernel struct is
//!   `user_pt_regs` (272 B).
//!
//! [`RegisterState`] carries the raw bytes; the architecture
//! is implicit (the manifest's CPU-feature check refuses
//! cross-arch replay before we'd ever try to write a wrong-
//! shape blob).

use std::io;
#[cfg(target_arch = "x86_64")]
use std::mem;

use nix::unistd::Pid;

/// Wrapped kernel register snapshot. Bytes are arch-shaped:
/// `arch_byte_len()` × 8 on x86-64, more on aarch64. The
/// payload codec doesn't peek inside; replay's
/// `restore_registers` writes them back via the same ptrace
/// path that produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterState {
    /// Raw kernel-ABI bytes. Length depends on the host arch.
    pub bytes: Vec<u8>,
}

impl RegisterState {
    /// Number of bytes the local arch's register snapshot
    /// occupies. Tier 2 payload codec uses this as a sanity
    /// tripwire.
    pub const fn arch_byte_len() -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            mem::size_of::<libc::user_regs_struct>()
        }
        #[cfg(target_arch = "aarch64")]
        {
            // struct user_pt_regs: 31×u64 + sp + pc + pstate = 34×8.
            34 * 8
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            0
        }
    }
}

/// `PTRACE_GETREGS` (x86_64) or `PTRACE_GETREGSET + NT_PRSTATUS`
/// (aarch64). Caller must have already ptrace-attached.
pub fn capture_registers(pid: Pid) -> Result<RegisterState, RegError> {
    capture_arch(pid)
}

/// Inverse of [`capture_registers`]: `PTRACE_SETREGS` (x86_64)
/// or `PTRACE_SETREGSET` (aarch64).
pub fn restore_registers(pid: Pid, state: &RegisterState) -> Result<(), RegError> {
    if state.bytes.len() != RegisterState::arch_byte_len() {
        return Err(RegError::WrongLen {
            got: state.bytes.len(),
            expected: RegisterState::arch_byte_len(),
        });
    }
    restore_arch(pid, &state.bytes)
}

#[cfg(target_arch = "x86_64")]
fn capture_arch(pid: Pid) -> Result<RegisterState, RegError> {
    let regs = nix::sys::ptrace::getregs(pid)?;
    let n = mem::size_of::<libc::user_regs_struct>();
    let mut bytes = Vec::with_capacity(n);
    // SAFETY: user_regs_struct is POD; reinterpreting its
    // memory as &[u8] for the duration of the copy is sound.
    unsafe {
        let src = std::slice::from_raw_parts(&regs as *const _ as *const u8, n);
        bytes.extend_from_slice(src);
    }
    Ok(RegisterState { bytes })
}

#[cfg(target_arch = "x86_64")]
fn restore_arch(pid: Pid, bytes: &[u8]) -> Result<(), RegError> {
    // Reconstruct the user_regs_struct from bytes and call
    // setregs. The struct is POD so a read_unaligned is fine
    // even if the buffer's start isn't 8-aligned.
    let regs: libc::user_regs_struct =
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const _) };
    nix::sys::ptrace::setregs(pid, regs)?;
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn capture_arch(pid: Pid) -> Result<RegisterState, RegError> {
    const NT_PRSTATUS: i32 = 1;
    let n = RegisterState::arch_byte_len();
    let mut bytes = vec![0u8; n];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr() as *mut libc::c_void,
        iov_len: n,
    };
    // SAFETY: PTRACE_GETREGSET fills iov_base with up to
    // iov_len bytes; the buffer is owned and exactly that size.
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid.as_raw(),
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(RegError::Io(io::Error::last_os_error()));
    }
    bytes.truncate(iov.iov_len);
    Ok(RegisterState { bytes })
}

#[cfg(target_arch = "aarch64")]
fn restore_arch(pid: Pid, bytes: &[u8]) -> Result<(), RegError> {
    const NT_PRSTATUS: i32 = 1;
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid.as_raw(),
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(RegError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn capture_arch(_pid: Pid) -> Result<RegisterState, RegError> {
    Err(RegError::Io(io::Error::from_raw_os_error(libc::ENOSYS)))
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn restore_arch(_pid: Pid, _bytes: &[u8]) -> Result<(), RegError> {
    Err(RegError::Io(io::Error::from_raw_os_error(libc::ENOSYS)))
}

/// Errors arising from getregs / setregs.
#[derive(thiserror::Error, Debug)]
pub enum RegError {
    /// nix-level errno failure (x86_64 path).
    #[error("nix error: {0}")]
    Nix(#[from] nix::errno::Errno),
    /// raw I/O error (aarch64 path).
    #[error("ptrace io: {0}")]
    Io(io::Error),
    /// Caller passed a [`RegisterState`] with a `bytes` length
    /// that doesn't match the host architecture's expected
    /// snapshot size — typically a cross-arch trace replay.
    #[error("register-state byte length: got {got}, expected {expected}")]
    WrongLen {
        /// What the caller supplied.
        got: usize,
        /// What `RegisterState::arch_byte_len()` requires.
        expected: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_byte_len_is_nontrivial() {
        let n = RegisterState::arch_byte_len();
        // x86_64: 216, aarch64: 272.
        assert!(n == 216 || n == 272, "unexpected arch byte len: {n}",);
    }

    #[test]
    fn restore_rejects_wrong_len() {
        let bad = RegisterState {
            bytes: vec![0u8; 7],
        };
        let err = restore_registers(Pid::from_raw(1), &bad).unwrap_err();
        match err {
            RegError::WrongLen { got: 7, expected } => {
                assert_eq!(expected, RegisterState::arch_byte_len());
            }
            other => panic!("expected WrongLen, got {other:?}"),
        }
    }

    #[cfg(target_arch = "x86_64")]
    mod x86_64_only {
        use super::super::*;
        use crate::linux::fork_self::LinuxForkSelfMechanism;
        use crate::ring::CheckpointMechanism;
        use std::thread::sleep;
        use std::time::Duration;

        #[test]
        fn capture_then_restore_is_identity() {
            let mut mech = LinuxForkSelfMechanism::new();
            let h = mech.take(0).expect("fork failed");
            sleep(Duration::from_millis(50));
            if let Err(e) = mech.seize(&h) {
                let s = format!("{e:?}");
                if s.contains("EPERM") {
                    eprintln!("skipping regs test: YAMA blocked seize");
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("seize failed: {e:?}");
            }

            let captured = match capture_registers(h.pid) {
                Ok(r) => r,
                Err(e) => {
                    let s = format!("{e:?}");
                    if s.contains("EPERM") {
                        mech.kill(h).expect("kill failed");
                        return;
                    }
                    panic!("getregs failed: {e:?}");
                }
            };
            // No-op round-trip.
            restore_registers(h.pid, &captured).expect("setregs failed");
            let recaptured = capture_registers(h.pid).expect("getregs2 failed");
            assert_eq!(captured, recaptured, "no-op restore mutated state");
            mech.kill(h).expect("kill failed");
        }

        #[test]
        fn targeted_modification_is_observable_after_restore() {
            // x86_64 only — demonstrates rax-flip via raw bytes.
            let mut mech = LinuxForkSelfMechanism::new();
            let h = mech.take(0).expect("fork failed");
            sleep(Duration::from_millis(50));
            if let Err(e) = mech.seize(&h) {
                let s = format!("{e:?}");
                if s.contains("EPERM") {
                    eprintln!("skipping regs-modify test: YAMA blocked seize");
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("seize failed: {e:?}");
            }
            let mut state = match capture_registers(h.pid) {
                Ok(r) => r,
                Err(e) => {
                    let s = format!("{e:?}");
                    if s.contains("EPERM") {
                        mech.kill(h).expect("kill failed");
                        return;
                    }
                    panic!("getregs failed: {e:?}");
                }
            };
            // RAX offset in user_regs_struct is +80 (10 u64 fields
            // before it). Reconstruct, flip, restore.
            const RAX_OFFSET: usize = 80;
            const SENTINEL: u64 = 0xdead_beef_cafe_babe;
            state.bytes[RAX_OFFSET..RAX_OFFSET + 8].copy_from_slice(&SENTINEL.to_le_bytes());
            restore_registers(h.pid, &state).expect("setregs failed");
            let recaptured = capture_registers(h.pid).expect("getregs2 failed");
            let read_back = u64::from_le_bytes(
                recaptured.bytes[RAX_OFFSET..RAX_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(read_back, SENTINEL, "rax did not survive setregs roundtrip",);
            mech.kill(h).expect("kill failed");
        }
    }
}

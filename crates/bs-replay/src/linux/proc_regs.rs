// SPDX-License-Identifier: MIT
//! Register-state capture / restore via `ptrace::getregs` and
//! `ptrace::setregs`.
//!
//! Tier 2 checkpoints need both memory state (handled in
//! [`super::checkpoint_capture`]) and CPU register state. This
//! module is the register half; combining the two into one
//! payload is the consumer's job.
//!
//! Architecture-specific. Linux x86_64 today; aarch64 will land
//! when Phase 5 sub-phase 3G ports the recorder.

use nix::unistd::Pid;

/// Wrapped `libc::user_regs_struct`. Newtype so the public API
/// doesn't leak the libc type and we can pin a `Clone + Copy +
/// Debug` shape independent of libc's bindings.
#[derive(Debug, Clone, Copy)]
pub struct RegisterState {
    /// Raw kernel-ABI register snapshot.
    pub regs: libc::user_regs_struct,
}

/// PTRACE_GETREGS the target. Caller must have already
/// ptrace-attached.
pub fn capture_registers(pid: Pid) -> Result<RegisterState, RegError> {
    let regs = nix::sys::ptrace::getregs(pid)?;
    Ok(RegisterState { regs })
}

/// PTRACE_SETREGS the target with `state`. Caller must have
/// already ptrace-attached.
pub fn restore_registers(pid: Pid, state: &RegisterState) -> Result<(), RegError> {
    nix::sys::ptrace::setregs(pid, state.regs)?;
    Ok(())
}

/// Errors arising from getregs / setregs.
#[derive(thiserror::Error, Debug)]
pub enum RegError {
    /// nix-level errno failure.
    #[error("nix error: {0}")]
    Nix(#[from] nix::errno::Errno),
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // Restore the same state — should be a no-op.
        restore_registers(h.pid, &captured).expect("setregs failed");
        let recaptured = capture_registers(h.pid).expect("getregs2 failed");

        // Compare the raw user_regs_struct field-by-field via a
        // memcmp on the byte representation. The struct is plain
        // POD (no padding aliasing per the kernel ABI).
        let a = bytes_of(&captured.regs);
        let b = bytes_of(&recaptured.regs);
        assert_eq!(a, b, "no-op restore changed the register state");

        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn targeted_modification_is_observable_after_restore() {
        // Capture; flip rax to a sentinel via setregs; recapture
        // and confirm rax matches the sentinel. This proves
        // restore_registers writes through to the kernel's tracee
        // copy.
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
        const SENTINEL: u64 = 0xdead_beef_cafe_babe;
        state.regs.rax = SENTINEL;
        restore_registers(h.pid, &state).expect("setregs failed");
        let recaptured = capture_registers(h.pid).expect("getregs2 failed");
        assert_eq!(
            recaptured.regs.rax, SENTINEL,
            "rax did not survive setregs roundtrip",
        );

        mech.kill(h).expect("kill failed");
    }

    /// Reinterpret the user_regs_struct as a byte slice for a
    /// memcmp-style equality check.
    fn bytes_of(r: &libc::user_regs_struct) -> &[u8] {
        // SAFETY: user_regs_struct is POD (no padding-by-language,
        // no Drop, no interior pointers). Reinterpreting its
        // memory as &[u8] for read-only inspection is sound.
        unsafe {
            std::slice::from_raw_parts(
                r as *const libc::user_regs_struct as *const u8,
                std::mem::size_of::<libc::user_regs_struct>(),
            )
        }
    }
}

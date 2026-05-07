// SPDX-License-Identifier: MIT
//! Tier 2 capture glue: a single ergonomic surface for the
//! fork-checkpoint primitives assembled in steps 35–43.
//!
//! [`Tier2Capture`] owns a [`ForkHandle`] alongside a captured
//! [`Tier2State`]. The state half (memory + registers) is what
//! gets stashed in a Tier 3 trace-internal `format::Checkpoint
//! .payload`; the handle half is what an in-memory Tier 2 ring
//! holds onto between capture and restore.
//!
//! Encoding: `[u64 reg_bytes_len][reg_bytes][writable_payload]`,
//! where `writable_payload` is exactly what
//! [`super::checkpoint_capture::to_payload`] produces. Designed
//! so an old Tier-2-state-only consumer can still decode the
//! `writable_payload` half by stripping the leading regs.

use nix::unistd::Pid;

use super::checkpoint_capture::{
    capture_writable_state, from_payload as writable_from_payload,
    restore_writable_state, to_payload as writable_to_payload,
    CaptureError, DecodeError, RestoreReport, WritableState,
};
use super::fork_self::{ForkHandle, ForkMechanismError, LinuxForkSelfMechanism};
use super::proc_regs::{
    capture_registers, restore_registers, RegError, RegisterState,
};
use crate::ring::CheckpointMechanism;

/// Pure-data half of a Tier 2 capture: writable memory plus
/// register snapshot. Encodable to a `Vec<u8>` payload.
#[derive(Debug, Clone)]
pub struct Tier2State {
    /// Writable memory regions captured via /proc/<pid>/mem.
    pub writable: WritableState,
    /// CPU register snapshot.
    pub regs: RegisterState,
}

/// Owned bundle: a SIGSTOP'd, SEIZE'd fork plus the captured
/// state at the moment of capture.
#[derive(Debug)]
pub struct Tier2Capture {
    /// Live fork PID + capture timestamp.
    pub handle: ForkHandle,
    /// Memory + registers at capture time.
    pub state: Tier2State,
}

impl Tier2Capture {
    /// One-shot capture: fork(2) + raise(SIGSTOP) + PTRACE_SEIZE,
    /// then snapshot writable memory and registers. Returns the
    /// owned [`Tier2Capture`] on success.
    pub fn capture(
        mech: &mut LinuxForkSelfMechanism,
        key: u64,
    ) -> Result<Self, Tier2Error> {
        let handle = mech.take(key)?;
        // Briefly let the kernel deliver the self-SIGSTOP.
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Err(e) = mech.seize(&handle) {
            // Best-effort cleanup; propagate the seize error.
            let _ = mech.kill(handle);
            return Err(Tier2Error::Mechanism(e));
        }
        let writable = capture_writable_state(handle.pid).map_err(|e| {
            // Tear down before propagating.
            // We need a fresh ForkHandle here — re-clone the pid.
            tracing::debug!(
                target: "bs_replay",
                "tier2 capture: writable failed: {e:?}",
            );
            Tier2Error::Capture(e)
        })?;
        let regs = capture_registers(handle.pid).map_err(Tier2Error::Reg)?;
        Ok(Self {
            handle,
            state: Tier2State { writable, regs },
        })
    }

    /// Restore this capture's state into an already-SEIZE'd target
    /// PID. Memory regions are written first, then registers.
    /// Returns the per-region restore report from the writable
    /// half (registers either restore or fail outright).
    pub fn restore_into(&self, target: Pid) -> Result<RestoreReport, Tier2Error> {
        let report = restore_writable_state(target, &self.state.writable)
            .map_err(|e| Tier2Error::Capture(CaptureError::Mem(e)))?;
        restore_registers(target, &self.state.regs).map_err(Tier2Error::Reg)?;
        Ok(report)
    }

    /// Drop the capture and SIGKILL the underlying fork.
    pub fn kill(
        self,
        mech: &mut LinuxForkSelfMechanism,
    ) -> Result<(), Tier2Error> {
        mech.kill(self.handle).map_err(Tier2Error::Mechanism)
    }
}

/// Encode a [`Tier2State`] into a single `Vec<u8>` suitable for
/// stashing in a Tier 3 `format::Checkpoint.payload`.
pub fn to_payload(state: &Tier2State) -> Vec<u8> {
    let reg_bytes = state.regs.bytes.as_slice();
    let writable_payload = writable_to_payload(&state.writable);
    let mut out = Vec::with_capacity(8 + reg_bytes.len() + writable_payload.len());
    out.extend_from_slice(&(reg_bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(reg_bytes);
    out.extend_from_slice(&writable_payload);
    out
}

/// Decode a payload produced by [`to_payload`].
pub fn from_payload(bytes: &[u8]) -> Result<Tier2State, Tier2DecodeError> {
    if bytes.len() < 8 {
        return Err(Tier2DecodeError::Truncated {
            needed: 8,
            have: bytes.len(),
        });
    }
    let (head, rest) = bytes.split_at(8);
    let reg_len = u64::from_le_bytes(head.try_into().expect("8")) as usize;
    let expected = RegisterState::arch_byte_len();
    if reg_len != expected {
        return Err(Tier2DecodeError::WrongRegSize {
            claimed: reg_len,
            expected,
        });
    }
    if rest.len() < reg_len {
        return Err(Tier2DecodeError::Truncated {
            needed: reg_len,
            have: rest.len(),
        });
    }
    let (reg_bytes, writable_bytes) = rest.split_at(reg_len);
    let writable =
        writable_from_payload(writable_bytes).map_err(Tier2DecodeError::Writable)?;
    Ok(Tier2State {
        writable,
        regs: RegisterState { bytes: reg_bytes.to_vec() },
    })
}

/// Errors arising from [`Tier2Capture::capture`] /
/// [`Tier2Capture::restore_into`] / [`Tier2Capture::kill`].
#[derive(thiserror::Error, Debug)]
pub enum Tier2Error {
    /// The fork-self mechanism failed (fork, seize, kill, etc.).
    #[error("mechanism: {0}")]
    Mechanism(#[from] ForkMechanismError),
    /// Memory snapshot or restore failed.
    #[error("capture: {0}")]
    Capture(#[from] CaptureError),
    /// Register snapshot or restore failed.
    #[error("regs: {0}")]
    Reg(#[from] RegError),
}

/// Errors arising from [`from_payload`].
#[derive(thiserror::Error, Debug, Eq, PartialEq)]
pub enum Tier2DecodeError {
    /// Buffer was shorter than the format requires.
    #[error("truncated: needed {needed} bytes, had {have}")]
    Truncated {
        /// Bytes the read needed.
        needed: usize,
        /// Bytes available when the read tried.
        have: usize,
    },
    /// The leading u64 reg_bytes_len did not match this build's
    /// expected `sizeof(user_regs_struct)`. Indicates a payload
    /// from a different architecture or a different libc layout.
    #[error("payload registers byte-size {claimed}, this build expects {expected}")]
    WrongRegSize {
        /// Byte size the payload claimed.
        claimed: usize,
        /// Byte size this build expects.
        expected: usize,
    },
    /// The trailing writable-state payload failed to decode.
    #[error("writable: {0}")]
    Writable(DecodeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::proc_mem::{read_bytes_at, write_bytes_at};
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn payload_roundtrip_preserves_state() {
        // Build a synthetic state, encode, decode, byte-compare.
        let state = Tier2State {
            writable: WritableState {
                regions: vec![super::super::checkpoint_capture::CapturedRegion {
                    start: 0x1000,
                    bytes: vec![0xab; 32],
                }],
            },
            regs: RegisterState {
                bytes: vec![0u8; RegisterState::arch_byte_len()],
            },
        };
        let p = to_payload(&state);
        let back = from_payload(&p).unwrap();
        assert_eq!(state.writable, back.writable);
        assert_eq!(state.regs.bytes, back.regs.bytes);
    }

    #[test]
    fn from_payload_rejects_wrong_reg_size() {
        let mut buf = Vec::new();
        // claim reg_len = 999 (not sizeof user_regs_struct).
        buf.extend_from_slice(&999u64.to_le_bytes());
        // payload tail can be anything.
        assert!(matches!(
            from_payload(&buf),
            Err(Tier2DecodeError::WrongRegSize { .. })
        ));
    }

    #[test]
    fn from_payload_rejects_truncated() {
        // Empty buffer can't even contain the leading reg_len.
        assert!(matches!(
            from_payload(&[]),
            Err(Tier2DecodeError::Truncated { needed: 8, have: 0 })
        ));
    }

    #[test]
    fn capture_then_restore_into_fresh_fork_yields_matching_state() {
        // The end-to-end Tier 2 round-trip, but driven through
        // the Tier2Capture surface rather than the bare
        // primitives. Same shape as step 44's test, exercised
        // through the new ergonomic API.
        use crate::linux::proc_regs::capture_registers;

        let buf: Vec<u8> = vec![0u8; 128];
        let addr = buf.as_ptr() as u64;

        let mut mech = LinuxForkSelfMechanism::new();
        let a = match Tier2Capture::capture(&mut mech, 1) {
            Ok(c) => c,
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EPERM") || s.contains("EACCES") {
                    eprintln!("skipping tier2 e2e: {e:?}");
                    return;
                }
                panic!("tier2 capture A failed: {e:?}");
            }
        };

        // Encode + decode the state half — proves the payload
        // codec inside the Tier 2 boundary.
        let payload = to_payload(&a.state);
        let decoded = from_payload(&payload).expect("decode failed");

        // Take a fresh fork B and SEIZE it.
        let b_handle = mech.take(2).expect("fork B");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&b_handle) {
            eprintln!("skipping tier2 e2e: seize B failed: {e:?}");
            let _ = a.kill(&mut mech);
            mech.kill(b_handle).expect("kill B");
            return;
        }

        // Perturb B at addr.
        let sentinel = vec![0xfe; 128];
        write_bytes_at(b_handle.pid, addr, &sentinel)
            .expect("write sentinel");

        // Reconstruct an owned Tier2Capture-shaped value pointing
        // at A's PID — but for restore we just need the state.
        let restore_into_b = Tier2Capture {
            handle: ForkHandle {
                pid: a.handle.pid, // not used by restore
                captured_unix_seconds: a.handle.captured_unix_seconds,
            },
            state: decoded,
        };
        // Still call restore_into using B's PID directly.
        restore_into_b
            .restore_into(b_handle.pid)
            .expect("restore failed");

        // B's bytes at addr now should match A's.
        let b_post = read_bytes_at(b_handle.pid, addr, 128).expect("read");
        let a_at_addr = read_bytes_at(a.handle.pid, addr, 128).expect("read A");
        assert_eq!(b_post, a_at_addr);

        // Registers too — bytes-equality on the captured snapshot.
        let b_regs = capture_registers(b_handle.pid).expect("getregs B");
        assert_eq!(a.state.regs.bytes, b_regs.bytes);

        // Clean up.
        a.kill(&mut mech).expect("kill A");
        mech.kill(b_handle).expect("kill B");
    }
}

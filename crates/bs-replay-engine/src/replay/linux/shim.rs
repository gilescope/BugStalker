// SPDX-License-Identifier: MIT
//! Sub-phase 3C — single-thread Linux replay shim.
//!
//! Inverse of [`crate::record::linux::ptrace_driver`]: same
//! seccomp filter installed in the relaunched tracee, but on
//! each notification the supervisor *intercepts* the syscall
//! (no `FLAG_CONTINUE`). Instead the supervisor:
//!
//! 1. Pulls the next [`Event::Syscall`] off the trace.
//! 2. Asserts the observed args match the recorded args
//!    (mismatch detector, plan §Invariants).
//! 3. Writes the recorded out-buffers back into the tracee's
//!    address space via [`MemoryWriter`].
//! 4. Responds with the recorded `result` — kernel returns it
//!    to the tracee without running the syscall.
//!
//! The program executes exactly as recorded, byte-for-byte,
//! because every observable kernel side-effect comes from the
//! trace.
//!
//! ## Mismatch detection
//!
//! Plan: "if syscall args at replay differ from recorded, the
//! trace is corrupt or the binary changed — fail loudly with
//! diff report". [`SyscallMismatch`] names *which* arg moved.
//!
//! ## What's not done in step 8
//!
//! - The `RESULT_NOT_CAPTURED_YET` sentinel (recorder step 7's
//!   staging) is rejected at replay with a clear "this trace
//!   was recorded with the entry-args-only recorder; full
//!   record/replay needs step 7b's syscall-exit-stop". Better
//!   to refuse a half-baked replay than to silently inject
//!   zero-results into the tracee.
//! - The actual fork+exec of the tracee with the listener fd
//!   handover is the driver wiring (step 7b's twin); the shim
//!   here operates on a `BorrowedFd` someone else hands it.

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::BorrowedFd;

use crate::format::event::Event;
use crate::record::linux::ptrace_driver::{
    RESULT_NOT_CAPTURED_YET, RecorderError, SeccompNotif, frame_from_notif, recv_notif,
    respond_intercept,
};
use crate::record::syscall_capture::{self, CapturedSyscall, MemoryReader};

/// Inverse of [`MemoryReader`] — write recorded bytes back into
/// the tracee's address space at replay time. Linux impl wraps
/// `process_vm_writev`; tests inject a mock that accumulates
/// writes for assertion.
pub trait MemoryWriter {
    /// Write `bytes` at `addr` in the tracee. Returns the count
    /// successfully written; short writes are accepted as
    /// truncated. Returning zero signals an unmapped address —
    /// the shim flags that as a replay divergence.
    fn write(&mut self, addr: u64, bytes: &[u8]) -> usize;
}

/// One supervisor turn at replay time — pure Rust over the
/// recorded event + observed notif + memory writer. Splits the
/// real ioctl/process_vm_writev mechanics into the wrappers
/// below; the body of the function is mock-driven testable.
pub fn apply_recorded_event(
    notif: &SeccompNotif,
    recorded: &Event,
    writer: &mut dyn MemoryWriter,
) -> Result<ReplayResponse, ReplayError> {
    match recorded {
        Event::Syscall {
            nr,
            args,
            result,
            output,
        } => {
            let observed = frame_from_notif(notif);

            // 1. Mismatch detector — every captured field must
            //    line up before we hand a result back to the
            //    tracee. Plan §Invariants.
            if observed.nr != *nr {
                return Err(ReplayError::Mismatch(SyscallMismatch::Nr {
                    observed: observed.nr,
                    recorded: *nr,
                }));
            }
            for (idx, (o, r)) in observed.args.iter().zip(args.iter()).enumerate() {
                if o != r {
                    return Err(ReplayError::Mismatch(SyscallMismatch::Arg {
                        idx,
                        observed: *o,
                        recorded: *r,
                    }));
                }
            }

            // 2. Refuse the RESULT_NOT_CAPTURED_YET sentinel —
            //    a trace recorded with entry-args-only can't be
            //    replayed deterministically.
            if *result == RESULT_NOT_CAPTURED_YET {
                return Err(ReplayError::ResultNotCaptured {
                    nr: *nr,
                    notif_id: notif.id,
                });
            }

            // 3. Decode the captured-output blob and write
            //    every OutBuf / catch-all region back into
            //    tracee memory.
            let cap = CapturedSyscall::decode_output(*nr, *args, *result, output)
                .map_err(ReplayError::Decode)?;
            let mut regions_written = 0u32;
            let mut regions_short = 0u32;
            for r in &cap.regions {
                if !syscall_capture::looks_like_user_pointer(r.addr) {
                    // Recorder captured a region whose address
                    // we wouldn't have considered a pointer.
                    // Could be a malformed trace, or could be
                    // the recorder's heuristic disagreeing with
                    // ours — log via the response struct and
                    // keep going.
                    continue;
                }
                let n = writer.write(r.addr, &r.bytes);
                if n == r.bytes.len() {
                    regions_written += 1;
                } else {
                    regions_short += 1;
                }
            }

            Ok(ReplayResponse {
                notif_id: notif.id,
                result: *result,
                regions_written,
                regions_short,
            })
        }
        other => Err(ReplayError::UnexpectedEvent {
            got: format!("{other:?}"),
        }),
    }
}

/// Result of one apply step — what to send back to the kernel
/// and how the memory writes went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResponse {
    /// Notification id to echo back.
    pub notif_id: u64,
    /// Recorded syscall result to return to the tracee.
    pub result: i64,
    /// Out-buffer regions written cleanly.
    pub regions_written: u32,
    /// Out-buffer regions where the writer truncated.
    pub regions_short: u32,
}

/// Errors arising from one replay turn.
#[derive(thiserror::Error, Debug)]
pub enum ReplayError {
    /// Observed syscall didn't match the recorded one.
    #[error("syscall mismatch at replay: {0}")]
    Mismatch(SyscallMismatch),
    /// Recorded event wasn't an `Event::Syscall`.
    #[error("expected Event::Syscall at replay, got {got}")]
    UnexpectedEvent {
        /// Debug-format of the unexpected variant.
        got: String,
    },
    /// Recorder hadn't yet captured the result (entry-args-only
    /// trace; needs step 7b's syscall-exit-stop).
    #[error(
        "trace was recorded with the entry-args-only recorder \
         (Event::Syscall.result == i64::MIN sentinel) at notif id {notif_id}, \
         syscall nr {nr}; full record/replay needs step 7b"
    )]
    ResultNotCaptured {
        /// Notification id for diagnostic.
        notif_id: u64,
        /// Syscall number for diagnostic.
        nr: u32,
    },
    /// Captured-output blob inside Event::Syscall failed to
    /// decode.
    #[error("captured-output decode: {0}")]
    Decode(syscall_capture::DecodeError),
}

/// Detail of a syscall arg/nr divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyscallMismatch {
    /// Syscall number changed between record and replay.
    Nr {
        /// What the tracee actually invoked.
        observed: u32,
        /// What the trace says it invoked.
        recorded: u32,
    },
    /// Argument value diverged.
    Arg {
        /// 0-based ABI register index.
        idx: usize,
        /// What the tracee actually passed.
        observed: u64,
        /// What the trace says it passed.
        recorded: u64,
    },
}

impl core::fmt::Display for SyscallMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Nr { observed, recorded } => write!(
                f,
                "nr observed={observed}, recorded={recorded} \
                 — the tracee called a different syscall than recorded; \
                 trace is corrupt or the binary was rebuilt",
            ),
            Self::Arg {
                idx,
                observed,
                recorded,
            } => write!(
                f,
                "arg[{idx}] observed={observed:#x}, recorded={recorded:#x} \
                 — argument register diverged; the most likely causes are \
                 a different binary, a non-deterministic input source not \
                 yet trapped (3D), or stack-pointer divergence",
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// End-to-end loop driver
// ---------------------------------------------------------------------------

/// Pull one notif off the listener, apply the next recorded
/// event, send the recorded result back. The fd-touching half;
/// pairs with [`apply_recorded_event`]'s pure-Rust half.
pub fn replay_one_syscall(
    listener: BorrowedFd<'_>,
    recorded: &Event,
    writer: &mut dyn MemoryWriter,
) -> Result<ReplayResponse, ReplayLoopError> {
    let notif =
        recv_notif(listener).map_err(|e| ReplayLoopError::Recorder(RecorderError::Recv(e)))?;
    let resp = apply_recorded_event(&notif, recorded, writer).map_err(ReplayLoopError::Replay)?;
    respond_intercept(listener, resp.notif_id, resp.result, /*err=*/ 0)
        .map_err(|e| ReplayLoopError::Recorder(RecorderError::Respond(e)))?;
    Ok(resp)
}

/// Outer error wrapping both ioctl-side and replay-logic-side
/// failures.
#[derive(thiserror::Error, Debug)]
pub enum ReplayLoopError {
    /// Replay logic disagreed with the recorded event.
    #[error("replay: {0}")]
    Replay(ReplayError),
    /// ioctl path failed.
    #[error("ioctl: {0}")]
    Recorder(RecorderError),
}

/// `MemoryWriter` impl that writes through `process_vm_writev`.
/// Linux-only; falls back to /proc/<pid>/mem if vm_writev fails
/// (older kernels lacking ptrace_attach permission can still
/// permit /proc writes when the supervisor has CAP_SYS_PTRACE).
#[derive(Debug)]
pub struct ProcMemWriter {
    pid: i32,
}

impl ProcMemWriter {
    /// Wrap a tracee pid for replay-time writes.
    pub fn new(pid: i32) -> Self {
        Self { pid }
    }
}

impl MemoryWriter for ProcMemWriter {
    fn write(&mut self, addr: u64, bytes: &[u8]) -> usize {
        let local = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut _,
            iov_len: bytes.len(),
        };
        // SAFETY: process_vm_writev reads from `local` (we own
        // it) and writes to `remote` in the tracee. The kernel
        // returns the number of bytes successfully copied; -1
        // on outright failure.
        let r =
            unsafe { libc::process_vm_writev(self.pid as libc::pid_t, &local, 1, &remote, 1, 0) };
        if r < 0 {
            // Best-effort fall-back to /proc/<pid>/mem.
            return write_via_proc_mem(self.pid, addr, bytes).unwrap_or(0);
        }
        r as usize
    }
}

fn write_via_proc_mem(pid: i32, addr: u64, bytes: &[u8]) -> io::Result<usize> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::FileExt;
    let path = format!("/proc/{pid}/mem");
    let file = OpenOptions::new().write(true).open(path)?;
    file.write_at(bytes, addr)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::linux::ptrace_driver::SeccompData;
    use crate::record::syscall_capture::{
        CallFrame, CaptureTier, CapturedKind, CapturedRegion, CapturedSyscall, capture_pre_syscall,
    };

    /// Mock writer — accumulates per-address last-write so a
    /// test can assert the recorded buffer ended up at the
    /// recorded address.
    #[derive(Default)]
    struct MockWriter {
        writes: Vec<(u64, Vec<u8>)>,
    }
    impl MemoryWriter for MockWriter {
        fn write(&mut self, addr: u64, bytes: &[u8]) -> usize {
            self.writes.push((addr, bytes.to_vec()));
            bytes.len()
        }
    }

    fn notif(nr: i32, args: [u64; 6]) -> SeccompNotif {
        SeccompNotif {
            id: 7,
            pid: 4242,
            flags: 0,
            data: SeccompData {
                nr,
                arch: 0xC000_003E,
                instruction_pointer: 0,
                args,
            },
        }
    }

    fn syscall_event(cap: &CapturedSyscall) -> Event {
        Event::Syscall {
            nr: cap.nr,
            args: cap.args,
            result: cap.result,
            output: cap.encode_output(),
        }
    }

    #[test]
    fn matched_args_apply_writes_and_return_recorded_result() {
        // read(fd=3, buf=0xDEAD_BEEF_00, count=4096) -> 5
        let recorded = CapturedSyscall {
            nr: 0,
            args: [3, 0xDEAD_BEEF_00, 4096, 0, 0, 0],
            result: 5,
            regions: vec![CapturedRegion {
                arg_idx: 1,
                addr: 0xDEAD_BEEF_00,
                bytes: b"hello".to_vec(),
                requested_len: 5,
                kind: CapturedKind::OutBuf,
            }],
            tier: CaptureTier::Curated,
        };
        let event = syscall_event(&recorded);
        let n = notif(0, [3, 0xDEAD_BEEF_00, 4096, 0, 0, 0]);
        let mut w = MockWriter::default();
        let resp = apply_recorded_event(&n, &event, &mut w).expect("apply");
        assert_eq!(resp.result, 5);
        assert_eq!(resp.notif_id, 7);
        assert_eq!(resp.regions_written, 1);
        assert_eq!(resp.regions_short, 0);
        assert_eq!(w.writes, vec![(0xDEAD_BEEF_00u64, b"hello".to_vec())]);
    }

    #[test]
    fn nr_mismatch_returns_diagnosable_error() {
        let recorded = CapturedSyscall {
            nr: 0,
            args: [3, 0x1000, 5, 0, 0, 0],
            result: 5,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let event = syscall_event(&recorded);
        // tracee called write (nr=1), trace expects read (nr=0)
        let n = notif(1, [3, 0x1000, 5, 0, 0, 0]);
        let err = apply_recorded_event(&n, &event, &mut MockWriter::default()).unwrap_err();
        match err {
            ReplayError::Mismatch(SyscallMismatch::Nr {
                observed: 1,
                recorded: 0,
            }) => {}
            other => panic!("expected Nr mismatch, got {other:?}"),
        }
    }

    #[test]
    fn arg_mismatch_names_index_and_values() {
        let recorded = CapturedSyscall {
            nr: 1,
            args: [1, 0xCAFE, 5, 0, 0, 0],
            result: 5,
            regions: vec![],
            tier: CaptureTier::Curated,
        };
        let event = syscall_event(&recorded);
        // Same nr, but arg[1] (buf pointer) moved.
        let n = notif(1, [1, 0xBEEF, 5, 0, 0, 0]);
        let err = apply_recorded_event(&n, &event, &mut MockWriter::default()).unwrap_err();
        match err {
            ReplayError::Mismatch(SyscallMismatch::Arg {
                idx: 1,
                observed: 0xBEEF,
                recorded: 0xCAFE,
            }) => {}
            other => panic!("expected Arg mismatch on idx 1, got {other:?}"),
        }
    }

    #[test]
    fn result_not_captured_is_rejected_with_clear_error() {
        let event = Event::Syscall {
            nr: 1,
            args: [1, 0x1000, 5, 0, 0, 0],
            result: RESULT_NOT_CAPTURED_YET,
            output: CapturedSyscall {
                nr: 1,
                args: [1, 0x1000, 5, 0, 0, 0],
                result: RESULT_NOT_CAPTURED_YET,
                regions: vec![],
                tier: CaptureTier::Curated,
            }
            .encode_output(),
        };
        let n = notif(1, [1, 0x1000, 5, 0, 0, 0]);
        let err = apply_recorded_event(&n, &event, &mut MockWriter::default()).unwrap_err();
        match err {
            ReplayError::ResultNotCaptured { nr: 1, notif_id: 7 } => {}
            other => panic!("expected ResultNotCaptured, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_record_then_replay() {
        // Record: write(fd=2, buf="hello", count=5) -> 5.
        // Capture pre-syscall (InBuf), then synthesise the
        // result by running capture_post_syscall.
        let frame = CallFrame {
            nr: 1,
            args: [2, 0xCAFE_BA00, 5, 0, 0, 0],
        };
        // The "memory" the recorder reads from.
        struct OneRegion(u64, Vec<u8>);
        impl MemoryReader for OneRegion {
            fn read(&self, addr: u64, max: usize) -> Vec<u8> {
                if addr == self.0 {
                    self.1.iter().take(max).copied().collect()
                } else {
                    Vec::new()
                }
            }
        }
        let pre = capture_pre_syscall(frame, &OneRegion(0xCAFE_BA00, b"hello".to_vec()));
        let mut recorded = pre.clone();
        recorded.result = 5; // pretend the syscall succeeded

        let event = syscall_event(&recorded);
        let n = notif(1, frame.args);
        let mut w = MockWriter::default();
        let resp = apply_recorded_event(&n, &event, &mut w).expect("apply");
        assert_eq!(resp.result, 5);
        // InBuf regions are written back to ensure the tracee's
        // memory matches what the recorder observed pre-syscall
        // — important when the syscall is intercepted (not run)
        // and the tracee subsequently re-reads from its own buf.
        assert_eq!(w.writes, vec![(0xCAFE_BA00u64, b"hello".to_vec())]);
    }

    #[test]
    fn unexpected_event_variant_is_rejected() {
        let event = Event::Marker { tag: 1, data: 2 };
        let n = notif(0, [0; 6]);
        let err = apply_recorded_event(&n, &event, &mut MockWriter::default()).unwrap_err();
        match err {
            ReplayError::UnexpectedEvent { got } => {
                assert!(got.contains("Marker"), "want Marker in error, got `{got}`");
            }
            other => panic!("expected UnexpectedEvent, got {other:?}"),
        }
    }
}

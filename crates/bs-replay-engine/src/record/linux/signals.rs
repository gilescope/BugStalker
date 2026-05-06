// SPDX-License-Identifier: MIT
//! Sub-phase 3E — signal record/replay primitives.
//!
//! The format crate already has [`Event::Signal { sig_no, pc,
//! siginfo }`]; this module wraps the supervisor-side
//! plumbing.
//!
//! ## Record path
//!
//! The supervisor catches every signal-delivery-stop on the
//! tracee. `PTRACE_O_TRACESYSGOOD` distinguishes seccomp /
//! syscall stops from real signals; the supervisor reads the
//! `siginfo_t` via `PTRACE_GETSIGINFO`, the PC via
//! `PTRACE_GETREGS`, then stamps an [`Event::Signal`].
//!
//! Async signals (timer expiry, `SIGTERM` from a peer) carry
//! the precise PC at delivery so replay can walk forward to
//! the same PC and re-deliver. Sync signals (`SIGSEGV` from a
//! bad load) replay implicitly because the same instructions
//! re-produce the fault — for those the `pc` is advisory.
//!
//! ## Replay path
//!
//! At the recorded PC, the supervisor:
//!
//! 1. `PTRACE_SETSIGINFO` with the recorded `siginfo` bytes.
//! 2. `PTRACE_CONT(sig_no)` — step and deliver. The kernel
//!    routes the signal through the tracee's handler chain as
//!    if it had arrived now.
//!
//! ## What this commit lands
//!
//! Pure-Rust + Linux-only ABI mirrors and helpers:
//!
//! - [`SignalCapture`] — typed struct that round-trips through
//!   [`Event::Signal`].
//! - [`event_for_signal`] / [`signal_from_event`] — encode +
//!   decode helpers.
//! - [`SIGINFO_T_LEN_X86_64`] — fixed length the wire format
//!   always stamps for `siginfo_t`. The kernel's
//!   `siginfo_t` is 128 bytes on Linux x86-64 (per `man 2
//!   sigaction`); we record the whole struct rather than
//!   risk losing the union variants.
//! - [`SignalReplayPlan`] — the replay-side action description
//!   the ptrace driver consumes.

#![cfg(target_os = "linux")]

use crate::format::event::Event;

/// `siginfo_t` size on Linux x86-64 (`<bits/types/siginfo_t.h>`,
/// `_SI_MAX_SIZE = 128`).
pub const SIGINFO_T_LEN_X86_64: usize = 128;

/// One signal observation — the record-side capture and the
/// replay-side input both have this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalCapture {
    /// Signal number (`SIGINT`, `SIGSEGV`, …). Matches the
    /// kernel's `_NSIG` numbering — same as libc::SIGINT etc.
    pub sig_no: u32,
    /// PC at delivery time. For sync signals (program faults)
    /// this is the faulting instruction; for async signals
    /// it's the instruction the tracee was about to execute
    /// when the kernel delivered the signal.
    pub pc: u64,
    /// Opaque `siginfo_t` bytes. The format crate doesn't
    /// decode this — it's the recorder's contract with the
    /// replay shim, which feeds it back through
    /// `PTRACE_SETSIGINFO`.
    pub siginfo: Vec<u8>,
}

impl SignalCapture {
    /// Build a capture from raw fields. Validates that the
    /// `siginfo` length is exactly [`SIGINFO_T_LEN_X86_64`] —
    /// short or long blobs would confuse `PTRACE_SETSIGINFO`
    /// at replay.
    pub fn new(
        sig_no: u32,
        pc: u64,
        siginfo: Vec<u8>,
    ) -> Result<Self, SignalLengthError> {
        if siginfo.len() != SIGINFO_T_LEN_X86_64 {
            return Err(SignalLengthError {
                got: siginfo.len(),
                expected: SIGINFO_T_LEN_X86_64,
            });
        }
        Ok(Self { sig_no, pc, siginfo })
    }
}

/// Error returned by [`SignalCapture::new`] when the supplied
/// `siginfo` blob isn't the expected ABI size.
#[derive(thiserror::Error, Debug, Clone, Copy, Eq, PartialEq)]
#[error(
    "siginfo blob is {got} bytes; PTRACE_SETSIGINFO expects \
     exactly {expected} bytes (sizeof(siginfo_t) on Linux x86-64)"
)]
pub struct SignalLengthError {
    /// Number of bytes the caller supplied.
    pub got: usize,
    /// What the kernel ABI requires.
    pub expected: usize,
}

/// Wire-format helper. Trivial mapping today; the indirection
/// makes the recorder and replayer share one source of truth
/// for the field set.
pub fn event_for_signal(cap: &SignalCapture) -> Event {
    Event::Signal {
        sig_no: cap.sig_no,
        pc: cap.pc,
        siginfo: cap.siginfo.clone(),
    }
}

/// Inverse of [`event_for_signal`]. Returns
/// `Err(SignalLengthError)` if a malformed event has the wrong
/// `siginfo` length.
pub fn signal_from_event(event: &Event) -> Result<SignalCapture, SignalDecodeError> {
    match event {
        Event::Signal {
            sig_no,
            pc,
            siginfo,
        } => SignalCapture::new(*sig_no, *pc, siginfo.clone())
            .map_err(SignalDecodeError::Length),
        other => Err(SignalDecodeError::WrongVariant {
            got: format!("{other:?}"),
        }),
    }
}

/// Errors arising from [`signal_from_event`].
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum SignalDecodeError {
    /// `siginfo` bytes weren't the right length.
    #[error(transparent)]
    Length(SignalLengthError),
    /// Caller passed an event variant other than
    /// [`Event::Signal`].
    #[error("expected Event::Signal, got {got}")]
    WrongVariant {
        /// Debug-format of the unexpected variant.
        got: String,
    },
}

/// Replay-side action description for one signal. The ptrace
/// driver consumes this and runs `PTRACE_SETSIGINFO` +
/// `PTRACE_CONT(sig_no)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalReplayPlan<'a> {
    /// Tracee pid the supervisor will deliver to.
    pub pid: i32,
    /// What signal to deliver; `0` would mean "no signal" but
    /// the recorder never produces 0-numbered Signal events.
    pub sig_no: u32,
    /// `siginfo_t` bytes the kernel will hand the tracee's
    /// signal handler.
    pub siginfo: &'a [u8],
}

/// Validate a [`SignalReplayPlan`] — every field must be in the
/// kernel-acceptable range. Returns `Ok` if the plan is
/// well-formed.
pub fn validate_replay_plan(plan: &SignalReplayPlan<'_>) -> Result<(), SignalReplayError> {
    if plan.sig_no == 0 {
        return Err(SignalReplayError::ZeroSigNo);
    }
    // Linux signal numbers are 1..=64 (`_NSIG`).
    if plan.sig_no > 64 {
        return Err(SignalReplayError::SigNoOutOfRange { got: plan.sig_no });
    }
    if plan.siginfo.len() != SIGINFO_T_LEN_X86_64 {
        return Err(SignalReplayError::Length(SignalLengthError {
            got: plan.siginfo.len(),
            expected: SIGINFO_T_LEN_X86_64,
        }));
    }
    Ok(())
}

/// Errors raised by [`validate_replay_plan`].
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum SignalReplayError {
    /// `sig_no == 0`.
    #[error("signal number 0 doesn't deliver anything; recorder never produces this")]
    ZeroSigNo,
    /// `sig_no > 64`.
    #[error("signal number {got} out of kernel range 1..=64")]
    SigNoOutOfRange {
        /// What the caller passed.
        got: u32,
    },
    /// `siginfo` length wrong.
    #[error("siginfo length: {0}")]
    Length(SignalLengthError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_siginfo(byte: u8) -> Vec<u8> {
        vec![byte; SIGINFO_T_LEN_X86_64]
    }

    #[test]
    fn capture_round_trip_through_event() {
        let cap = SignalCapture::new(
            libc::SIGTERM as u32,
            0xCAFE_F00D,
            fake_siginfo(0xAA),
        )
        .expect("new");
        let ev = event_for_signal(&cap);
        let back = signal_from_event(&ev).expect("decode");
        assert_eq!(cap, back);
    }

    #[test]
    fn length_mismatch_is_rejected_in_constructor() {
        let err = SignalCapture::new(libc::SIGUSR1 as u32, 0, vec![0; 64]).unwrap_err();
        assert_eq!(err.got, 64);
        assert_eq!(err.expected, SIGINFO_T_LEN_X86_64);
    }

    #[test]
    fn wrong_variant_decode_returns_diagnosable_error() {
        let ev = Event::Marker { tag: 0, data: 0 };
        let err = signal_from_event(&ev).unwrap_err();
        match err {
            SignalDecodeError::WrongVariant { got } => assert!(got.contains("Marker")),
            other => panic!("expected WrongVariant, got {other:?}"),
        }
    }

    #[test]
    fn replay_plan_validates_well_formed() {
        let bytes = fake_siginfo(0);
        let plan = SignalReplayPlan {
            pid: 1234,
            sig_no: libc::SIGCHLD as u32,
            siginfo: &bytes,
        };
        assert_eq!(validate_replay_plan(&plan), Ok(()));
    }

    #[test]
    fn replay_plan_rejects_sig_no_zero() {
        let bytes = fake_siginfo(0);
        let plan = SignalReplayPlan { pid: 1, sig_no: 0, siginfo: &bytes };
        assert_eq!(
            validate_replay_plan(&plan),
            Err(SignalReplayError::ZeroSigNo),
        );
    }

    #[test]
    fn replay_plan_rejects_sig_no_too_big() {
        let bytes = fake_siginfo(0);
        let plan = SignalReplayPlan { pid: 1, sig_no: 100, siginfo: &bytes };
        assert_eq!(
            validate_replay_plan(&plan),
            Err(SignalReplayError::SigNoOutOfRange { got: 100 }),
        );
    }

    #[test]
    fn replay_plan_rejects_short_siginfo() {
        let bytes = vec![0u8; 32];
        let plan = SignalReplayPlan {
            pid: 1,
            sig_no: libc::SIGTERM as u32,
            siginfo: &bytes,
        };
        match validate_replay_plan(&plan) {
            Err(SignalReplayError::Length(SignalLengthError { got: 32, expected: 128 })) => {}
            other => panic!("expected length error, got {other:?}"),
        }
    }

    #[test]
    fn siginfo_t_len_matches_libc_constant() {
        // libc 0.2 exposes a `MAX_SIGNAL` and a per-target
        // `siginfo_t`; the size of `siginfo_t` on Linux x86_64
        // matches our hard-coded 128.
        assert_eq!(
            std::mem::size_of::<libc::siginfo_t>(),
            SIGINFO_T_LEN_X86_64,
            "libc's siginfo_t size drifted from {SIGINFO_T_LEN_X86_64}; \
             update the recorder constant",
        );
    }
}

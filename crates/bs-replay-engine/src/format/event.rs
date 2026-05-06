// SPDX-License-Identifier: MIT
//! Trace event vocabulary.
//!
//! Sub-phase 3A ships the *frame* — the codec, segment file format,
//! manifest. Real syscall, signal, and instruction-trap event
//! variants land in 3B / 3D / 3E.
//!
//! ## Variant-ordering rule
//!
//! `rkyv` encodes enums by variant *index*, so the on-disk
//! representation of `Marker` is the byte `0`. Adding a new variant
//! at the **end** of this enum is forward-compatible: old traces
//! that only wrote `Marker` still parse against new code because
//! the discriminant `0` still maps to `Marker`. Conversely:
//!
//! - Inserting a variant in the middle renumbers every later
//!   variant — old traces silently misinterpret as the wrong
//!   variant. This requires a `MAX_SUPPORTED_FORMAT_VERSION` bump
//!   and a reader that maps old discriminants explicitly.
//! - Removing a variant is the same — old traces would fail.
//! - Changing a variant's *fields* is a wire-format change —
//!   same bump rule.
//!
//! The defensive policy: **always append, never insert or remove**,
//! at v1 of the format. The version-rejection path (covered in
//! `version::tests`) ensures a future trace written by a newer
//! BugStalker fails fast against an older one.

use rkyv::{Archive, Deserialize, Serialize};

/// One recorded event. Public-API stability is **not** promised at
/// version 0; the variant set grows additively as the recorder
/// gains coverage.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum Event {
    /// Diagnostic placeholder. Carries an opaque tag + payload so
    /// integration tests can write a known sequence and read it
    /// back. Keep at index 0; new variants append below.
    Marker {
        /// Caller-defined tag. No semantics at this layer.
        tag: u32,
        /// Caller-defined payload word.
        data: u64,
    },
    /// One Linux syscall observation. Recorded by sub-phase 3B
    /// once seccomp-bpf user-notify is wired up; for the moment
    /// only the wire format is defined here so the rest of the
    /// pipeline can be exercised.
    ///
    /// Field meanings mirror the kernel's syscall ABI: `nr` is
    /// `__NR_*`, `args` is the six general-purpose argument
    /// registers (RDI, RSI, RDX, R10, R8, R9 on x86-64; X0–X5 on
    /// aarch64), `result` is the return value sign-extended into
    /// 64 bits (errors are negative `errno`s on the kernel ABI),
    /// `output` is any pointed-to data the kernel wrote that we
    /// have to replay back into the tracee's address space (e.g.
    /// the bytes returned by `read(fd, buf, n)`).
    Syscall {
        /// Syscall number (`__NR_*`).
        nr: u32,
        /// Six argument registers in ABI order.
        args: [u64; 6],
        /// Sign-extended return value; negative is `-errno`.
        result: i64,
        /// Bytes the kernel wrote into pointed-to buffers, in the
        /// order they appear in the call's output buffer list.
        /// Empty when the syscall has no out-pointer side effects.
        output: Vec<u8>,
    },
}

impl Event {
    /// Upper-bound estimate of this event's contribution to the
    /// segment archive in bytes. Used by the writer to decide when
    /// to rotate before compression. Slight over-estimation is
    /// safe; under-estimation is not (could overshoot the segment
    /// size cap).
    pub fn approx_archive_size(&self) -> usize {
        // rkyv enum tag overhead + alignment slack. Conservative.
        const VARIANT_OVERHEAD: usize = 16;
        match self {
            // 4-byte tag + 8-byte data + alignment.
            Self::Marker { .. } => VARIANT_OVERHEAD + 4 + 8,
            // 4-byte nr + 6×8-byte args + 8-byte result + the
            // archived Vec layout (16-byte rkyv RelPtr + len) +
            // the payload bytes themselves.
            Self::Syscall { output, .. } => {
                VARIANT_OVERHEAD + 4 + 6 * 8 + 8 + 16 + output.len()
            }
        }
    }
}

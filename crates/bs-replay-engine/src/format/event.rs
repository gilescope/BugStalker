// SPDX-License-Identifier: MIT
//! Trace event vocabulary.
//!
//! Sub-phase 3A ships the *frame* — the codec, segment file format,
//! manifest. The actual syscall, signal, and instruction-trap event
//! variants land in 3B / 3D / 3E. Until then this enum carries one
//! placeholder `Marker` variant so the writer/reader pipeline can
//! be exercised end-to-end.

use rkyv::{Archive, Deserialize, Serialize};

/// One recorded event. Public-API stability is **not** promised at
/// version 0; the variant set grows as the recorder gains coverage.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum Event {
    /// Diagnostic placeholder. Carries an opaque tag + payload so
    /// integration tests can write a known sequence and read it
    /// back. Removed (or renumbered) once 3B starts adding the real
    /// syscall variants — variant order is part of the on-disk
    /// format, so any change here is a `FormatVersion` bump.
    Marker {
        /// Caller-defined tag. No semantics at this layer.
        tag: u32,
        /// Caller-defined payload word.
        data: u64,
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
        }
    }
}

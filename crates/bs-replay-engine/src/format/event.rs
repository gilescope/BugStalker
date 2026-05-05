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

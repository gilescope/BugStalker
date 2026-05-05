// SPDX-License-Identifier: MIT
//! `mach_vm_remap`-based checkpoint primitives for Darwin.
//!
//! Skeleton only. The Mach equivalent of fork-with-ptrace is
//! ~2 weeks of work in the plan; this module fixes the public
//! surface and parks the implementation for a follow-up batch.

/// A snapshot of the debuggee's writable regions taken via Mach VM.
///
/// On Darwin we do not get a free copy-on-write child the way Linux
/// `fork(2)` provides one, so each checkpoint owns its own copy of
/// the dirty pages at capture time. Cost is correspondingly higher.
#[derive(Debug)]
pub struct Checkpoint {
    /// Wall-clock instant the snapshot was taken, mach absolute time.
    pub captured_mach_abs: u64,
    /// Program counter at snapshot time.
    pub pc: u64,
}

impl Checkpoint {
    /// Take a snapshot of the debuggee's writable regions.
    ///
    /// Implementation deferred.
    pub fn take(_pc: u64) -> Result<Self, CheckpointError> {
        Err(CheckpointError::Unimplemented)
    }
}

/// Errors arising from checkpoint capture or replay on Darwin.
#[derive(thiserror::Error, Debug)]
pub enum CheckpointError {
    /// Skeleton placeholder.
    #[error("Darwin checkpoint mechanism not yet implemented")]
    Unimplemented,
}

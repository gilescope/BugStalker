// SPDX-License-Identifier: MIT
//! `fork(2)` + `ptrace` + `SIGSTOP` checkpoint primitives.
//!
//! Skeleton only — Phase 5 starts here. The implementation lands as
//! follow-up batches; this module fixes the public surface so the
//! rest of the crate can compile against it.

use core::num::NonZeroI32;

/// A frozen-in-time copy of the debuggee, suspended via `SIGSTOP`.
///
/// Created by [`Checkpoint::take`]. The child receives `SIGCONT` when
/// the user requests replay through it.
#[derive(Debug)]
pub struct Checkpoint {
    /// PID of the suspended fork.
    pub pid: NonZeroI32,
    /// Wall-clock instant the fork was taken, monotonic clock.
    pub captured_ns: u64,
    /// Program counter of the parent at fork time, opaque to this
    /// crate — supplied by the caller to keep us free of DWARF deps.
    pub pc: u64,
}

impl Checkpoint {
    /// Take a checkpoint of `parent_pid` at the current instant.
    ///
    /// Implementation deferred — see plan § "Tier 2 — Checkpoint-based
    /// replay" step 2 ("the fork").
    pub fn take(_parent_pid: NonZeroI32, _pc: u64) -> Result<Self, CheckpointError> {
        Err(CheckpointError::Unimplemented)
    }
}

/// Errors arising from checkpoint capture or replay.
#[derive(thiserror::Error, Debug)]
pub enum CheckpointError {
    /// Skeleton placeholder — implementation pending.
    #[error("checkpoint mechanism not yet implemented")]
    Unimplemented,
}

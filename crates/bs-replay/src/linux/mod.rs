// SPDX-License-Identifier: MIT
//! Linux Tier 2 fork-checkpoint primitives.
//!
//! See `doc/plans/phase-5-time-travel.md` § "Tier 2 — Checkpoint-based
//! replay".

pub mod fork_checkpoint;
pub mod fork_self;
pub mod proc_maps;

pub use fork_self::{ForkHandle, ForkMechanismError, LinuxForkSelfMechanism};
pub use proc_maps::{read_proc_maps, MemoryRegion, Permissions, ProcMapsError};

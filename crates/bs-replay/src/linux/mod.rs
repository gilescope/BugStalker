// SPDX-License-Identifier: MIT
//! Linux Tier 2 fork-checkpoint primitives.
//!
//! See `doc/plans/phase-5-time-travel.md` § "Tier 2 — Checkpoint-based
//! replay".

pub mod checkpoint_capture;
pub mod fork_checkpoint;
pub mod fork_self;
pub mod proc_maps;
pub mod proc_mem;
pub mod proc_regs;
pub mod tier2;

pub use checkpoint_capture::{
    CaptureError, CapturedRegion, DecodeError, RestoreReport, WritableState,
    capture_writable_state, from_payload, restore_writable_state, to_payload,
};
pub use fork_self::{ForkHandle, ForkMechanismError, LinuxForkSelfMechanism};
pub use proc_maps::{MemoryRegion, Permissions, ProcMapsError, read_proc_maps};
pub use proc_mem::{ProcMemError, read_bytes_at, read_region, write_bytes_at};
pub use proc_regs::{RegError, RegisterState, capture_registers, restore_registers};
pub use tier2::{
    Tier2Capture, Tier2DecodeError, Tier2Error, Tier2State, from_payload as tier2_from_payload,
    to_payload as tier2_to_payload,
};

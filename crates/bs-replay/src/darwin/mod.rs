// SPDX-License-Identifier: MIT
//! Darwin Tier 2 checkpoint primitives.
//!
//! Apple does not expose `fork(2)`-with-ptrace cleanly. The fallback
//! is `mach_vm_remap` snapshots of writable regions — heavier than
//! Linux COW forks but functional. See
//! `doc/plans/phase-5-time-travel.md` § "Tier 2 — Checkpoint-based
//! replay" → "darwin/checkpoint.rs".

pub mod checkpoint;

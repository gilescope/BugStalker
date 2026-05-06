// SPDX-License-Identifier: MIT
//! Linux replay back-end.
//!
//! Modules:
//!
//! - `shim` — supplies recorded syscall results in place of running
//!   them (3C).
//! - `scheduler` — drives threads in recorded order at recorded
//!   instruction counts (3F).
//!
//! `scheduler` is deferred to a follow-up batch.

pub mod replay_child;
pub mod shim;

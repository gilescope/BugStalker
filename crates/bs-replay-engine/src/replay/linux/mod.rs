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
//! Deferred to follow-up batches.

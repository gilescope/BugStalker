// SPDX-License-Identifier: MIT
//! Darwin record back-end (best-effort, plan § "macOS strategy").
//!
//! Modules:
//!
//! - `dyld_shim` — `DYLD_INSERT_LIBRARIES`-based syscall shim covering
//!   `libsystem` syscalls. Misses anything bypassing libc.
//!
//! Deferred to follow-up batches.

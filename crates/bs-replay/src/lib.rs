// SPDX-License-Identifier: MIT
//! BugStalker time-travel — Tier 2 fork-checkpoint replay.
//!
//! See `doc/plans/phase-5-time-travel.md` for the architecture.
//!
//! Tier 2 periodically `fork(2)`s the debuggee at safe points and
//! keeps the children suspended. To inspect state at an earlier PC
//! we replay forward from the nearest checkpoint to the target.
//!
//! This crate is the Tier 2 implementation; Tier 3 (deterministic
//! record-and-replay) lives in `bs-replay-engine`. Tier 1 is the
//! Intel-PT reverse-step UX layered over Phase 6's PT trace and
//! is exposed via `bs-replay-driver` once that crate lands (3I).
//!
//! ## Tier 2 in one paragraph
//!
//! `fork(2)` snapshots are copy-on-write at the kernel: cheap to
//! take, expensive only as memory diverges. We keep up to
//! `MAX_CHECKPOINTS` of them in a ring; when full, the oldest
//! `SIGKILL`s. Replay attaches BugStalker to the chosen child via
//! `PTRACE_SEIZE`, sets a one-shot breakpoint at the target PC,
//! and `SIGCONT`s it forward.
//!
//! Determinism is best-effort: forks share fds initially and
//! diverge on I/O; thread scheduling on replay differs from the
//! original. For UI work and pure-compute bugs this is enough.
//! For race conditions, callers must escalate to Tier 3.

#![deny(missing_docs)]

pub mod replay;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod darwin;

/// Upper bound on the checkpoint ring buffer.
///
/// Phase 5 invariant: `self.checkpoints.len() <= MAX_CHECKPOINTS`.
pub const MAX_CHECKPOINTS: usize = 32;

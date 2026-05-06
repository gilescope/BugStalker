// SPDX-License-Identifier: MIT
//! BugStalker time-travel — sub-phase 3I integration seam.
//!
//! See `doc/plans/phase-5-time-travel.md` § "3I. BugStalker driver
//! integration".
//!
//! The plan's promise: "BugStalker doesn't know whether it's
//! attached to a live process or a replay; same breakpoints, same
//! watchpoints, same step semantics." The eventual `bs-replay-driver`
//! exposes the same `ptrace`-event surface the existing tracee
//! plumbing consumes; the debugger's command dispatcher sees a
//! uniform interface either way.
//!
//! That's a multi-week landing — `crates/.../tracee.rs` is heavily
//! Linux-ptrace-coupled. This scaffold lays the integration crate
//! down with a small [`TraceReplayer`] API the future fake-tracee
//! drives through: open a trace, walk events, jump by checkpoint,
//! report position. Each method composes a fresh
//! [`bs_replay_engine::format::EventCursor`] under the hood, which
//! is cheap because the cursor's expensive state (decompressed
//! segments) lives in caches inside [`bs_replay_engine::format::TraceReader`].

#![warn(missing_docs)]

pub mod capture;
pub mod host;
pub mod replayer;
pub mod reverse;

pub use capture::capture_host_manifest;
pub use host::{host_features, HostDetectError};
pub use replayer::{
    BuildIdMismatch, HostMismatchError, ReplayError, ReplayabilityError, TraceReplayer,
};
pub use reverse::ReverseDebugger;

/// Re-export the engine for downstream consumers — most callers
/// want both the driver and the engine's types.
pub use bs_replay_engine as engine;

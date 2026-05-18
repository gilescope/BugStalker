// SPDX-License-Identifier: MIT
//! BugStalker driver integration — sub-phase 3I.
//!
//! Exposes the same ptrace-event surface BugStalker's existing
//! tracee plumbing already consumes. The debugger does not know
//! whether it's attached to a live process or a replay; same
//! breakpoints, same watchpoints, same step semantics.
//!
//! The eventual `bs-replay-driver` crate hosts the production
//! integration; this stub fixes the architectural seam.

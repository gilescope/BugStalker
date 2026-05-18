// SPDX-License-Identifier: MIT
//! Tier-selection and replay orchestration.
//!
//! BugStalker picks the finest-grained back-end available:
//!
//! | Tier | Mechanism                          | Window  |
//! | ---- | ---------------------------------- | ------- |
//! | 1    | Intel PT decode (Phase 6)          | seconds |
//! | 2    | `fork(2)` checkpoints (this crate) | minutes |
//! | 3    | full record-replay (`bs-replay-engine`) | hours |
//!
//! See `doc/plans/phase-5-time-travel.md` § "Cross-tier UX".

use core::fmt;

/// Which time-travel tier produced a given replay frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Tier {
    /// Intel PT exact-PC reverse step. State is *not* recoverable
    /// without escalating to a checkpoint or full trace.
    Pt,
    /// `fork(2)` checkpoint replay. State is exact at checkpoint
    /// time, best-effort thereafter.
    ForkCheckpoint,
    /// Full deterministic record-replay (Tier 3, `bs-replay-engine`).
    /// State is exact at any recorded instant.
    RecordReplay,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pt => "tier1-pt",
            Self::ForkCheckpoint => "tier2-fork",
            Self::RecordReplay => "tier3-rr",
        })
    }
}

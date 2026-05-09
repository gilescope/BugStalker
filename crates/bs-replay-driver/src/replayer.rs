// SPDX-License-Identifier: MIT
//! `TraceReplayer` — owned wrapper around a recorded trace.
//!
//! Holds a [`TraceReader`] and a position counter. Each
//! [`Self::next_event`] call composes a fresh cursor at the
//! current position and pulls one event; the underlying segment
//! cache inside `TraceReader` makes that cheap (~one event-vec
//! deserialization per call within the same segment).
//!
//! Why position counter instead of holding a long-lived
//! `EventCursor`? Cursors borrow `&TraceReader` — making
//! `TraceReplayer` self-referential. The position-counter shape
//! gives the same outward behaviour with no `ouroboros` /
//! self-borrow gymnastics; the cursor's internal segment caching
//! still pays off because `TraceReader` itself caches across
//! calls.

use std::path::Path;

use bs_replay_engine::format::checkpoint::CheckpointHeader;
use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::{TraceReadError, TraceReader};

/// Owned handle to a replay-able trace. The integration seam
/// between the trace engine and the debugger.
#[derive(Debug)]
pub struct TraceReplayer {
    reader: TraceReader,
    next_event_index: u64,
}

impl TraceReplayer {
    /// Open the trace at `dir`. Validates the manifest version
    /// and enumerates segments / checkpoints up front; per-event
    /// work is deferred to `next_event`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, ReplayError> {
        let reader = TraceReader::open(dir).map_err(ReplayError::Engine)?;
        Ok(Self {
            reader,
            next_event_index: 0,
        })
    }

    /// The trace's manifest. Cheap.
    pub fn manifest(&self) -> &Manifest {
        self.reader.manifest()
    }

    /// Position of the *next* event `next_event` would yield.
    /// Equal to the number of events that have been consumed.
    pub fn position(&self) -> u64 {
        self.next_event_index
    }

    /// Yield the next event in record order, or `Ok(None)` past the
    /// end of the trace.
    pub fn next_event(&mut self) -> Result<Option<Event>, ReplayError> {
        let mut cursor = self.reader.cursor_at(self.next_event_index);
        let res = cursor.next().map_err(ReplayError::Engine)?;
        if res.is_some() {
            self.next_event_index += 1;
        }
        Ok(res)
    }

    /// Move the playhead to `event_index` without yielding any
    /// events. The next [`Self::next_event`] returns the event at
    /// that index.
    pub fn seek_to(&mut self, event_index: u64) {
        self.next_event_index = event_index;
    }

    /// Find the latest checkpoint with `header.event_index <=
    /// target_event_index` — the integration point Tier 2 / Tier 3
    /// replay uses to locate where to restore process state from.
    pub fn find_checkpoint_at_or_before(
        &self,
        target_event_index: u64,
    ) -> Result<Option<CheckpointHeader>, ReplayError> {
        self.reader
            .find_checkpoint_at_or_before(target_event_index)
            .map_err(ReplayError::Engine)
    }

    /// Borrow the underlying engine reader. Useful for callers
    /// that need a query API the driver hasn't yet exposed
    /// directly (e.g. segment ranges, checkpoint enumeration).
    pub fn reader(&self) -> &TraceReader {
        &self.reader
    }

    /// Verify the host can replay this trace's recorded CPU
    /// features. Plan §Invariants: replay host must support the
    /// recording's features. Fails with [`HostMismatchError`]
    /// naming every feature the host lacks; returns `Ok(())` if
    /// the host is a strict or equal superset.
    ///
    /// `host_features` is supplied by the caller because feature
    /// enumeration is host-OS specific (e.g. `/proc/cpuinfo` flags
    /// on Linux, `sysctl hw.optional.*` on Darwin). The driver
    /// stays portable; recipe-level helpers can land later.
    pub fn check_host_compatibility<S: AsRef<str>>(
        &self,
        host_features: &[S],
    ) -> Result<(), HostMismatchError> {
        let missing = self.manifest().missing_host_features(host_features);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(HostMismatchError { missing })
        }
    }

    /// Verify the binary the caller is about to replay against
    /// matches the build-id stamped into the manifest at record
    /// time. Plan: "build-id of recorded binary (cross-checked at
    /// replay)". A mismatch means the caller likely rebuilt the
    /// binary between recording and replay; instructions, layout,
    /// and DWARF have all moved, and replay would be silently
    /// wrong. Fail fast with both ids surfaced.
    ///
    /// Comparison is byte-exact — the caller is responsible for
    /// canonicalising hex case (lower vs upper) at record and
    /// replay sides.
    pub fn check_build_id(&self, host_build_id: &str) -> Result<(), BuildIdMismatch> {
        if self.manifest().build_id == host_build_id {
            Ok(())
        } else {
            Err(BuildIdMismatch {
                recorded: self.manifest().build_id.clone(),
                actual: host_build_id.to_owned(),
            })
        }
    }

    /// Convenience: run every replay-time host invariant the
    /// driver currently knows about. Returns the first error
    /// encountered as a [`ReplayabilityError`]. Order is
    /// deliberately stable so a CI failure pinpoints the same
    /// reason across runs.
    pub fn check_replayability<S: AsRef<str>>(
        &self,
        host_features: &[S],
        expected_build_id: Option<&str>,
    ) -> Result<(), ReplayabilityError> {
        if let Some(bid) = expected_build_id {
            self.check_build_id(bid)
                .map_err(ReplayabilityError::BuildId)?;
        }
        self.check_host_compatibility(host_features)
            .map_err(ReplayabilityError::HostFeatures)?;
        Ok(())
    }
}

/// Driver-level error. Currently a thin wrapper around the engine
/// error; future work (real fake-tracee integration) will add
/// driver-specific failure modes.
#[derive(thiserror::Error, Debug)]
pub enum ReplayError {
    /// The trace engine returned an error.
    #[error("trace engine: {0}")]
    Engine(TraceReadError),
}

/// Replay-time host/recording mismatch. Names every CPU feature
/// the recording used that the replay host doesn't have.
#[derive(thiserror::Error, Debug, Eq, PartialEq, Clone)]
pub struct HostMismatchError {
    /// Features the recording used that this host lacks.
    pub missing: Vec<String>,
}

impl core::fmt::Display for HostMismatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "replay host is missing CPU features the recording used: {}",
            self.missing.join(", "),
        )
    }
}

/// Build-id mismatch between the binary the manifest names and the
/// binary the caller is about to replay against.
#[derive(thiserror::Error, Debug, Eq, PartialEq, Clone)]
pub struct BuildIdMismatch {
    /// Build-id the manifest stamped at record time.
    pub recorded: String,
    /// Build-id the caller supplied at replay time.
    pub actual: String,
}

impl core::fmt::Display for BuildIdMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "binary build-id changed between record and replay; \
             recorded={recorded}, actual={actual}",
            recorded = self.recorded,
            actual = self.actual,
        )
    }
}

/// Aggregate failure for `check_replayability`.
#[derive(thiserror::Error, Debug, Eq, PartialEq, Clone)]
pub enum ReplayabilityError {
    /// Build-id stamped at record time does not match the host's.
    #[error(transparent)]
    BuildId(BuildIdMismatch),
    /// Host CPU is missing features the recording used.
    #[error(transparent)]
    HostFeatures(HostMismatchError),
}

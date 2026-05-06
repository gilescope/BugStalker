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
        Ok(Self { reader, next_event_index: 0 })
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

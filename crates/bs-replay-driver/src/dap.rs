// SPDX-License-Identifier: MIT
//! Rust shapes for the plan's `bs/replay*` DAP custom requests.
//!
//! See `doc/plans/phase-5-time-travel.md` § "DAP integration".
//! These are the wire-shape definitions a future DAP server uses
//! when handling time-travel commands; the handlers on
//! [`TraceReplayer`] answer the queries the engine layer already
//! supports today.
//!
//! No serde here. JSON encoding is the DAP server's concern; the
//! driver crate stays at "owned Rust types + handlers". Wiring
//! these to `serde_json::Value` (or `dap-types`) is a one-line
//! `From` impl per type.
//!
//! ## Coverage of the plan's request set
//!
//! | Plan request                  | Status            | Reason                              |
//! | ----------------------------- | ----------------- | ----------------------------------- |
//! | `bs/replayCheckpointList`     | implemented       | engine answers today                |
//! | `bs/replayJump`               | implemented       | seek_to + checkpoint lookup         |
//! | `bs/replayTimeline`           | implemented       | sparse timeline from current data   |
//! | `bs/replayRecord`             | type-only stub    | needs the recorder (sub-phase 3B)   |
//! | `bs/replayLoad`               | type-only stub    | needs debugger attach machinery     |

use bs_replay_engine::format::checkpoint::CheckpointHeader;

use crate::replayer::{ReplayError, TraceReplayer};

// ---------------------------------------------------------------------------
// bs/replayCheckpointList
// ---------------------------------------------------------------------------

/// Request: enumerate checkpoints in the current trace.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct ReplayCheckpointListRequest {}

/// Response: per-checkpoint summary.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayCheckpointListResponse {
    /// Checkpoints in stored order.
    pub checkpoints: Vec<CheckpointSummary>,
}

/// Per-checkpoint summary the UI scrubber renders.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CheckpointSummary {
    /// 1-based checkpoint index.
    pub index: u64,
    /// Number of events captured at the moment this checkpoint was taken.
    pub event_index: u64,
}

impl From<&CheckpointHeader> for CheckpointSummary {
    fn from(h: &CheckpointHeader) -> Self {
        Self { index: h.index, event_index: h.event_index }
    }
}

// ---------------------------------------------------------------------------
// bs/replayJump
// ---------------------------------------------------------------------------

/// Request: jump the playhead to a recorded location.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayJumpRequest {
    /// Where to jump.
    pub target: JumpTarget,
}

/// Where the user asked the playhead to land. Specified explicitly
/// so the DAP layer doesn't have to overload one numeric field
/// with two meanings.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum JumpTarget {
    /// Land on the event_index recorded by checkpoint `index`.
    Checkpoint {
        /// 1-based checkpoint index.
        index: u64,
    },
    /// Land on `event_index` directly. The replay engine then
    /// uses [`TraceReplayer::find_checkpoint_at_or_before`] to
    /// locate the nearest checkpoint to *restore from*.
    EventIndex {
        /// Global event index to seek to.
        event_index: u64,
    },
}

/// Response: where the playhead actually landed + which checkpoint
/// the replay engine should restore from to reach that target.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayJumpResponse {
    /// Final playhead position.
    pub event_index: u64,
    /// 1-based checkpoint to restore from, or `None` when no
    /// checkpoint covers the target (replay walks from event 0).
    pub restore_from_checkpoint: Option<u64>,
}

// ---------------------------------------------------------------------------
// bs/replayTimeline
// ---------------------------------------------------------------------------

/// Request: sparse timeline of recorded events for a UI scrubber.
///
/// The plan says "breakpoint hits, syscall events, and signal
/// deliveries"; the v1 trace records syscalls as variants but no
/// signals or breakpoints yet, so this is a *forward-compatible*
/// shape returning what's available today.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct ReplayTimelineRequest {}

/// Response: timeline waypoints in event-order.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayTimelineResponse {
    /// Total event count across all segments.
    pub total_events: u64,
    /// Sparse waypoint markers — checkpoints today, more variants
    /// as the recorder gains coverage.
    pub waypoints: Vec<TimelineWaypoint>,
}

/// One waypoint on the timeline.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TimelineWaypoint {
    /// A checkpoint sample.
    Checkpoint {
        /// 1-based checkpoint index.
        index: u64,
        /// Event index the checkpoint marks.
        event_index: u64,
    },
}

// ---------------------------------------------------------------------------
// bs/replayRecord — type-only stub (needs the recorder, sub-phase 3B)
// ---------------------------------------------------------------------------

/// Request: start or stop the recorder.
///
/// Wire-shape only — the recorder itself (sub-phase 3B) does not
/// yet exist, so [`TraceReplayer`] does not implement a handler
/// for this request.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRecordRequest {
    /// `true` → begin recording; `false` → stop & finalise.
    pub start: bool,
    /// Output trace directory (when `start == true`).
    pub trace_path: Option<String>,
}

/// Response shape mirror.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRecordResponse {
    /// Whether the recorder is now actively recording.
    pub recording: bool,
    /// Number of events written so far. Always 0 in the stub.
    pub events_written: u64,
}

// ---------------------------------------------------------------------------
// bs/replayLoad — open a trace and report summary stats
// ---------------------------------------------------------------------------

/// Request: load a saved trace and attach the debugger to its
/// virtual tracee. The handler opens the trace and returns
/// summary counts; the *attach* leg (sub-phase 3I fake-tracee)
/// happens downstream from the DAP server using the returned
/// [`TraceReplayer`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayLoadRequest {
    /// Path to the trace directory on disk.
    pub trace_path: String,
}

/// Response.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayLoadResponse {
    /// Total events the loaded trace contains.
    pub total_events: u64,
    /// Number of segments.
    pub total_segments: u64,
    /// Number of checkpoints.
    pub total_checkpoints: u64,
}

/// Handle `bs/replayLoad`. Opens the trace at the requested path
/// and reports its summary stats; returns the [`TraceReplayer`]
/// the caller stashes for subsequent [`bs/replayJump`](dap_jump)
/// and friends. Free function rather than a method because the
/// request *constructs* the replayer rather than acting on one.
pub fn load(
    req: &ReplayLoadRequest,
) -> Result<(TraceReplayer, ReplayLoadResponse), ReplayError> {
    let replayer = TraceReplayer::open(&req.trace_path)?;
    let segments = replayer
        .reader()
        .segment_event_ranges()
        .map_err(ReplayError::Engine)?;
    let total_events: u64 = segments.iter().map(|r| r.event_count).sum();
    let total_segments = segments.len() as u64;
    let total_checkpoints = replayer.reader().checkpoint_indices().len() as u64;
    Ok((
        replayer,
        ReplayLoadResponse {
            total_events,
            total_segments,
            total_checkpoints,
        },
    ))
}

// ---------------------------------------------------------------------------
// Handlers on TraceReplayer
// ---------------------------------------------------------------------------

impl TraceReplayer {
    /// Handle `bs/replayCheckpointList`.
    pub fn dap_checkpoint_list(
        &self,
        _req: &ReplayCheckpointListRequest,
    ) -> Result<ReplayCheckpointListResponse, ReplayError> {
        let headers = self
            .reader()
            .checkpoint_headers()
            .map_err(ReplayError::Engine)?;
        Ok(ReplayCheckpointListResponse {
            checkpoints: headers.iter().map(CheckpointSummary::from).collect(),
        })
    }

    /// Handle `bs/replayJump`. Does not yield events; updates the
    /// playhead and reports which checkpoint should be the
    /// restore-from anchor.
    pub fn dap_jump(
        &mut self,
        req: &ReplayJumpRequest,
    ) -> Result<ReplayJumpResponse, ReplayError> {
        let target_event = match req.target {
            JumpTarget::EventIndex { event_index } => event_index,
            JumpTarget::Checkpoint { index } => {
                let headers = self
                    .reader()
                    .checkpoint_headers()
                    .map_err(ReplayError::Engine)?;
                let h = headers
                    .iter()
                    .find(|h| h.index == index)
                    .ok_or_else(|| {
                        ReplayError::Engine(
                            bs_replay_engine::format::TraceReadError::CheckpointHeaderMismatch {
                                file_index: index,
                                header_index: 0,
                            },
                        )
                    })?;
                h.event_index
            }
        };
        self.seek_to(target_event);
        let restore_from_checkpoint = self
            .find_checkpoint_at_or_before(target_event)?
            .map(|h| h.index);
        Ok(ReplayJumpResponse { event_index: target_event, restore_from_checkpoint })
    }

    /// Handle `bs/replayTimeline`.
    pub fn dap_timeline(
        &self,
        _req: &ReplayTimelineRequest,
    ) -> Result<ReplayTimelineResponse, ReplayError> {
        let total_events = self
            .reader()
            .segment_event_ranges()
            .map_err(ReplayError::Engine)?
            .iter()
            .map(|r| r.event_count)
            .sum();
        let headers = self
            .reader()
            .checkpoint_headers()
            .map_err(ReplayError::Engine)?;
        let waypoints = headers
            .iter()
            .map(|h| TimelineWaypoint::Checkpoint {
                index: h.index,
                event_index: h.event_index,
            })
            .collect();
        Ok(ReplayTimelineResponse { total_events, waypoints })
    }
}

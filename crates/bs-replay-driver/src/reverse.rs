// SPDX-License-Identifier: MIT
//! Tier 1 reverse-step skeleton.
//!
//! See `doc/plans/phase-5-time-travel.md` § "Tier 1 — Intel PT
//! reverse step" for the eventual user-visible UX:
//!
//! ```text
//! (bs) rstep
//! stepped backward 1 instruction; now at src/handler.rs:42
//! (bs) rcontinue
//! stopped at breakpoint #2 (src/middleware.rs:18); 142 ms ago in run
//! ```
//!
//! That display ("now at src/handler.rs:42") needs each event to
//! carry program-counter / file-line metadata, which the v1
//! [`Event`] does not yet. The *navigation* shape — backing the
//! playhead up event by event, scanning backward for a breakpoint
//! — is fully expressible against event indices today, and that's
//! what reverse-debugging hangs off.
//!
//! When events grow PC, the rendering layer that turns "at event N"
//! into "at src/handler.rs:42" lands as a thin formatter on top of
//! this skeleton. The plan's `rnext` ("step backward 1 line") will
//! similarly be a `rstep` loop that compares the resolved file:line
//! pair across iterations.

use bs_replay_engine::format::event::Event;

use crate::replayer::{ReplayError, TraceReplayer};

/// A reverse-debugging session over a recorded trace.
///
/// Wraps a [`TraceReplayer`] with a forward+backward navigation
/// API and a list of event-index breakpoints. The replayer's
/// position counter is the playhead.
#[derive(Debug)]
pub struct ReverseDebugger {
    replayer: TraceReplayer,
    /// Event indices the user has set breakpoints on. Sorted on
    /// insert so [`Self::rcontinue`] / [`Self::run_forward`] can
    /// binary-search across them.
    breakpoints: Vec<u64>,
}

impl ReverseDebugger {
    /// Wrap an existing replayer. Starts with no breakpoints; the
    /// playhead is wherever the replayer left it.
    pub fn new(replayer: TraceReplayer) -> Self {
        Self { replayer, breakpoints: Vec::new() }
    }

    /// Borrow the underlying replayer (to inspect manifest, run
    /// host-compatibility checks, etc.).
    pub fn replayer(&self) -> &TraceReplayer {
        &self.replayer
    }

    /// Current playhead in the global event-index space.
    pub fn position(&self) -> u64 {
        self.replayer.position()
    }

    /// Move the playhead without yielding events.
    pub fn seek_to(&mut self, event_index: u64) {
        self.replayer.seek_to(event_index);
    }

    /// Add an event-index breakpoint. Idempotent — adding an
    /// already-present index is a no-op.
    pub fn add_breakpoint(&mut self, event_index: u64) {
        if let Err(insert_at) = self.breakpoints.binary_search(&event_index) {
            self.breakpoints.insert(insert_at, event_index);
        }
    }

    /// Remove an event-index breakpoint. Returns true if a
    /// breakpoint was actually removed.
    pub fn remove_breakpoint(&mut self, event_index: u64) -> bool {
        match self.breakpoints.binary_search(&event_index) {
            Ok(at) => {
                self.breakpoints.remove(at);
                true
            }
            Err(_) => false,
        }
    }

    /// Sorted breakpoint event indices.
    pub fn breakpoints(&self) -> &[u64] {
        &self.breakpoints
    }

    /// Forward step. Yields the event at the current playhead and
    /// advances by one. `Ok(None)` past the end of trace.
    pub fn step(&mut self) -> Result<Option<Event>, ReplayError> {
        self.replayer.next_event()
    }

    /// Reverse step. Decrements the playhead by one (clamped at
    /// zero) and yields the event at the new position without
    /// advancing past it.
    ///
    /// `Ok(None)` is returned only if the playhead was already at
    /// zero — there is no event "before" event 0.
    pub fn rstep(&mut self) -> Result<Option<Event>, ReplayError> {
        let here = self.position();
        if here == 0 {
            return Ok(None);
        }
        let target = here - 1;
        // Snapshot the event without consuming it: read it via a
        // throwaway forward step from `target`, then leave the
        // playhead at `target` for the next call.
        self.replayer.seek_to(target);
        let ev = self.replayer.next_event()?;
        self.replayer.seek_to(target);
        Ok(ev)
    }

    /// Continue forward until any breakpoint fires, or the end of
    /// the trace is reached. Returns the resting event index.
    ///
    /// Behaviour at start-of-walk: if the playhead is *exactly* on
    /// a breakpoint, that breakpoint is *not* re-hit — the user
    /// just stopped there, the natural next step is to leave it.
    pub fn run_forward(&mut self) -> Result<u64, ReplayError> {
        loop {
            match self.replayer.next_event()? {
                Some(_) => {
                    let at = self.replayer.position();
                    if self.breakpoints.binary_search(&at).is_ok() {
                        return Ok(at);
                    }
                }
                None => return Ok(self.replayer.position()),
            }
        }
    }

    /// Continue backward until any breakpoint fires, or the start
    /// of the trace is reached. Returns the resting event index
    /// (0 means we ran off the start without hitting any).
    pub fn rcontinue(&mut self) -> Result<u64, ReplayError> {
        let here = self.position();
        // Find the largest breakpoint strictly less than `here`.
        let pos = self.breakpoints.partition_point(|&b| b < here);
        if pos == 0 {
            self.seek_to(0);
            return Ok(0);
        }
        let stop_at = self.breakpoints[pos - 1];
        self.seek_to(stop_at);
        Ok(stop_at)
    }
}

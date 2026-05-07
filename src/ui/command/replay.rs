// SPDX-License-Identifier: MIT
//! Tier 1 reverse-step REPL surface.
//!
//! Loads a Phase 5 trace and exposes the navigation primitives in
//! [`bs_replay_driver`] as REPL commands. Operates on the trace
//! only — does not rewind the live tracee. Coexists with normal
//! debugging: a loaded trace is sidecar state and the standard
//! commands keep working.
//!
//! Commands:
//!
//! | Command            | Effect                                                |
//! | ------------------ | ----------------------------------------------------- |
//! | `replay load <p>`  | Open a trace at path `p`. Replaces any prior session. |
//! | `replay unload`    | Drop the loaded trace (if any).                       |
//! | `replay status`    | Print playhead, total events, and trace metadata.     |
//! | `rstep`  (`rs`)    | Step the playhead back one event.                     |
//! | `rstep-fwd` (`rsf`)| Step the playhead forward one event (mirror of rstep).|
//! | `rcontinue` (`rc`) | Run the playhead back to the previous replay         |
//! |                    | breakpoint (or to event 0 if none match).             |
//! | `rbreak <idx>`     | Add a replay breakpoint at event index `idx`.         |
//! | `rbreak-clear <i>` | Remove a replay breakpoint at event index `i`.        |

use bs_replay_driver::{ReplayError, ReverseDebugger, TraceReplayer};

/// User-typed reverse-debug command. Each variant maps to one
/// invocation of [`Handler::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `replay load <path>` — open a trace.
    Load {
        /// On-disk trace directory.
        trace_path: String,
    },
    /// `replay unload` — drop the loaded session.
    Unload,
    /// `replay status` — print summary of the loaded session.
    Status,
    /// `rstep` — step backward one event.
    RStep,
    /// `rstep-fwd` — step forward one event.
    RStepForward,
    /// `rcontinue` — step backward to the previous replay
    /// breakpoint.
    RContinue,
    /// `rbreak <idx>` — add a replay breakpoint at event index.
    RAddBreakpoint {
        /// Event index to mark.
        event_index: u64,
    },
    /// `rbreak-clear <idx>` — remove a replay breakpoint.
    RRemoveBreakpoint {
        /// Event index to clear.
        event_index: u64,
    },
}

/// Handler-side outcome of one command. The console renderer turns
/// each variant into human-readable output.
#[derive(Debug)]
pub enum Outcome {
    /// `replay load` succeeded; display summary.
    Loaded {
        /// Trace path that was loaded.
        trace_path: String,
        /// Total events the loaded trace carries.
        total_events: u64,
        /// Build-id stamped into the manifest.
        build_id: String,
    },
    /// `replay unload` succeeded — there was a session and we
    /// dropped it. `had_session=false` means there wasn't one.
    Unloaded {
        /// Whether a session was actually present before this call.
        had_session: bool,
    },
    /// `replay status` — playhead + event count + manifest metadata.
    Status {
        /// Current playhead event index.
        position: u64,
        /// Total events in the trace.
        total_events: u64,
        /// Number of active replay breakpoints.
        breakpoint_count: usize,
        /// Build-id stamped into the manifest.
        build_id: String,
        /// Trace's `recorded_at` timestamp, if any.
        recorded_at: Option<String>,
    },
    /// `rstep` / `rstep-fwd` — moved the playhead by one step.
    Stepped {
        /// New playhead event index.
        position: u64,
        /// Whether this was a backward step (`rstep`) or forward
        /// (`rstep-fwd`).
        backward: bool,
    },
    /// `rcontinue` — playhead landed at this event index. If a
    /// replay breakpoint matched, that event index; otherwise 0.
    Continued {
        /// New playhead position.
        position: u64,
    },
    /// `rbreak <idx>` — registered a new breakpoint.
    BreakpointAdded {
        /// The event index marked.
        event_index: u64,
    },
    /// `rbreak-clear <idx>` — removed (or no-oped, see `was_present`).
    BreakpointRemoved {
        /// The event index targeted.
        event_index: u64,
        /// True iff a breakpoint was actually present at that
        /// event index before removal.
        was_present: bool,
    },
}

/// Errors a Tier 1 REPL command can produce.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// User issued a session-touching command (`rstep`, etc.) but
    /// no trace is loaded.
    #[error("no trace loaded — use `replay load <path>` first")]
    NoSession,
    /// Engine-level failure when opening, walking, or reading the
    /// trace.
    #[error("trace operation failed: {0}")]
    Replay(#[from] ReplayError),
}

/// Shared session state for the Tier 1 REPL surface. The console
/// owns one of these and threads `&mut` references into the handler
/// per command. `None` = no trace loaded.
pub type Session = Option<ReverseDebugger>;

/// Stateful handler bound to the long-lived `Session`. Each command
/// runs against the current session (which may be loaded, unloaded,
/// or replaced).
pub struct Handler<'a> {
    session: &'a mut Session,
}

impl<'a> Handler<'a> {
    /// Wrap the long-lived session reference.
    pub fn new(session: &'a mut Session) -> Self {
        Self { session }
    }

    /// Dispatch one command against the current session.
    pub fn handle(&mut self, cmd: Command) -> Result<Outcome, Error> {
        match cmd {
            Command::Load { trace_path } => self.do_load(trace_path),
            Command::Unload => Ok(self.do_unload()),
            Command::Status => self.do_status(),
            Command::RStep => self.do_rstep(),
            Command::RStepForward => self.do_step_forward(),
            Command::RContinue => self.do_rcontinue(),
            Command::RAddBreakpoint { event_index } => {
                self.do_add_breakpoint(event_index)
            }
            Command::RRemoveBreakpoint { event_index } => {
                self.do_remove_breakpoint(event_index)
            }
        }
    }

    fn do_load(&mut self, trace_path: String) -> Result<Outcome, Error> {
        let replayer = TraceReplayer::open(&trace_path)?;
        let total_events: u64 = replayer
            .reader()
            .segment_event_ranges()
            .map_err(ReplayError::Engine)?
            .iter()
            .map(|r| r.event_count)
            .sum();
        let build_id = replayer.manifest().build_id.clone();
        *self.session = Some(ReverseDebugger::new(replayer));
        Ok(Outcome::Loaded { trace_path, total_events, build_id })
    }

    fn do_unload(&mut self) -> Outcome {
        let had_session = self.session.is_some();
        *self.session = None;
        Outcome::Unloaded { had_session }
    }

    fn do_status(&self) -> Result<Outcome, Error> {
        let rdb = self.session.as_ref().ok_or(Error::NoSession)?;
        let replayer = rdb.replayer();
        let total_events: u64 = replayer
            .reader()
            .segment_event_ranges()
            .map_err(ReplayError::Engine)?
            .iter()
            .map(|r| r.event_count)
            .sum();
        let manifest = replayer.manifest();
        Ok(Outcome::Status {
            position: rdb.position(),
            total_events,
            breakpoint_count: rdb.breakpoints().len(),
            build_id: manifest.build_id.clone(),
            recorded_at: manifest.recorded_at.clone(),
        })
    }

    fn do_rstep(&mut self) -> Result<Outcome, Error> {
        let rdb = self.session.as_mut().ok_or(Error::NoSession)?;
        rdb.rstep()?;
        Ok(Outcome::Stepped { position: rdb.position(), backward: true })
    }

    fn do_step_forward(&mut self) -> Result<Outcome, Error> {
        let rdb = self.session.as_mut().ok_or(Error::NoSession)?;
        rdb.step()?;
        Ok(Outcome::Stepped { position: rdb.position(), backward: false })
    }

    fn do_rcontinue(&mut self) -> Result<Outcome, Error> {
        let rdb = self.session.as_mut().ok_or(Error::NoSession)?;
        let position = rdb.rcontinue()?;
        Ok(Outcome::Continued { position })
    }

    fn do_add_breakpoint(&mut self, event_index: u64) -> Result<Outcome, Error> {
        let rdb = self.session.as_mut().ok_or(Error::NoSession)?;
        rdb.add_breakpoint(event_index);
        Ok(Outcome::BreakpointAdded { event_index })
    }

    fn do_remove_breakpoint(
        &mut self,
        event_index: u64,
    ) -> Result<Outcome, Error> {
        let rdb = self.session.as_mut().ok_or(Error::NoSession)?;
        let was_present = rdb.remove_breakpoint(event_index);
        Ok(Outcome::BreakpointRemoved { event_index, was_present })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_replay_driver::engine::format::event::Event;
    use bs_replay_driver::engine::format::manifest::Manifest;
    use bs_replay_driver::engine::format::version::FormatVersion;
    use bs_replay_driver::engine::format::TraceWriter;
    use std::fs;
    use std::path::PathBuf;

    fn fixture_manifest() -> Manifest {
        Manifest {
            format_version: FormatVersion::V1,
            build_id: "ab".repeat(16),
            kernel_release: "test".to_owned(),
            cpu_features: vec![],
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            initial_env: vec![],
            initial_cwd: "/tmp".to_owned(),
            initial_args: vec![],
            recorded_at: Some("2026-05-07T00:00:00Z".to_owned()),
            initial_fds: vec![],
        }
    }

    fn fixture_trace(label: &str, count: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bs-ui-replay-{label}-{}",
            std::process::id(),
        ));
        let _ = fs::remove_dir_all(&dir);
        let mut writer = TraceWriter::create(&dir, &fixture_manifest()).unwrap();
        for i in 0..count {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.finish().unwrap();
        dir
    }

    #[test]
    fn rstep_without_session_errors_with_no_session() {
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        let err = h.handle(Command::RStep).unwrap_err();
        assert!(matches!(err, Error::NoSession), "got: {err}");
    }

    #[test]
    fn load_replaces_any_prior_session() {
        let dir = fixture_trace("load-replace", 3);
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        let r = h
            .handle(Command::Load { trace_path: dir.to_string_lossy().into_owned() })
            .unwrap();
        match r {
            Outcome::Loaded { total_events, .. } => assert_eq!(total_events, 3),
            other => panic!("expected Loaded, got {other:?}"),
        }
        assert!(session.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unload_drops_the_session() {
        let dir = fixture_trace("unload", 1);
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        h.handle(Command::Load { trace_path: dir.to_string_lossy().into_owned() })
            .unwrap();
        assert!(matches!(
            h.handle(Command::Unload).unwrap(),
            Outcome::Unloaded { had_session: true },
        ));
        assert!(session.is_none());
        // A second unload reports had_session = false.
        let mut h = Handler::new(&mut session);
        assert!(matches!(
            h.handle(Command::Unload).unwrap(),
            Outcome::Unloaded { had_session: false },
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Forward + back through a five-event trace; playhead must
    /// match the navigation invariants. Catches off-by-one bugs in
    /// the handler's seek arithmetic.
    #[test]
    fn step_back_then_forward_round_trips_position() {
        let dir = fixture_trace("step", 5);
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        h.handle(Command::Load { trace_path: dir.to_string_lossy().into_owned() })
            .unwrap();
        // Walk forward twice: 0 → 1 → 2.
        h.handle(Command::RStepForward).unwrap();
        let r = h.handle(Command::RStepForward).unwrap();
        match r {
            Outcome::Stepped { position, backward } => {
                assert_eq!(position, 2);
                assert!(!backward);
            }
            other => panic!("expected Stepped, got {other:?}"),
        }
        // One step back: 2 → 1.
        let r = h.handle(Command::RStep).unwrap();
        match r {
            Outcome::Stepped { position, backward } => {
                assert_eq!(position, 1);
                assert!(backward);
            }
            other => panic!("expected Stepped, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn breakpoints_round_trip() {
        let dir = fixture_trace("rbp", 5);
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        h.handle(Command::Load { trace_path: dir.to_string_lossy().into_owned() })
            .unwrap();
        match h.handle(Command::RAddBreakpoint { event_index: 3 }).unwrap() {
            Outcome::BreakpointAdded { event_index } => assert_eq!(event_index, 3),
            other => panic!("expected BreakpointAdded, got {other:?}"),
        }
        // Removing an existing one reports was_present = true.
        match h.handle(Command::RRemoveBreakpoint { event_index: 3 }).unwrap() {
            Outcome::BreakpointRemoved { was_present, event_index } => {
                assert_eq!(event_index, 3);
                assert!(was_present);
            }
            other => panic!("expected BreakpointRemoved, got {other:?}"),
        }
        // Removing a non-existent one reports was_present = false.
        match h.handle(Command::RRemoveBreakpoint { event_index: 99 }).unwrap() {
            Outcome::BreakpointRemoved { was_present, .. } => assert!(!was_present),
            other => panic!("expected BreakpointRemoved, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_reports_position_total_and_metadata() {
        let dir = fixture_trace("status", 7);
        let mut session: Session = None;
        let mut h = Handler::new(&mut session);
        h.handle(Command::Load { trace_path: dir.to_string_lossy().into_owned() })
            .unwrap();
        h.handle(Command::RStepForward).unwrap();
        h.handle(Command::RStepForward).unwrap();
        match h.handle(Command::Status).unwrap() {
            Outcome::Status {
                position,
                total_events,
                breakpoint_count,
                recorded_at,
                ..
            } => {
                assert_eq!(position, 2);
                assert_eq!(total_events, 7);
                assert_eq!(breakpoint_count, 0);
                assert_eq!(recorded_at, Some("2026-05-07T00:00:00Z".to_owned()));
            }
            other => panic!("expected Status, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

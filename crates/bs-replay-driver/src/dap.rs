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
//! | `bs/replayRecord`             | implemented (Linux) | wraps `record_program`            |
//! | `bs/replayCapture`            | implemented (Linux) | wraps `capture_one_shot`          |
//! | `bs/replayRestore`            | implemented (Linux) | wraps Tier 2 restore primitives   |
//! | `bs/replayLoad`               | implemented       | opens trace + reports stats         |

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
        Self {
            index: h.index,
            event_index: h.event_index,
        }
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
// bs/replayRecord — synchronous full-program record into a fresh trace dir
// ---------------------------------------------------------------------------

/// Request: record a program end-to-end into a fresh trace dir.
///
/// Synchronous: the handler returns when the recorded program
/// exits (or hits the iteration cap). A future "background +
/// stop" request can layer over this without breaking the
/// shape; for now we mirror what the engine actually exposes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRecordRequest {
    /// Output trace directory. Refuses to overwrite — caller
    /// supplies a fresh path.
    pub trace_path: String,
    /// argv passed to the recorded program. `argv[0]` is the
    /// executable. Must be non-empty.
    pub argv: Vec<String>,
    /// envp for the recorded program. Each entry is `(key,
    /// value)`; the handler joins on `=`. Empty means "no
    /// environment", *not* "inherit" — the DAP server
    /// chooses what to expose.
    pub envp: Vec<(String, String)>,
    /// build-id to stamp into the manifest. Hex string. The
    /// DAP server typically extracts this from the binary's
    /// ELF NT_GNU_BUILD_ID; if unknown, supply a placeholder
    /// (e.g. 32 hex zeroes).
    pub build_id: String,
    /// Optional kernel-release / label override stamped into
    /// the manifest. None → engine-default ("unknown" off-Linux
    /// or `/proc/sys/kernel/osrelease` on Linux).
    pub kernel_label: Option<String>,
    /// Recorder tunables.
    pub options: ReplayRecordOptions,
}

/// Wire-shape mirror of [`crate::record::RecordOptions`]. Kept
/// separate so the DAP shape can grow knobs the engine doesn't
/// expose yet (and vice versa).
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ReplayRecordOptions {
    /// Hard cap on loop iterations. None → engine default
    /// (currently 2_000_000).
    pub max_iterations: Option<u64>,
    /// Patch the tracee's vDSO so libc fast paths trip the
    /// recorder. See [`crate::record::RecordOptions::patch_vdso`].
    pub patch_vdso: bool,
    /// Trap RDTSC/RDTSCP via PR_SET_TSC. See
    /// [`crate::record::RecordOptions::trap_tsc`].
    pub trap_tsc: bool,
    /// Trap CPUID via ARCH_SET_CPUID. See
    /// [`crate::record::RecordOptions::disable_cpuid`].
    pub disable_cpuid: bool,
}

/// How the recorded program left the recorder loop. Mirrors
/// [`crate::record::ExitStatus`] in DAP-friendly variant form.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReplayRecordExitKind {
    /// Tracee called exit/exit_group with this code.
    Exited {
        /// Process exit code.
        code: i32,
    },
    /// Tracee was killed by this signal.
    Signalled {
        /// Signal number.
        signal: i32,
    },
    /// Recorder hit its iteration cap before the tracee exited.
    /// The trace is well-formed but truncated.
    IterationCap {
        /// Iteration count at which the cap fired.
        iterations: u64,
    },
}

/// Response: per-event counts plus how the run ended.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRecordResponse {
    /// Path the trace was written to (echoed for clients that
    /// derive it from the request and want one canonical
    /// answer).
    pub trace_path: String,
    /// Total events written across all variants.
    pub events_written: u64,
    /// `Event::Syscall` events written.
    pub syscall_events: u64,
    /// `Event::PcMarker` events written.
    pub pc_marker_events: u64,
    /// `Event::Signal` events written.
    pub signal_events: u64,
    /// `Event::InstructionTrap` events written.
    pub instruction_traps: u64,
    /// Loop iterations executed.
    pub iterations: u64,
    /// How the recorded program ended.
    pub exit: ReplayRecordExitKind,
}

/// Handle `bs/replayRecord`. Linux only — the engine's recorder
/// is `cfg(target_os = "linux")`. Synchronous: returns when the
/// recorded program exits or hits the iteration cap.
#[cfg(target_os = "linux")]
pub fn record(req: &ReplayRecordRequest) -> Result<ReplayRecordResponse, DapRecordError> {
    use std::ffi::CString;

    use bs_replay_engine::VERSION as ENGINE_VERSION;
    use bs_replay_engine::format::manifest::Manifest;
    use bs_replay_engine::format::version::FormatVersion;

    use crate::record::{self, ExitStatus, RecordOptions};

    if req.argv.is_empty() {
        return Err(DapRecordError::EmptyArgv);
    }
    let argv: Vec<CString> = req
        .argv
        .iter()
        .map(|s| CString::new(s.as_str()).map_err(|_| DapRecordError::ArgvNul))
        .collect::<Result<_, _>>()?;
    let envp: Vec<CString> = req
        .envp
        .iter()
        .map(|(k, v)| {
            if k.contains('=') {
                return Err(DapRecordError::EnvKeyContainsEquals(k.clone()));
            }
            CString::new(format!("{k}={v}")).map_err(|_| DapRecordError::EnvNul)
        })
        .collect::<Result<_, _>>()?;

    let manifest = Manifest {
        format_version: FormatVersion::V1,
        build_id: req.build_id.clone(),
        kernel_release: req
            .kernel_label
            .clone()
            .unwrap_or_else(default_kernel_release),
        cpu_features: crate::host::host_features().unwrap_or_default(),
        engine_version: ENGINE_VERSION.to_owned(),
        initial_env: req.envp.clone(),
        initial_cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unknown>".to_owned()),
        initial_args: req.argv.clone(),
        recorded_at: Some(chrono::Utc::now().to_rfc3339()),
        initial_fds: vec![],
    };

    let options = RecordOptions {
        max_iterations: req.options.max_iterations.unwrap_or(2_000_000),
        patch_vdso: req.options.patch_vdso,
        trap_tsc: req.options.trap_tsc,
        disable_cpuid: req.options.disable_cpuid,
    };

    let report = record::record_program(&req.trace_path, &manifest, argv, envp, options)
        .map_err(DapRecordError::Record)?;

    Ok(ReplayRecordResponse {
        trace_path: req.trace_path.clone(),
        events_written: report.pc_marker_events
            + report.syscall_events
            + report.signal_events
            + report.instruction_traps,
        syscall_events: report.syscall_events,
        pc_marker_events: report.pc_marker_events,
        signal_events: report.signal_events,
        instruction_traps: report.instruction_traps,
        iterations: report.iterations,
        exit: match report.exit_status {
            ExitStatus::Exited(code) => ReplayRecordExitKind::Exited { code },
            ExitStatus::Signalled(signal) => ReplayRecordExitKind::Signalled { signal },
            ExitStatus::IterationCap(iterations) => {
                ReplayRecordExitKind::IterationCap { iterations }
            }
        },
    })
}

#[cfg(target_os = "linux")]
fn default_kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Errors arising from [`record`]. Layered: argv/envp validation
/// happens before any spawning; recorder errors come back wrapped.
#[cfg(target_os = "linux")]
#[derive(thiserror::Error, Debug)]
pub enum DapRecordError {
    /// `argv` was empty — at minimum `argv[0]` (the executable)
    /// must be supplied.
    #[error("argv must contain at least argv[0]")]
    EmptyArgv,
    /// One of the argv entries contained a NUL byte; the kernel
    /// won't accept it via `execve`.
    #[error("argv contains a NUL byte")]
    ArgvNul,
    /// One of the env vars contained a NUL byte.
    #[error("envp contains a NUL byte")]
    EnvNul,
    /// POSIX disallows `=` in env-var keys; surface this rather
    /// than silently misparse.
    #[error("env key contains `=`: {0}")]
    EnvKeyContainsEquals(String),
    /// The recorder loop itself failed — almost always a setup
    /// (yama lockdown, missing /proc/<pid>/mem) or disk-full
    /// error, not a tracee bug.
    #[error("recorder: {0}")]
    Record(#[from] crate::record::RecordProgramError),
}

// ---------------------------------------------------------------------------
// bs/replayCapture — Tier 2 snapshot into a fresh trace dir (Linux only handler)
// ---------------------------------------------------------------------------

/// Request: snapshot one Tier 2 checkpoint (memory + registers)
/// into a fresh trace directory.
///
/// Linux-only at the handler level; the wire shape is portable so
/// non-Linux DAP servers can still represent the request.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayCaptureRequest {
    /// Path to a fresh trace dir to create. Refuses to overwrite.
    pub trace_path: String,
    /// Caller-defined key — typically the event index at the
    /// moment of capture.
    pub key: u64,
}

/// Response.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayCaptureResponse {
    /// 1-based index of the checkpoint inside the trace.
    pub checkpoint_index: u64,
    /// Size of the encoded Tier 2 state payload, in bytes.
    pub payload_bytes: u64,
}

/// Handle `bs/replayCapture`. Linux only — the handler calls
/// into `bs_replay::linux` (fork+SIGSTOP+SEIZE+memory+regs).
#[cfg(target_os = "linux")]
pub fn capture(
    req: &ReplayCaptureRequest,
) -> Result<ReplayCaptureResponse, crate::record::RecordError> {
    let manifest = crate::capture::capture_host_manifest(format!("key-{}", req.key));
    let report = crate::record::capture_one_shot(&req.trace_path, &manifest, req.key)?;
    Ok(ReplayCaptureResponse {
        checkpoint_index: report.checkpoint_index,
        payload_bytes: report.payload_bytes,
    })
}

// ---------------------------------------------------------------------------
// bs/replayRestore — write a recorded checkpoint into a target process
// ---------------------------------------------------------------------------

/// Request: restore a recorded checkpoint into an
/// already-ptrace-attached target process. Caller is responsible
/// for SEIZE'ing the target before calling.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRestoreRequest {
    /// Path to the trace directory containing the checkpoint.
    pub trace_path: String,
    /// 1-based checkpoint index to restore from.
    pub checkpoint_index: u64,
    /// PID of the (already-SEIZE'd) target to restore into.
    pub target_pid: i32,
}

/// Response.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReplayRestoreResponse {
    /// Memory regions whose bytes were written into the target.
    pub regions_written: u64,
    /// Memory regions that failed to write (e.g. unmapped at
    /// restore time). Caller decides whether this is acceptable.
    pub regions_skipped: u64,
}

/// Handle `bs/replayRestore`. Linux only.
#[cfg(target_os = "linux")]
pub fn restore(req: &ReplayRestoreRequest) -> Result<ReplayRestoreResponse, RestoreError> {
    use bs_replay::linux::checkpoint_capture::restore_writable_state;
    use bs_replay::linux::proc_regs::restore_registers;
    use bs_replay::linux::tier2;
    use bs_replay_engine::format::TraceReader;

    let reader = TraceReader::open(&req.trace_path).map_err(RestoreError::TraceOpen)?;
    let cp = reader
        .open_checkpoint(req.checkpoint_index)
        .map_err(RestoreError::CheckpointOpen)?;
    let state = tier2::from_payload(&cp.payload).map_err(RestoreError::Decode)?;
    let target = nix::unistd::Pid::from_raw(req.target_pid);
    let report =
        restore_writable_state(target, &state.writable).map_err(RestoreError::RestoreMem)?;
    restore_registers(target, &state.regs).map_err(RestoreError::RestoreReg)?;
    Ok(ReplayRestoreResponse {
        regions_written: report.written as u64,
        regions_skipped: report.skipped as u64,
    })
}

/// Errors arising from `restore`.
#[cfg(target_os = "linux")]
#[derive(thiserror::Error, Debug)]
pub enum RestoreError {
    /// Couldn't open the trace dir.
    #[error("trace open: {0}")]
    TraceOpen(bs_replay_engine::format::TraceReadError),
    /// Couldn't open the named checkpoint inside the trace.
    #[error("checkpoint open: {0}")]
    CheckpointOpen(bs_replay_engine::format::TraceReadError),
    /// Decoding the Tier 2 payload failed.
    #[error("decode: {0}")]
    Decode(bs_replay::linux::tier2::Tier2DecodeError),
    /// Memory restore failed.
    #[error("restore memory: {0}")]
    RestoreMem(bs_replay::linux::proc_mem::ProcMemError),
    /// Register restore failed.
    #[error("restore registers: {0}")]
    RestoreReg(bs_replay::linux::proc_regs::RegError),
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
    /// Wall-clock instant the recording started (RFC 3339), if
    /// the writer stamped one. None for traces predating that
    /// field. UI uses this for "recorded at YYYY-MM-DD" display.
    pub recorded_at: Option<String>,
    /// Build-id of the recorded binary. Same hex string the
    /// manifest carries — DAP clients echo it back to the user
    /// so they can confirm the right binary is loaded.
    pub build_id: String,
}

/// Handle `bs/replayLoad`. Opens the trace at the requested path
/// and reports its summary stats; returns the [`TraceReplayer`]
/// the caller stashes for subsequent [`bs/replayJump`](dap_jump)
/// and friends. Free function rather than a method because the
/// request *constructs* the replayer rather than acting on one.
pub fn load(req: &ReplayLoadRequest) -> Result<(TraceReplayer, ReplayLoadResponse), ReplayError> {
    let replayer = TraceReplayer::open(&req.trace_path)?;
    let segments = replayer
        .reader()
        .segment_event_ranges()
        .map_err(ReplayError::Engine)?;
    let total_events: u64 = segments.iter().map(|r| r.event_count).sum();
    let total_segments = segments.len() as u64;
    let total_checkpoints = replayer.reader().checkpoint_indices().len() as u64;
    let recorded_at = replayer.manifest().recorded_at.clone();
    let build_id = replayer.manifest().build_id.clone();
    Ok((
        replayer,
        ReplayLoadResponse {
            total_events,
            total_segments,
            total_checkpoints,
            recorded_at,
            build_id,
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
    pub fn dap_jump(&mut self, req: &ReplayJumpRequest) -> Result<ReplayJumpResponse, ReplayError> {
        let target_event = match req.target {
            JumpTarget::EventIndex { event_index } => event_index,
            JumpTarget::Checkpoint { index } => {
                let headers = self
                    .reader()
                    .checkpoint_headers()
                    .map_err(ReplayError::Engine)?;
                let h = headers.iter().find(|h| h.index == index).ok_or_else(|| {
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
        Ok(ReplayJumpResponse {
            event_index: target_event,
            restore_from_checkpoint,
        })
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
        Ok(ReplayTimelineResponse {
            total_events,
            waypoints,
        })
    }
}

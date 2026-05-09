// SPDX-License-Identifier: MIT
//! DAP-handler coverage. Each request shape is round-tripped
//! against a real on-disk trace via `TraceReplayer::dap_*`.

use std::fs;
use std::path::PathBuf;

use bs_replay_driver::TraceReplayer;
use bs_replay_driver::dap::{
    JumpTarget, ReplayCheckpointListRequest, ReplayJumpRequest, ReplayLoadRequest,
    ReplayTimelineRequest, TimelineWaypoint, load,
};
use bs_replay_driver::engine::format::TraceWriter;
use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;

fn manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "test".to_owned(),
        cpu_features: vec![],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec![],
        recorded_at: None,
        initial_fds: vec![],
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-driver-dap-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Five events split into segments at events {3, 7}, with two
/// checkpoints at event_index = 3 and 7.
fn make_trace_with_two_checkpoints(dir: &PathBuf) {
    let mut writer = TraceWriter::create(dir, &manifest()).unwrap();
    for i in 0..3u32 {
        writer
            .write_event(Event::Marker { tag: i, data: 0 })
            .unwrap();
    }
    writer.take_checkpoint(b"A".to_vec()).unwrap();
    for i in 3..7u32 {
        writer
            .write_event(Event::Marker { tag: i, data: 0 })
            .unwrap();
    }
    writer.take_checkpoint(b"B".to_vec()).unwrap();
    for i in 7..10u32 {
        writer
            .write_event(Event::Marker { tag: i, data: 0 })
            .unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn dap_checkpoint_list_returns_each_in_order() {
    let dir = temp_dir("cp-list");
    make_trace_with_two_checkpoints(&dir);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let resp = replayer
        .dap_checkpoint_list(&ReplayCheckpointListRequest::default())
        .unwrap();
    assert_eq!(resp.checkpoints.len(), 2);
    assert_eq!(resp.checkpoints[0].index, 1);
    assert_eq!(resp.checkpoints[0].event_index, 3);
    assert_eq!(resp.checkpoints[1].index, 2);
    assert_eq!(resp.checkpoints[1].event_index, 7);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_jump_to_checkpoint_lands_on_its_event_index() {
    let dir = temp_dir("jump-cp");
    make_trace_with_two_checkpoints(&dir);
    let mut replayer = TraceReplayer::open(&dir).unwrap();
    let resp = replayer
        .dap_jump(&ReplayJumpRequest {
            target: JumpTarget::Checkpoint { index: 2 },
        })
        .unwrap();
    assert_eq!(resp.event_index, 7);
    assert_eq!(resp.restore_from_checkpoint, Some(2));
    assert_eq!(replayer.position(), 7);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_jump_to_event_index_picks_nearest_prior_checkpoint() {
    let dir = temp_dir("jump-ev");
    make_trace_with_two_checkpoints(&dir);
    let mut replayer = TraceReplayer::open(&dir).unwrap();

    // event 5 sits between checkpoints A (event 3) and B (event 7).
    let resp = replayer
        .dap_jump(&ReplayJumpRequest {
            target: JumpTarget::EventIndex { event_index: 5 },
        })
        .unwrap();
    assert_eq!(resp.event_index, 5);
    assert_eq!(resp.restore_from_checkpoint, Some(1)); // A

    // event 0 sits before any checkpoint.
    let resp = replayer
        .dap_jump(&ReplayJumpRequest {
            target: JumpTarget::EventIndex { event_index: 0 },
        })
        .unwrap();
    assert_eq!(resp.event_index, 0);
    assert_eq!(resp.restore_from_checkpoint, None);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_jump_to_unknown_checkpoint_index_errors() {
    let dir = temp_dir("jump-bad");
    make_trace_with_two_checkpoints(&dir);
    let mut replayer = TraceReplayer::open(&dir).unwrap();
    let err = replayer
        .dap_jump(&ReplayJumpRequest {
            target: JumpTarget::Checkpoint { index: 99 },
        })
        .unwrap_err();
    let s = format!("{err}");
    assert!(
        s.contains("99") || s.contains("checkpoint") || s.contains("disagree"),
        "unexpected error: {s}",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_timeline_reports_total_events_and_checkpoint_waypoints() {
    let dir = temp_dir("timeline");
    make_trace_with_two_checkpoints(&dir);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let resp = replayer
        .dap_timeline(&ReplayTimelineRequest::default())
        .unwrap();
    assert_eq!(resp.total_events, 10);
    assert_eq!(resp.waypoints.len(), 2);
    match &resp.waypoints[0] {
        TimelineWaypoint::Checkpoint { index, event_index } => {
            assert_eq!(*index, 1);
            assert_eq!(*event_index, 3);
        }
    }
    match &resp.waypoints[1] {
        TimelineWaypoint::Checkpoint { index, event_index } => {
            assert_eq!(*index, 2);
            assert_eq!(*event_index, 7);
        }
    }
    fs::remove_dir_all(&dir).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn dap_capture_writes_a_decodable_checkpoint() {
    use bs_replay_driver::dap::{ReplayCaptureRequest, capture};
    use bs_replay_driver::engine::format::TraceReader;

    let dir = temp_dir("dap-capture");
    let req = ReplayCaptureRequest {
        trace_path: dir.to_string_lossy().into_owned(),
        key: 7,
    };
    let resp = match capture(&req) {
        Ok(r) => r,
        Err(e) => {
            let s = format!("{e:?}");
            if s.contains("EPERM") || s.contains("EACCES") {
                eprintln!("skipping dap_capture: {e:?}");
                return;
            }
            panic!("capture failed: {e:?}");
        }
    };
    assert_eq!(resp.checkpoint_index, 1);
    assert!(resp.payload_bytes > 0);

    // Sanity-reopen via the engine to confirm the trace landed.
    let reader = TraceReader::open(&dir).expect("reopen failed");
    assert_eq!(reader.checkpoint_indices(), &[1u64]);
    fs::remove_dir_all(&dir).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn dap_capture_then_restore_into_fresh_fork_round_trips() {
    use bs_replay::linux::fork_self::LinuxForkSelfMechanism;
    use bs_replay::linux::proc_mem::{read_bytes_at, write_bytes_at};
    use bs_replay::ring::CheckpointMechanism;
    use bs_replay_driver::dap::{ReplayCaptureRequest, ReplayRestoreRequest, capture, restore};

    // The headline DAP-driven flow: capture into a trace, then
    // restore from that same trace into a fresh fork. End-to-end
    // through the DAP-shaped public surface.

    // Heap buffer the parent shares with both forks at the same VA.
    let buf: Vec<u8> = vec![0u8; 64];
    let addr = buf.as_ptr() as u64;

    let dir = temp_dir("dap-cap-restore");
    let req = ReplayCaptureRequest {
        trace_path: dir.to_string_lossy().into_owned(),
        key: 0,
    };
    let cap_resp = match capture(&req) {
        Ok(r) => r,
        Err(e) => {
            let s = format!("{e:?}");
            if s.contains("EPERM") || s.contains("EACCES") {
                eprintln!("skipping dap_capture_then_restore: {e:?}");
                return;
            }
            panic!("capture failed: {e:?}");
        }
    };

    // Take a fresh fork and SEIZE it as the restore target.
    let mut mech = LinuxForkSelfMechanism::new();
    let target = mech.take(0).expect("fork target");
    std::thread::sleep(std::time::Duration::from_millis(50));
    if let Err(e) = mech.seize(&target) {
        eprintln!("skipping dap_capture_then_restore: seize: {e:?}");
        mech.kill(target).expect("kill target");
        fs::remove_dir_all(&dir).ok();
        return;
    }

    // Perturb the target so we can prove restore brought capture's
    // bytes back.
    let sentinel = vec![0xab; buf.len()];
    write_bytes_at(target.pid, addr, &sentinel).expect("perturb target");

    // Restore via the DAP handler.
    let restore_req = ReplayRestoreRequest {
        trace_path: dir.to_string_lossy().into_owned(),
        checkpoint_index: cap_resp.checkpoint_index,
        target_pid: target.pid.as_raw(),
    };
    let restore_resp = restore(&restore_req).expect("restore failed");
    assert!(restore_resp.regions_written > 0);

    // Target's bytes at addr now should match the captured A's
    // (which were all zero — fork happened before we touched buf).
    let post = read_bytes_at(target.pid, addr, buf.len()).expect("read post");
    assert_eq!(post, vec![0u8; buf.len()]);

    mech.kill(target).expect("kill target");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_load_returns_replayer_plus_summary_for_a_real_trace() {
    let dir = temp_dir("load-ok");
    make_trace_with_two_checkpoints(&dir);
    let req = ReplayLoadRequest {
        trace_path: dir.to_string_lossy().into_owned(),
    };
    let (replayer, resp) = load(&req).unwrap();

    assert_eq!(resp.total_events, 10);
    assert_eq!(resp.total_segments, 3, "checkpoints rotate so 3 segments");
    assert_eq!(resp.total_checkpoints, 2);
    // Build-id always populated; recorded_at None for the
    // hand-built test fixture's manifest.
    assert_eq!(resp.build_id, manifest().build_id);
    assert_eq!(resp.recorded_at, None);

    // The returned replayer is fully usable for follow-up DAP
    // commands — exercise dap_timeline against it to prove.
    let timeline = replayer
        .dap_timeline(&ReplayTimelineRequest::default())
        .unwrap();
    assert_eq!(timeline.total_events, 10);
    assert_eq!(timeline.waypoints.len(), 2);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_load_surfaces_recorded_at_when_present() {
    let dir = temp_dir("load-recorded-at");
    let mut m = manifest();
    m.recorded_at = Some("2026-05-06T10:00:00Z".to_owned());
    {
        let mut writer = bs_replay_driver::engine::format::TraceWriter::create(&dir, &m).unwrap();
        writer
            .write_event(Event::Marker { tag: 0, data: 0 })
            .unwrap();
        writer.finish().unwrap();
    }
    let (_, resp) = load(&ReplayLoadRequest {
        trace_path: dir.to_string_lossy().into_owned(),
    })
    .unwrap();
    assert_eq!(resp.recorded_at, Some("2026-05-06T10:00:00Z".to_owned()));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_load_on_missing_directory_propagates_engine_error() {
    let req = ReplayLoadRequest {
        trace_path: "/nope/this/dir/should/not/exist".to_owned(),
    };
    let err = load(&req).unwrap_err();
    let s = format!("{err}");
    // Engine error path: the inner ManifestIo is what the user sees.
    assert!(
        s.contains("manifest") || s.contains("trace"),
        "unexpected: {s}",
    );
}

#[test]
fn dap_load_summary_matches_reader_native_indices() {
    // Same trace, two ways: load() vs reading TraceReader's index
    // slices directly. They must agree.
    let dir = temp_dir("load-vs-native");
    make_trace_with_two_checkpoints(&dir);
    let req = ReplayLoadRequest {
        trace_path: dir.to_string_lossy().into_owned(),
    };
    let (replayer, resp) = load(&req).unwrap();
    assert_eq!(
        resp.total_segments,
        replayer.reader().segment_indices().len() as u64,
    );
    assert_eq!(
        resp.total_checkpoints,
        replayer.reader().checkpoint_indices().len() as u64,
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dap_timeline_on_empty_trace_is_zero_with_no_waypoints() {
    let dir = temp_dir("timeline-empty");
    {
        let writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let resp = replayer
        .dap_timeline(&ReplayTimelineRequest::default())
        .unwrap();
    assert_eq!(resp.total_events, 0);
    assert!(resp.waypoints.is_empty());
    fs::remove_dir_all(&dir).ok();
}

// -- bs/replayRecord ---------------------------------------------------------

/// Cross-platform: the `record()` handler is gated to Linux but
/// validation errors surface from a pure-Rust pre-flight that
/// runs everywhere — we can't exercise it directly off-Linux
/// without conditional compilation, so the validation suite
/// itself is also Linux-gated. The shapes themselves still
/// compile cross-platform via the unconditional re-exports.
#[cfg(target_os = "linux")]
mod record_handler {
    use super::*;
    use bs_replay_driver::dap::{
        DapRecordError, ReplayRecordExitKind, ReplayRecordOptions, ReplayRecordRequest, record,
    };

    fn req(trace_path: &std::path::Path, argv: Vec<String>) -> ReplayRecordRequest {
        ReplayRecordRequest {
            trace_path: trace_path.to_string_lossy().into_owned(),
            argv,
            envp: vec![],
            build_id: "0".repeat(40),
            kernel_label: Some("dap-test".to_owned()),
            options: ReplayRecordOptions::default(),
        }
    }

    #[test]
    fn empty_argv_errors_before_spawn() {
        let dir = temp_dir("rec-empty-argv");
        let r = req(&dir, vec![]);
        let err = record(&r).unwrap_err();
        assert!(matches!(err, DapRecordError::EmptyArgv), "got: {err}");
        // Trace dir must not have been created — the handler
        // refuses *before* touching the filesystem.
        assert!(!dir.exists(), "trace_path created on validation failure");
    }

    #[test]
    fn argv_with_nul_errors() {
        let dir = temp_dir("rec-argv-nul");
        let r = req(&dir, vec!["/bin/true\0bad".to_owned()]);
        let err = record(&r).unwrap_err();
        assert!(matches!(err, DapRecordError::ArgvNul), "got: {err}");
    }

    #[test]
    fn env_key_with_equals_errors() {
        let dir = temp_dir("rec-env-eq");
        let mut r = req(&dir, vec!["/bin/true".to_owned()]);
        r.envp.push(("BAD=KEY".to_owned(), "v".to_owned()));
        let err = record(&r).unwrap_err();
        assert!(
            matches!(err, DapRecordError::EnvKeyContainsEquals(ref k) if k == "BAD=KEY"),
            "got: {err}",
        );
    }

    /// End-to-end: record `/bin/true` through the DAP handler,
    /// confirm we get a sensible report and a real on-disk trace.
    /// Skips on environments where ptrace is restricted (yama,
    /// containerised CI without CAP_SYS_PTRACE).
    #[test]
    fn record_bin_true_round_trip() {
        let dir = temp_dir("rec-bin-true");
        let r = req(&dir, vec!["/bin/true".to_owned()]);
        let resp = match record(&r) {
            Ok(r) => r,
            Err(e) => {
                let s = format!("{e}");
                if s.contains("EPERM")
                    || s.contains("EACCES")
                    || s.contains("yama")
                    || s.contains("ENOSYS")
                    || s.contains("child setup failed")
                    || s.contains("PTRACE_TRACEME")
                {
                    eprintln!("skipping record_bin_true_round_trip — {s}");
                    return;
                }
                panic!("record failed: {e}");
            }
        };
        assert_eq!(resp.trace_path, dir.to_string_lossy());
        assert!(matches!(
            resp.exit,
            ReplayRecordExitKind::Exited { code: 0 }
        ));
        // /bin/true exits cleanly; we expect at least one
        // syscall (exit_group) and zero instruction-traps with
        // default options.
        assert!(resp.syscall_events >= 1, "no syscalls recorded");
        assert_eq!(resp.instruction_traps, 0, "default options trap nothing");
        assert_eq!(
            resp.events_written,
            resp.pc_marker_events
                + resp.syscall_events
                + resp.signal_events
                + resp.instruction_traps,
        );
        // Trace dir was created and is non-empty.
        assert!(dir.is_dir(), "trace dir wasn't created");
        assert!(
            fs::read_dir(&dir).unwrap().next().is_some(),
            "trace dir is empty",
        );
        fs::remove_dir_all(&dir).ok();
    }
}

// SPDX-License-Identifier: MIT
//! DAP-handler coverage. Each request shape is round-tripped
//! against a real on-disk trace via `TraceReplayer::dap_*`.

use std::fs;
use std::path::PathBuf;

use bs_replay_driver::dap::{
    JumpTarget, ReplayCheckpointListRequest, ReplayJumpRequest, ReplayTimelineRequest,
    TimelineWaypoint,
};
use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use bs_replay_driver::engine::format::TraceWriter;
use bs_replay_driver::TraceReplayer;

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
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"A".to_vec()).unwrap();
    for i in 3..7u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"B".to_vec()).unwrap();
    for i in 7..10u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
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

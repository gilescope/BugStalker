// SPDX-License-Identifier: MIT
//! ReverseDebugger coverage. The Tier 1 navigation surface — step,
//! rstep, run_forward, rcontinue — exercised against a recorded
//! trace.

use std::fs;
use std::path::PathBuf;

use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use bs_replay_driver::engine::format::TraceWriter;
use bs_replay_driver::{ReverseDebugger, TraceReplayer};

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
        "bs-replay-driver-reverse-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Write a trace of N Marker events with tag = i.
fn make_trace(dir: &PathBuf, count: u32) {
    let mut writer = TraceWriter::create(dir, &manifest()).unwrap();
    for i in 0..count {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.finish().unwrap();
}

fn marker_tag(ev: &Event) -> u32 {
    match ev {
        Event::Marker { tag, .. } => *tag,
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn rstep_at_position_zero_returns_none() {
    let dir = temp_dir("rstep-zero");
    make_trace(&dir, 5);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    assert_eq!(rdb.position(), 0);
    assert!(rdb.rstep().unwrap().is_none());
    assert_eq!(rdb.position(), 0);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn step_then_rstep_returns_to_same_event() {
    let dir = temp_dir("step-rstep");
    make_trace(&dir, 5);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);

    let forward = rdb.step().unwrap().unwrap();
    assert_eq!(marker_tag(&forward), 0);
    assert_eq!(rdb.position(), 1);

    let backward = rdb.rstep().unwrap().unwrap();
    assert_eq!(marker_tag(&backward), 0, "rstep yields the same event we stepped past");
    assert_eq!(rdb.position(), 0);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn rstep_walks_backward_through_recorded_sequence() {
    let dir = temp_dir("rstep-walk");
    make_trace(&dir, 5);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);

    // Walk to event 4.
    for _ in 0..5 {
        rdb.step().unwrap();
    }
    assert_eq!(rdb.position(), 5); // past last
    rdb.seek_to(5);

    let mut tags = Vec::new();
    while let Some(ev) = rdb.rstep().unwrap() {
        tags.push(marker_tag(&ev));
    }
    assert_eq!(tags, vec![4, 3, 2, 1, 0]);
    assert_eq!(rdb.position(), 0);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn breakpoints_are_unique_and_sorted() {
    let dir = temp_dir("bp-unique");
    make_trace(&dir, 1);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    rdb.add_breakpoint(5);
    rdb.add_breakpoint(2);
    rdb.add_breakpoint(7);
    rdb.add_breakpoint(2); // duplicate
    assert_eq!(rdb.breakpoints(), &[2, 5, 7]);
    assert!(rdb.remove_breakpoint(5));
    assert!(!rdb.remove_breakpoint(99));
    assert_eq!(rdb.breakpoints(), &[2, 7]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn run_forward_stops_at_first_breakpoint() {
    let dir = temp_dir("run-fwd-bp");
    make_trace(&dir, 10);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    rdb.add_breakpoint(3);
    rdb.add_breakpoint(7);
    let stopped_at = rdb.run_forward().unwrap();
    assert_eq!(stopped_at, 3, "first hit wins");
    assert_eq!(rdb.position(), 3);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn run_forward_walks_to_end_when_no_breakpoint_hits() {
    let dir = temp_dir("run-fwd-end");
    make_trace(&dir, 4);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    let stopped_at = rdb.run_forward().unwrap();
    assert_eq!(stopped_at, 4);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn rcontinue_jumps_to_latest_breakpoint_before_position() {
    let dir = temp_dir("rcont-bp");
    make_trace(&dir, 10);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    rdb.add_breakpoint(2);
    rdb.add_breakpoint(7);
    rdb.seek_to(9);
    let stopped = rdb.rcontinue().unwrap();
    assert_eq!(stopped, 7, "latest breakpoint before pos=9 is 7");
    // Run again from there — should jump to 2.
    let stopped = rdb.rcontinue().unwrap();
    assert_eq!(stopped, 2);
    // One more — no breakpoint left before pos=2, so we land at 0.
    let stopped = rdb.rcontinue().unwrap();
    assert_eq!(stopped, 0);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn rcontinue_with_no_breakpoints_walks_to_zero() {
    let dir = temp_dir("rcont-empty");
    make_trace(&dir, 5);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    rdb.seek_to(4);
    let stopped = rdb.rcontinue().unwrap();
    assert_eq!(stopped, 0);
    assert_eq!(rdb.position(), 0);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn run_forward_does_not_re_hit_current_breakpoint() {
    // If the playhead is exactly on a breakpoint, run_forward
    // should leave it (just stopped there, natural next move is
    // to advance past).
    let dir = temp_dir("no-rehit");
    make_trace(&dir, 10);
    let replayer = TraceReplayer::open(&dir).unwrap();
    let mut rdb = ReverseDebugger::new(replayer);
    rdb.add_breakpoint(3);
    rdb.add_breakpoint(7);
    rdb.seek_to(3);
    let stopped = rdb.run_forward().unwrap();
    assert_eq!(stopped, 7, "should pass over current bp at 3 and stop at 7");
    fs::remove_dir_all(&dir).ok();
}

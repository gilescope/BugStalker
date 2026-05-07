// SPDX-License-Identifier: MIT
//! Replay-seek query API coverage. Tests routing of arbitrary event
//! indices to the segment that holds them, and "find the latest
//! checkpoint at or before this event index" — the two queries a
//! future replay driver issues to seek inside a trace.

use std::fs;
use std::path::PathBuf;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{TraceReader, TraceWriter};

fn manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "ab".repeat(32),
        kernel_release: "test".to_owned(),
        cpu_features: vec!["sse2".into()],
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
        "bs-replay-engine-seek-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Trace shape: 4 segments with event counts [3, 2, 5, 1].
/// Global event indices: seg1=[0,3), seg2=[3,5), seg3=[5,10), seg4=[10,11).
fn make_4_segment_trace(dir: &PathBuf) {
    let mut writer = TraceWriter::create(dir, &manifest()).unwrap();
    for i in 0..3u32 {
        writer.write_event(Event::Marker { tag: i, data: 1 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 3..5u32 {
        writer.write_event(Event::Marker { tag: i, data: 2 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 5..10u32 {
        writer.write_event(Event::Marker { tag: i, data: 3 }).unwrap();
    }
    writer.rotate().unwrap();
    writer.write_event(Event::Marker { tag: 10, data: 4 }).unwrap();
    writer.finish().unwrap();
}

#[test]
fn segment_event_ranges_walks_every_segment() {
    let dir = temp_dir("ranges");
    make_4_segment_trace(&dir);

    let reader = TraceReader::open(&dir).unwrap();
    let ranges = reader.segment_event_ranges().unwrap();

    assert_eq!(ranges.len(), 4);
    assert_eq!(ranges[0].segment_index, 1);
    assert_eq!(ranges[0].first_event_index, 0);
    assert_eq!(ranges[0].event_count, 3);
    assert_eq!(ranges[1].first_event_index, 3);
    assert_eq!(ranges[1].event_count, 2);
    assert_eq!(ranges[2].first_event_index, 5);
    assert_eq!(ranges[2].event_count, 5);
    assert_eq!(ranges[3].first_event_index, 10);
    assert_eq!(ranges[3].event_count, 1);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_event_ranges_is_cached_across_calls() {
    let dir = temp_dir("cached");
    make_4_segment_trace(&dir);

    let reader = TraceReader::open(&dir).unwrap();
    let r1 = reader.segment_event_ranges().unwrap().as_ptr();
    let r2 = reader.segment_event_ranges().unwrap().as_ptr();
    assert_eq!(r1, r2, "second call should return the same cached slice");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_for_event_routes_to_the_right_segment() {
    let dir = temp_dir("routing");
    make_4_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();

    // First event of seg1.
    assert_eq!(reader.segment_for_event(0).unwrap(), Some((1, 0)));
    // Mid seg1.
    assert_eq!(reader.segment_for_event(2).unwrap(), Some((1, 2)));
    // Boundary: first event of seg2.
    assert_eq!(reader.segment_for_event(3).unwrap(), Some((2, 0)));
    // Mid seg3.
    assert_eq!(reader.segment_for_event(7).unwrap(), Some((3, 2)));
    // Last event of seg3.
    assert_eq!(reader.segment_for_event(9).unwrap(), Some((3, 4)));
    // Single event of seg4.
    assert_eq!(reader.segment_for_event(10).unwrap(), Some((4, 0)));
    // Past end.
    assert_eq!(reader.segment_for_event(11).unwrap(), None);
    assert_eq!(reader.segment_for_event(u64::MAX).unwrap(), None);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_for_event_on_empty_trace_returns_none() {
    let dir = temp_dir("empty");
    let writer = TraceWriter::create(&dir, &manifest()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_for_event(0).unwrap(), None);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn find_checkpoint_at_or_before_picks_the_latest_qualifying() {
    // Trace shape: events 0..2, checkpoint A (at event_index=2),
    // events 2..7, checkpoint B (at event_index=7), events 7..9,
    // checkpoint C (at event_index=9).
    let dir = temp_dir("checkpoints");
    let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
    for i in 0..2u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"A".to_vec()).unwrap();
    for i in 2..7u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"B".to_vec()).unwrap();
    for i in 7..9u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"C".to_vec()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();

    // Target before any checkpoint exists.
    assert_eq!(
        reader.find_checkpoint_at_or_before(0).unwrap().map(|h| h.index),
        None,
    );
    assert_eq!(
        reader.find_checkpoint_at_or_before(1).unwrap().map(|h| h.index),
        None,
    );
    // Target == checkpoint A's event_index → A.
    assert_eq!(
        reader.find_checkpoint_at_or_before(2).unwrap().map(|h| h.index),
        Some(1),
    );
    // Target after A but before B → A.
    assert_eq!(
        reader.find_checkpoint_at_or_before(5).unwrap().map(|h| h.index),
        Some(1),
    );
    // Target == B → B.
    assert_eq!(
        reader.find_checkpoint_at_or_before(7).unwrap().map(|h| h.index),
        Some(2),
    );
    // Target after C → C (latest).
    assert_eq!(
        reader.find_checkpoint_at_or_before(100).unwrap().map(|h| h.index),
        Some(3),
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn find_checkpoint_returns_event_index_pointer() {
    let dir = temp_dir("event-index-ptr");
    let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
    for i in 0..4u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"snap".to_vec()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let h = reader.find_checkpoint_at_or_before(10).unwrap().unwrap();
    assert_eq!(h.event_index, 4, "checkpoint should mark events 0..4 as captured");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkpoint_headers_is_cached() {
    let dir = temp_dir("hdr-cache");
    let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
    writer.take_checkpoint(b"a".to_vec()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let h1 = reader.checkpoint_headers().unwrap().as_ptr();
    let h2 = reader.checkpoint_headers().unwrap().as_ptr();
    assert_eq!(h1, h2, "second call should return the cached slice");

    fs::remove_dir_all(&dir).ok();
}

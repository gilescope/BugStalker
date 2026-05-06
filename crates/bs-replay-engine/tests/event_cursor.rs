// SPDX-License-Identifier: MIT
//! EventCursor coverage. Tests sequential walk + seek across
//! segment boundaries.

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
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-cursor-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// 11 events spread across 3 segments: counts [4, 5, 2].
fn make_3_segment_trace(dir: &PathBuf) {
    let mut writer = TraceWriter::create(dir, &manifest()).unwrap();
    for i in 0..4u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 4..9u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 9..11u32 {
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
fn cursor_walks_every_event_in_record_order() {
    let dir = temp_dir("walk-all");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor();

    let mut tags = Vec::new();
    while let Some(ev) = cursor.next().unwrap() {
        tags.push(marker_tag(&ev));
    }
    assert_eq!(tags, (0..11u32).collect::<Vec<_>>());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_at_starts_from_arbitrary_index() {
    let dir = temp_dir("start-mid");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();

    // Start at event 6 (mid-segment-2) and walk to end.
    let mut cursor = reader.cursor_at(6);
    let mut tags = Vec::new();
    while let Some(ev) = cursor.next().unwrap() {
        tags.push(marker_tag(&ev));
    }
    assert_eq!(tags, (6..11u32).collect::<Vec<_>>());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_position_advances_per_next() {
    let dir = temp_dir("position");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor();

    assert_eq!(cursor.position(), 0);
    cursor.next().unwrap();
    assert_eq!(cursor.position(), 1);
    cursor.next().unwrap();
    cursor.next().unwrap();
    assert_eq!(cursor.position(), 3);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_crosses_segment_boundary() {
    let dir = temp_dir("boundary");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    // Start at event 3 (last of seg1) and read 3 → forces a
    // crossing into seg2 between two .next() calls.
    let mut cursor = reader.cursor_at(3);
    let e3 = marker_tag(&cursor.next().unwrap().unwrap());
    let e4 = marker_tag(&cursor.next().unwrap().unwrap());
    let e5 = marker_tag(&cursor.next().unwrap().unwrap());
    assert_eq!(e3, 3);
    assert_eq!(e4, 4); // first event of seg2
    assert_eq!(e5, 5);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_past_end_returns_none() {
    let dir = temp_dir("past-end");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor_at(11); // one past last
    assert!(cursor.next().unwrap().is_none());

    let mut far = reader.cursor_at(u64::MAX);
    assert!(far.next().unwrap().is_none());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_seek_to_within_loaded_segment_keeps_buffer() {
    // After loading seg2, seek to another event in seg2 — the
    // cursor should *not* drop the cached SegmentReader. This is
    // observable via behaviour: subsequent next() must still
    // produce correct events, which is the only contract that
    // matters externally.
    let dir = temp_dir("seek-within");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor_at(4);

    let _ = cursor.next().unwrap(); // loads seg2, returns event 4
    cursor.seek_to(7); // still in seg2 (4..9)
    let e7 = marker_tag(&cursor.next().unwrap().unwrap());
    assert_eq!(e7, 7);
    let e8 = marker_tag(&cursor.next().unwrap().unwrap());
    assert_eq!(e8, 8);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_seek_to_other_segment_reloads() {
    let dir = temp_dir("seek-cross");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor();

    cursor.next().unwrap(); // loads seg1
    cursor.seek_to(9);       // jumps to seg3
    let e9 = marker_tag(&cursor.next().unwrap().unwrap());
    let e10 = marker_tag(&cursor.next().unwrap().unwrap());
    assert_eq!(e9, 9);
    assert_eq!(e10, 10);
    assert!(cursor.next().unwrap().is_none()); // past end

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_seek_backwards_works() {
    let dir = temp_dir("seek-back");
    make_3_segment_trace(&dir);
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor_at(7);

    let _ = cursor.next().unwrap(); // loads seg2
    cursor.seek_to(0); // back to seg1
    let e0 = marker_tag(&cursor.next().unwrap().unwrap());
    assert_eq!(e0, 0);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_trace_cursor_returns_none_immediately() {
    let dir = temp_dir("empty");
    let writer = TraceWriter::create(&dir, &manifest()).unwrap();
    writer.finish().unwrap();
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor();
    assert!(cursor.next().unwrap().is_none());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn cursor_yields_owned_syscall_events_too() {
    let dir = temp_dir("syscall");
    let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
    writer
        .write_event(Event::Syscall {
            nr: 0,
            args: [1, 2, 3, 4, 5, 6],
            result: 42,
            output: vec![0xaa, 0xbb],
        })
        .unwrap();
    writer.finish().unwrap();
    let reader = TraceReader::open(&dir).unwrap();
    let mut cursor = reader.cursor();
    match cursor.next().unwrap().unwrap() {
        Event::Syscall { nr, result, output, .. } => {
            assert_eq!(nr, 0);
            assert_eq!(result, 42);
            assert_eq!(output, vec![0xaa, 0xbb]);
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(cursor.next().unwrap().is_none());

    fs::remove_dir_all(&dir).ok();
}

// SPDX-License-Identifier: MIT
//! End-to-end test for the sub-phase 3A trace format: write events
//! through `TraceWriter`, close, re-open through `TraceReader`,
//! verify the events come back identical and zero-copy access works.

use std::fs;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{TraceReader, TraceWriter};

fn sample_manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "6.6.42-test".to_owned(),
        cpu_features: vec!["sse2".into(), "sse4_2".into()],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![("PATH".into(), "/usr/bin".into())],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec!["--probe".into()],
    }
}

fn temp_trace_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-test-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn roundtrip_one_segment_one_hundred_events() {
    let dir = temp_trace_dir("one-segment");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for i in 0..100u32 {
        writer
            .write_event(Event::Marker { tag: i, data: u64::from(i) * 7 + 1 })
            .unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.manifest().format_version, FormatVersion::V1);
    assert_eq!(reader.manifest().build_id, manifest.build_id);
    assert_eq!(reader.segment_indices(), &[1]);

    let segment = reader.open_segment(1).unwrap();
    let owned = segment.events_owned().unwrap();
    assert_eq!(owned.len(), 100);
    for (i, ev) in owned.iter().enumerate() {
        match ev {
            Event::Marker { tag, data } => {
                assert_eq!(*tag, i as u32);
                assert_eq!(*data, (i as u64) * 7 + 1);
            }
        }
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn roundtrip_multiple_segments_via_explicit_rotate() {
    let dir = temp_trace_dir("multi-segment");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for i in 0..10u32 {
        writer.write_event(Event::Marker { tag: i, data: 1 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 10..25u32 {
        writer.write_event(Event::Marker { tag: i, data: 2 }).unwrap();
    }
    writer.rotate().unwrap();
    for i in 25..27u32 {
        writer.write_event(Event::Marker { tag: i, data: 3 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[1, 2, 3]);

    let lengths_and_data: Vec<(usize, u64)> = reader
        .segment_indices()
        .iter()
        .map(|&idx| {
            let seg = reader.open_segment(idx).unwrap();
            let evs = seg.events_owned().unwrap();
            let data = match evs[0] {
                Event::Marker { data, .. } => data,
            };
            (evs.len(), data)
        })
        .collect();
    assert_eq!(lengths_and_data, vec![(10, 1), (15, 2), (2, 3)]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_archived_view_is_zero_copy() {
    use rkyv::vec::ArchivedVec;
    let dir = temp_trace_dir("zero-copy");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 1, data: 11 }).unwrap();
    writer.write_event(Event::Marker { tag: 2, data: 22 }).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let segment = reader.open_segment(1).unwrap();
    let archived: &ArchivedVec<_> = segment.events().unwrap();
    assert_eq!(archived.len(), 2);
    // Walk the archived view directly — the rkyv enum exposes its
    // discriminant + fields without ever materialising owned `Event`
    // values. This is the access pattern that motivates the rkyv
    // choice over speedy/bincode/borsh.
    use bs_replay_engine::format::event::ArchivedEvent;
    match &archived[0] {
        ArchivedEvent::Marker { tag, data } => {
            assert_eq!(tag.to_native(), 1);
            assert_eq!(data.to_native(), 11);
        }
    }
    match &archived[1] {
        ArchivedEvent::Marker { tag, data } => {
            assert_eq!(tag.to_native(), 2);
            assert_eq!(data.to_native(), 22);
        }
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_refuses_existing_directory() {
    let dir = temp_trace_dir("existing");
    fs::create_dir(&dir).unwrap();

    let manifest = sample_manifest();
    let err = TraceWriter::create(&dir, &manifest).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("I/O") || s.contains("exists"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_rotates_when_estimated_size_exceeds_threshold() {
    // One Marker is conservatively sized at 28 bytes by
    // approx_archive_size. Setting the threshold to 56 bytes means
    // every two events triggers a rotation: 5 events → 3 segments
    // (2, 2, 1).
    let dir = temp_trace_dir("auto-rotate");
    let manifest = sample_manifest();

    let mut writer = TraceWriter::create(&dir, &manifest)
        .unwrap()
        .with_segment_size(56);
    for i in 0..5u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[1, 2, 3]);
    let counts: Vec<usize> = reader
        .segment_indices()
        .iter()
        .map(|&i| reader.open_segment(i).unwrap().events_owned().unwrap().len())
        .collect();
    assert_eq!(counts, vec![2, 2, 1]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_header_index_mismatch_is_caught() {
    // Write a single segment, then rename event-000001.lz4 to
    // event-000099.lz4. The header still says index=1, the filename
    // now claims 99 — open_segment(99) must surface HeaderMismatch.
    let dir = temp_trace_dir("hdr-mismatch");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    writer.finish().unwrap();
    fs::rename(dir.join("event-000001.lz4"), dir.join("event-000099.lz4"))
        .unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[99]);
    let err = reader.open_segment(99).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("file/header disagree"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_reader_exposes_header() {
    let dir = temp_trace_dir("hdr-expose");
    let manifest = sample_manifest();
    let mut writer = TraceWriter::create(&dir, &manifest).unwrap();
    for _ in 0..3 {
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    }
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let seg = reader.open_segment(1).unwrap();
    let hdr = seg.header().unwrap();
    assert_eq!(hdr.index.to_native(), 1);
    assert_eq!(hdr.event_count.to_native(), 3);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_with_missing_manifest_fails_clearly() {
    let dir = temp_trace_dir("no-manifest");
    fs::create_dir(&dir).unwrap();

    let err = TraceReader::open(&dir).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("manifest"), "got: {s}");

    fs::remove_dir_all(&dir).ok();
}

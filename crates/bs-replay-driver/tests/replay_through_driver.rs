// SPDX-License-Identifier: MIT
//! End-to-end integration: write a trace via the engine, drive
//! replay through the driver, assert the seen event sequence is
//! identical. This is the smallest meaningful test of the
//! integration seam — the engine and the driver agree on the
//! contract.

use std::fs;
use std::path::PathBuf;

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
        cpu_features: vec!["sse2".into()],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec![],
        recorded_at: None,
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-driver-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn marker_tag(ev: &Event) -> u32 {
    match ev {
        Event::Marker { tag, .. } => *tag,
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn driver_walks_full_trace_in_record_order() {
    let dir = temp_dir("walk");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        for i in 0..15u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
            if i == 4 || i == 9 {
                writer.rotate().unwrap();
            }
        }
        writer.finish().unwrap();
    }
    let mut replayer = TraceReplayer::open(&dir).unwrap();
    assert_eq!(replayer.position(), 0);
    let mut tags = Vec::new();
    while let Some(ev) = replayer.next_event().unwrap() {
        tags.push(marker_tag(&ev));
    }
    assert_eq!(tags, (0..15u32).collect::<Vec<_>>());
    assert_eq!(replayer.position(), 15);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_seek_to_jumps_without_yielding() {
    let dir = temp_dir("seek");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        for i in 0..10u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.finish().unwrap();
    }
    let mut replayer = TraceReplayer::open(&dir).unwrap();
    replayer.seek_to(7);
    let ev = replayer.next_event().unwrap().unwrap();
    assert_eq!(marker_tag(&ev), 7);
    assert_eq!(replayer.position(), 8);
    // Past-end seek lets next_event return None cleanly.
    replayer.seek_to(100);
    assert!(replayer.next_event().unwrap().is_none());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_finds_checkpoint_at_or_before_target() {
    let dir = temp_dir("checkpoint");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        for i in 0..3u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.take_checkpoint(b"snap-A".to_vec()).unwrap();
        for i in 3..8u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.take_checkpoint(b"snap-B".to_vec()).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let h = replayer.find_checkpoint_at_or_before(0).unwrap();
    assert!(h.is_none(), "no checkpoint covers event 0");
    let h = replayer.find_checkpoint_at_or_before(3).unwrap().unwrap();
    assert_eq!(h.index, 1);
    let h = replayer.find_checkpoint_at_or_before(50).unwrap().unwrap();
    assert_eq!(h.index, 2, "latest checkpoint wins");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_position_advances_per_next_event_only() {
    let dir = temp_dir("position");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
        writer.write_event(Event::Marker { tag: 1, data: 0 }).unwrap();
        writer.finish().unwrap();
    }
    let mut replayer = TraceReplayer::open(&dir).unwrap();
    assert_eq!(replayer.position(), 0);
    replayer.next_event().unwrap();
    assert_eq!(replayer.position(), 1);
    // next_event past end does NOT bump the counter.
    replayer.seek_to(99);
    assert_eq!(replayer.position(), 99);
    let _ = replayer.next_event().unwrap();
    assert_eq!(replayer.position(), 99, "past-end next() must not advance");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_exposes_underlying_reader_for_advanced_queries() {
    let dir = temp_dir("reader-borrow");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
        writer.rotate().unwrap();
        writer.write_event(Event::Marker { tag: 1, data: 0 }).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    assert_eq!(replayer.reader().segment_indices(), &[1, 2]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_host_compatibility_passes_when_host_is_superset() {
    let dir = temp_dir("host-ok");
    let mut m = manifest();
    m.cpu_features = vec!["sse2".into(), "avx".into()];
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let host = ["sse2", "sse4_2", "avx", "avx2"]; // superset
    replayer.check_host_compatibility(&host).unwrap();
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_host_compatibility_fails_with_missing_listed() {
    let dir = temp_dir("host-bad");
    let mut m = manifest();
    m.cpu_features = vec!["sse2".into(), "avx".into(), "avx2".into()];
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let host = ["sse2"]; // host lacks avx and avx2
    let err = replayer.check_host_compatibility(&host).unwrap_err();
    assert_eq!(err.missing, vec!["avx".to_owned(), "avx2".to_owned()]);
    let s = format!("{err}");
    assert!(s.contains("avx"), "got: {s}");
    assert!(s.contains("avx2"), "got: {s}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_host_compatibility_passes_when_recording_named_no_features() {
    let dir = temp_dir("host-empty");
    let mut m = manifest();
    m.cpu_features.clear();
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let host: [&str; 0] = [];
    replayer.check_host_compatibility(&host).unwrap();
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_build_id_passes_on_match() {
    let dir = temp_dir("build-id-ok");
    let m = manifest();
    let recorded = m.build_id.clone();
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    replayer.check_build_id(&recorded).unwrap();
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_build_id_surfaces_both_sides() {
    let dir = temp_dir("build-id-bad");
    let m = manifest();
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    let err = replayer
        .check_build_id("00000000")
        .unwrap_err();
    assert_eq!(err.recorded, m.build_id);
    assert_eq!(err.actual, "00000000");
    let s = format!("{err}");
    assert!(s.contains(&m.build_id), "got: {s}");
    assert!(s.contains("00000000"), "got: {s}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_check_replayability_runs_both_checks_in_stable_order() {
    use bs_replay_driver::ReplayabilityError;

    let dir = temp_dir("replayability");
    let mut m = manifest();
    m.cpu_features = vec!["sse2".into()];
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();

    // Both wrong → build-id error wins (stable order).
    let host_features: [&str; 0] = [];
    let err = replayer
        .check_replayability(&host_features, Some("00000000"))
        .unwrap_err();
    match err {
        ReplayabilityError::BuildId(_) => { /* expected: build-id checked first */ }
        other => panic!("expected BuildId variant, got {other:?}"),
    }

    // Build-id right, features wrong → HostFeatures error.
    let err = replayer
        .check_replayability(&host_features, Some(&m.build_id))
        .unwrap_err();
    match err {
        ReplayabilityError::HostFeatures(_) => {}
        other => panic!("expected HostFeatures variant, got {other:?}"),
    }

    // Both right → Ok.
    let host = ["sse2", "avx"];
    replayer
        .check_replayability(&host, Some(&m.build_id))
        .unwrap();

    // No build-id supplied → only feature check runs.
    replayer.check_replayability(&host, None).unwrap();

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_manifest_round_trips_through_open() {
    let dir = temp_dir("manifest");
    let m = manifest();
    {
        let writer = TraceWriter::create(&dir, &m).unwrap();
        writer.finish().unwrap();
    }
    let replayer = TraceReplayer::open(&dir).unwrap();
    assert_eq!(replayer.manifest().build_id, m.build_id);
    assert_eq!(replayer.manifest().format_version, FormatVersion::V1);
    fs::remove_dir_all(&dir).ok();
}

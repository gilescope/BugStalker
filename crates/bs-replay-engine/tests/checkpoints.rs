// SPDX-License-Identifier: MIT
//! Trace-internal checkpoint coverage. Distinct from Tier 2
//! fork-checkpoints; these are snapshot files embedded inside the
//! trace dir so replay can fast-forward without scanning from
//! event 0.

use std::fs;
use std::path::PathBuf;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{DiagKind, TraceReader, TraceWriter, validate};

fn sample_manifest() -> Manifest {
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
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-checkpoint-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn take_checkpoint_records_event_offset_and_increments_index() {
    let dir = temp_dir("offset-and-index");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();

    for i in 0..3u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"snapshot-A".to_vec()).unwrap();
    for i in 3..7u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    writer.take_checkpoint(b"snapshot-B".to_vec()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.checkpoint_indices(), &[1, 2]);

    let cp1 = reader.open_checkpoint(1).unwrap();
    assert_eq!(cp1.header.index, 1);
    assert_eq!(cp1.header.event_index, 3, "checkpoint 1 captured 3 events");
    assert_eq!(cp1.payload, b"snapshot-A");

    let cp2 = reader.open_checkpoint(2).unwrap();
    assert_eq!(cp2.header.index, 2);
    assert_eq!(cp2.header.event_index, 7, "checkpoint 2 captured 7 events");
    assert_eq!(cp2.payload, b"snapshot-B");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn take_checkpoint_forces_segment_rotation() {
    // Without forcing rotation, events 0..2 would still be in the
    // writer's pending buffer and the checkpoint's `event_index`
    // would mark events that aren't yet on disk. `take_checkpoint`
    // is contracted to flush first.
    let dir = temp_dir("forces-rotate");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    for i in 0..2u32 {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
    }
    let segment_idx_before = writer.next_segment_index();
    writer.take_checkpoint(Vec::new()).unwrap();
    let segment_idx_after = writer.next_segment_index();
    writer.finish().unwrap();
    assert_eq!(segment_idx_before, 1);
    assert_eq!(segment_idx_after, 2, "take_checkpoint should rotate first");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkpoint_payload_is_opaque_bytes() {
    let dir = temp_dir("opaque");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    let blob: Vec<u8> = (0..=255).collect();
    writer.take_checkpoint(blob.clone()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    let cp = reader.open_checkpoint(1).unwrap();
    assert_eq!(cp.payload, blob);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validator_reports_total_checkpoints() {
    let dir = temp_dir("validator-total");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    writer.take_checkpoint(b"a".to_vec()).unwrap();
    writer.take_checkpoint(b"b".to_vec()).unwrap();
    writer.take_checkpoint(b"c".to_vec()).unwrap();
    writer.finish().unwrap();

    let report = validate(&dir);
    assert!(report.is_replayable());
    let total = report
        .info
        .iter()
        .find(|d| d.kind == DiagKind::TotalCheckpoints)
        .expect("missing total-checkpoints diag");
    assert_eq!(total.message, "3");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validator_catches_renamed_checkpoint_header_mismatch() {
    let dir = temp_dir("hdr-mismatch");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    writer.take_checkpoint(b"x".to_vec()).unwrap();
    writer.finish().unwrap();
    fs::rename(
        dir.join("checkpoint-000001.snap"),
        dir.join("checkpoint-000099.snap"),
    )
    .unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    let diag = report
        .errors
        .iter()
        .find(|d| d.kind == DiagKind::CheckpointHeaderIndexMismatch)
        .expect("missing checkpoint-header-mismatch diag");
    assert!(diag.message.contains("99"), "got: {}", diag.message);
    assert!(diag.message.contains("1"), "got: {}", diag.message);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkpoint_filename_does_not_register_as_segment() {
    // Defensive: the segment-filename parser must not match a
    // checkpoint filename. (Distinct prefixes, but bugs happen.)
    use bs_replay_engine::format::checkpoint::parse_checkpoint_filename;

    let dir = temp_dir("no-cross-talk");
    let mut writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
    writer.take_checkpoint(b"x".to_vec()).unwrap();
    writer.finish().unwrap();

    let reader = TraceReader::open(&dir).unwrap();
    assert_eq!(reader.segment_indices(), &[1]);
    assert_eq!(reader.checkpoint_indices(), &[1]);
    assert_eq!(parse_checkpoint_filename("event-000001.lz4"), None);

    fs::remove_dir_all(&dir).ok();
}

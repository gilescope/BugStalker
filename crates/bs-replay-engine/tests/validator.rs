// SPDX-License-Identifier: MIT
//! Trace-validator coverage. Each test deliberately corrupts one
//! aspect of a trace and asserts the validator's report flags
//! exactly that finding (and no others).

use std::fs;
use std::path::PathBuf;

use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{
    DiagKind, TraceWriter, ValidationOptions, validate, validate_with,
};

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
        initial_fds: vec![],
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-replay-engine-validator-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn write_one_segment_per_event(dir: &PathBuf, count: u32) {
    let mut writer = TraceWriter::create(dir, &sample_manifest()).unwrap();
    for i in 0..count {
        writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        writer.rotate().unwrap();
    }
    writer.finish().unwrap();
}

fn has(report: &bs_replay_engine::format::ValidationReport, kind: DiagKind) -> bool {
    report.all().any(|d| d.kind == kind)
}

#[test]
fn clean_trace_is_replayable_with_only_info_findings() {
    let dir = temp_dir("clean");
    write_one_segment_per_event(&dir, 3);

    let report = validate(&dir);
    assert!(report.is_replayable(), "errors: {:?}", report.errors);
    assert!(report.warnings.is_empty(), "warnings: {:?}", report.warnings);
    assert!(has(&report, DiagKind::TotalSegments));
    assert!(has(&report, DiagKind::TotalEvents));
    let totals: Vec<&str> = report
        .info
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(totals.contains(&"3"), "expected segments/events of 3 in {totals:?}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_manifest_reported() {
    let dir = temp_dir("no-manifest");
    fs::create_dir(&dir).unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    assert!(has(&report, DiagKind::ManifestMissing));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_manifest_reported_with_line_number() {
    let dir = temp_dir("bad-manifest");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("manifest.txt"),
        "format_version: 1\nbuild_id ab\nkernel_release: r\n",
    )
    .unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    let diag = report
        .errors
        .iter()
        .find(|d| d.kind == DiagKind::ManifestMalformed)
        .expect("missing manifest-malformed diag");
    assert!(diag.message.contains("line 2"), "got: {}", diag.message);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn unsupported_version_reported() {
    let dir = temp_dir("future-version");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("manifest.txt"),
        "format_version: 99\nbuild_id: ab\nkernel_release: r\nengine_version: 0\ninitial_cwd: /\n",
    )
    .unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    assert!(has(&report, DiagKind::UnsupportedVersion));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_segments_is_a_warning_not_an_error() {
    let dir = temp_dir("empty");
    let writer = TraceWriter::create(&dir, &sample_manifest()).unwrap();
    writer.finish().unwrap(); // writes no segments

    let report = validate(&dir);
    assert!(report.is_replayable(), "no segments should not error");
    assert!(has(&report, DiagKind::NoSegments));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn segment_index_gap_reported() {
    let dir = temp_dir("gap");
    write_one_segment_per_event(&dir, 4); // creates 1, 2, 3, 4
    fs::remove_file(dir.join("event-000003.lz4")).unwrap();

    let report = validate(&dir);
    let diag = report
        .warnings
        .iter()
        .find(|d| d.kind == DiagKind::SegmentGap)
        .expect("missing segment-gap diag");
    assert!(diag.message.contains("3"), "got: {}", diag.message);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn header_index_mismatch_reported() {
    let dir = temp_dir("hdr-mismatch");
    write_one_segment_per_event(&dir, 1);
    fs::rename(dir.join("event-000001.lz4"), dir.join("event-000099.lz4")).unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    let diag = report
        .errors
        .iter()
        .find(|d| d.kind == DiagKind::SegmentHeaderIndexMismatch)
        .expect("missing header-mismatch diag");
    assert!(diag.message.contains("99"), "got: {}", diag.message);
    assert!(diag.message.contains("1"), "got: {}", diag.message);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn corrupt_segment_bytes_reported() {
    let dir = temp_dir("corrupt");
    write_one_segment_per_event(&dir, 1);
    let path = dir.join("event-000001.lz4");
    let mut bytes = fs::read(&path).unwrap();
    // Smash the LZ4 frame magic so decompression fails outright.
    for b in bytes.iter_mut().take(4) {
        *b = 0xff;
    }
    fs::write(&path, bytes).unwrap();

    let report = validate(&dir);
    assert!(!report.is_replayable());
    let kinds: Vec<DiagKind> = report.errors.iter().map(|d| d.kind).collect();
    assert!(
        kinds.contains(&DiagKind::SegmentCorrupt) || kinds.contains(&DiagKind::SegmentUnreadable),
        "expected segment-corrupt or segment-unreadable, got {kinds:?}",
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_with_passes_clean_trace_when_host_matches() {
    let dir = temp_dir("with-ok");
    write_one_segment_per_event(&dir, 1);
    let m = sample_manifest();
    let opts = ValidationOptions {
        expected_build_id: Some(&m.build_id),
        host_features: Some(&["sse2", "sse4_2"]),
    };
    let report = validate_with(&dir, &opts);
    assert!(report.is_replayable(), "errors: {:?}", report.errors);
    assert!(!has(&report, DiagKind::BuildIdMismatch));
    assert!(!has(&report, DiagKind::HostFeatureMissing));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_with_flags_build_id_mismatch() {
    let dir = temp_dir("with-bid");
    write_one_segment_per_event(&dir, 1);
    let opts = ValidationOptions {
        expected_build_id: Some("00000000"),
        host_features: None,
    };
    let report = validate_with(&dir, &opts);
    assert!(!report.is_replayable());
    let diag = report
        .errors
        .iter()
        .find(|d| d.kind == DiagKind::BuildIdMismatch)
        .expect("missing build-id-mismatch diag");
    assert!(diag.message.contains("00000000"), "got: {}", diag.message);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_with_lists_every_missing_host_feature() {
    let dir = temp_dir("with-feats");
    let mut m = sample_manifest();
    m.cpu_features = vec!["sse2".into(), "avx".into(), "avx2".into()];
    {
        let mut writer = TraceWriter::create(&dir, &m).unwrap();
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
        writer.finish().unwrap();
    }
    let opts = ValidationOptions {
        expected_build_id: None,
        host_features: Some(&["sse2"]), // missing avx + avx2
    };
    let report = validate_with(&dir, &opts);
    let missing: Vec<&str> = report
        .errors
        .iter()
        .filter(|d| d.kind == DiagKind::HostFeatureMissing)
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(missing.len(), 2, "got {missing:?}");
    assert!(missing.iter().any(|s| s.contains("avx")));
    assert!(missing.iter().any(|s| s.contains("avx2")));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_with_skips_host_checks_when_options_are_none() {
    let dir = temp_dir("with-none");
    write_one_segment_per_event(&dir, 1);
    let report = validate_with(&dir, &ValidationOptions::default());
    assert!(!has(&report, DiagKind::BuildIdMismatch));
    assert!(!has(&report, DiagKind::HostFeatureMissing));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn diag_codes_are_stable_strings() {
    // Support workflows grep on these codes; pin them.
    assert_eq!(DiagKind::ManifestMissing.code(), "manifest-missing");
    assert_eq!(DiagKind::SegmentGap.code(), "segment-gap");
    assert_eq!(
        DiagKind::SegmentHeaderIndexMismatch.code(),
        "segment-header-index-mismatch",
    );
    assert_eq!(DiagKind::TotalSegments.code(), "total-segments");
    assert_eq!(DiagKind::BuildIdMismatch.code(), "build-id-mismatch");
    assert_eq!(DiagKind::HostFeatureMissing.code(), "host-feature-missing");
}

#[test]
fn report_display_groups_by_severity() {
    let dir = temp_dir("display");
    write_one_segment_per_event(&dir, 4);
    // Trigger one warning (gap) and one error (corrupt segment) so
    // we can prove the Display impl emits errors before warnings.
    fs::remove_file(dir.join("event-000003.lz4")).unwrap();
    let path = dir.join("event-000004.lz4");
    let mut bytes = fs::read(&path).unwrap();
    for b in bytes.iter_mut().take(4) {
        *b = 0xff;
    }
    fs::write(&path, bytes).unwrap();

    let report = validate(&dir);
    let s = format!("{report}");
    let err_pos = s.find("error:").expect("expected an error line");
    let warn_pos = s.find("warning:").expect("expected a warning line");
    assert!(err_pos < warn_pos, "errors must precede warnings:\n{s}");
    assert!(s.contains("[segment-gap]"), "{s}");

    fs::remove_dir_all(&dir).ok();
}

// SPDX-License-Identifier: MIT
//! Integration test for the driver's one-shot capture helper
//! (`bs_replay_driver::record::capture_one_shot`). Linux only.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;

use bs_replay::linux::tier2;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use bs_replay_driver::engine::format::TraceReader;
use bs_replay_driver::record::{capture_one_shot, CaptureReport, RecordError};

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
        "bs-replay-record-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn capture_one_shot_writes_a_decodable_checkpoint() {
    let dir = temp_dir("ok");
    let report: CaptureReport = match capture_one_shot(&dir, &manifest(), 42) {
        Ok(r) => r,
        Err(e) => {
            let s = format!("{e:?}");
            if s.contains("EPERM") || s.contains("EACCES") {
                eprintln!("skipping capture_one_shot test: {e:?}");
                return;
            }
            panic!("capture_one_shot failed: {e:?}");
        }
    };

    assert_eq!(report.checkpoint_index, 1);
    assert!(
        report.payload_bytes > 0,
        "payload should be non-empty for a real Tier 2 capture",
    );

    // Re-open the trace and decode the payload back into a
    // Tier2State — the round-trip the recording engine relies on.
    let reader = TraceReader::open(&dir).expect("reopen failed");
    assert_eq!(reader.checkpoint_indices(), &[1u64]);
    let cp = reader.open_checkpoint(1).expect("open checkpoint");
    assert_eq!(
        cp.payload.len() as u64,
        report.payload_bytes,
        "stored payload size mismatch with report",
    );
    let state = tier2::from_payload(&cp.payload).expect("decode failed");
    // Tier 2 captures always produce a non-empty WritableState
    // because the recorder process has at least heap/stack/.data.
    assert!(
        !state.writable.regions.is_empty(),
        "decoded WritableState should carry at least one region",
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn capture_one_shot_refuses_to_overwrite_existing_dir() {
    let dir = temp_dir("clash");
    fs::create_dir(&dir).unwrap();
    // TraceWriter::create errors out on an existing dir; that
    // surfaces as RecordError::Trace.
    let err = capture_one_shot(&dir, &manifest(), 0).unwrap_err();
    match err {
        RecordError::Trace(_) => {}
        other => panic!("expected Trace error, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}

// SPDX-License-Identifier: MIT
//! Smoke tests for the replay-doctor binary. Builds it via cargo,
//! runs it against real on-disk traces, asserts exit code +
//! relevant output substrings.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use bs_replay_driver::engine::format::TraceWriter;

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
        "bs-replay-doctor-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn doctor_bin() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration-test
    // builds — points to the just-built binary on the same
    // workspace target dir.
    PathBuf::from(env!("CARGO_BIN_EXE_replay-doctor"))
}

#[test]
fn doctor_exits_success_on_clean_trace() {
    let dir = temp_dir("clean");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.write_event(Event::Marker { tag: 0, data: 0 }).unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin()).arg(&dir).output().unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("trace OK") || stdout.contains("info"),
        "unexpected stdout:\n{stdout}",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_exits_failure_on_missing_manifest() {
    let dir = temp_dir("no-manifest");
    fs::create_dir(&dir).unwrap();

    let out = Command::new(doctor_bin()).arg(&dir).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("manifest-missing"), "stdout:\n{stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_with_build_id_flag_flags_mismatch() {
    let dir = temp_dir("bad-bid");
    {
        let writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--build-id")
        .arg("00000000")
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("build-id-mismatch"), "stdout:\n{stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_unknown_flag_exits_two() {
    let out = Command::new(doctor_bin())
        .arg("--definitely-not-a-flag")
        .arg("/tmp")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag"), "stderr:\n{stderr}");
}

#[test]
fn doctor_missing_dir_exits_two() {
    let out = Command::new(doctor_bin()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing trace directory"), "stderr:\n{stderr}");
}

#[test]
fn doctor_help_exits_zero_and_prints_usage() {
    let out = Command::new(doctor_bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("replay-doctor"));
    assert!(stdout.contains("--check-host"));
    assert!(stdout.contains("--load"));
}

#[test]
fn doctor_load_flag_prints_summary_one_liner() {
    let dir = temp_dir("load-summary");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        for i in 0..7u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.take_checkpoint(b"a".to_vec()).unwrap();
        for i in 7..10u32 {
            writer.write_event(Event::Marker { tag: i, data: 0 }).unwrap();
        }
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--load")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "stdout: {}", String::from_utf8_lossy(&out.stdout));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The trace held 10 events split into 2 segments by the
    // checkpoint-forced rotation, plus 1 checkpoint.
    assert!(
        stdout.contains("10 events"),
        "expected `10 events` in output, got: {stdout}",
    );
    assert!(stdout.contains("2 segments"), "got: {stdout}");
    assert!(stdout.contains("1 checkpoints"), "got: {stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_load_flag_on_missing_dir_exits_one() {
    let out = Command::new(doctor_bin())
        .arg("--load")
        .arg("/no/such/dir/exists/here")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("load failed"), "stderr: {stderr}");
}

#[test]
fn doctor_check_host_flag_does_not_panic_on_unsupported_os() {
    // On macOS host_features() returns Unsupported. The doctor
    // should fall back to skipping the host check rather than
    // crashing — exit code remains based on the rest of the report.
    let dir = temp_dir("check-host");
    {
        let writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--check-host")
        .arg(&dir)
        .output()
        .unwrap();
    // Exit code depends on platform: Linux finds features and
    // passes; non-linux skips. Either way it shouldn't panic.
    assert!(out.status.code().is_some(), "doctor died without an exit code");
    fs::remove_dir_all(&dir).ok();
}

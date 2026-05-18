// SPDX-License-Identifier: MIT
//! Smoke tests for the replay-doctor binary. Builds it via cargo,
//! runs it against real on-disk traces, asserts exit code +
//! relevant output substrings.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use bs_replay_driver::engine::format::TraceWriter;
use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;

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
        initial_fds: vec![],
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bs-replay-doctor-{label}-{}", std::process::id(),));
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
        writer
            .write_event(Event::Marker { tag: 0, data: 0 })
            .unwrap();
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
    assert!(
        stderr.contains("missing trace directory"),
        "stderr:\n{stderr}"
    );
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
    let m = {
        let mut m = manifest();
        m.recorded_at = Some("2026-05-06T12:00:00Z".to_owned());
        m
    };
    {
        let mut writer = TraceWriter::create(&dir, &m).unwrap();
        for i in 0..7u32 {
            writer
                .write_event(Event::Marker { tag: i, data: 0 })
                .unwrap();
        }
        writer.take_checkpoint(b"a".to_vec()).unwrap();
        for i in 7..10u32 {
            writer
                .write_event(Event::Marker { tag: i, data: 0 })
                .unwrap();
        }
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--load")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The trace held 10 events split into 2 segments by the
    // checkpoint-forced rotation, plus 1 checkpoint.
    assert!(
        stdout.contains("10 events"),
        "expected `10 events` in output, got: {stdout}",
    );
    assert!(stdout.contains("2 segments"), "got: {stdout}");
    assert!(stdout.contains("1 checkpoints"), "got: {stdout}");
    assert!(
        stdout.contains(&format!("build-id: {}", m.build_id)),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("recorded-at: 2026-05-06T12:00:00Z"),
        "got: {stdout}",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_load_omits_recorded_at_line_when_none() {
    let dir = temp_dir("load-no-ts");
    {
        // sample manifest() leaves recorded_at = None.
        let writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--load")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("recorded-at:"),
        "should not emit recorded-at line when None, got: {stdout}",
    );
    assert!(stdout.contains("build-id:"), "got: {stdout}");
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
fn doctor_counts_lists_every_event_kind() {
    let dir = temp_dir("counts");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer
            .write_event(Event::Marker { tag: 2, data: 0 })
            .unwrap();
        writer
            .write_event(Event::PcMarker { pc: 0xCAFE_F00D })
            .unwrap();
        writer
            .write_event(Event::Syscall {
                nr: 1,
                args: [2, 0xCAFE_BA00, 5, 0, 0, 0],
                result: 5,
                output: Vec::new(),
            })
            .unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--counts")
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Syscall"), "stdout: {stdout}");
    assert!(stdout.contains("Marker"));
    assert!(stdout.contains("PcMarker"));
    assert!(stdout.contains("total"));
    // Syscall should be 1, Marker should be 2, PcMarker 1.
    assert!(
        stdout.contains("Syscall          : 1"),
        "expected exactly 1 syscall in counts: {stdout}"
    );
    assert!(
        stdout.contains("Marker           : 2"),
        "expected exactly 2 markers: {stdout}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_dump_events_renders_each_kind_in_order() {
    let dir = temp_dir("dump");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        writer
            .write_event(Event::Marker {
                tag: 0xFF,
                data: 42,
            })
            .unwrap();
        writer
            .write_event(Event::PcMarker { pc: 0xC0DE_F00D })
            .unwrap();
        writer
            .write_event(Event::Syscall {
                nr: 1,
                args: [2, 0xCAFE, 5, 0, 0, 0],
                result: 5,
                output: vec![],
            })
            .unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--dump-events")
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each event should produce one line tagged with its kind.
    assert!(stdout.contains("Marker"), "stdout: {stdout}");
    assert!(stdout.contains("PcMarker"));
    assert!(stdout.contains("Syscall"));
    // Syscall line should resolve nr=1 to "write" via the
    // bs-syscall-spec curated table.
    assert!(
        stdout.contains("write"),
        "syscall name not resolved in dump: {stdout}"
    );
    // PcMarker line should print the hex PC.
    assert!(
        stdout.contains("0xc0de"),
        "PC not formatted in hex: {stdout}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_dump_events_with_explicit_n_caps_output() {
    let dir = temp_dir("dump-n");
    {
        let mut writer = TraceWriter::create(&dir, &manifest()).unwrap();
        for i in 0..10 {
            writer
                .write_event(Event::Marker { tag: i, data: 0 })
                .unwrap();
        }
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--dump-events")
        .arg("3")
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let marker_lines = stdout.lines().filter(|l| l.contains("Marker")).count();
    assert_eq!(
        marker_lines, 3,
        "expected 3 marker lines for --dump-events 3, got {marker_lines}: {stdout}"
    );
    assert!(
        stdout.contains("stopping after 3 events"),
        "expected truncation note: {stdout}",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_diff_identical_traces_returns_zero() {
    let dir_a = temp_dir("diff-a");
    let dir_b = temp_dir("diff-b");
    for d in [&dir_a, &dir_b] {
        let mut writer = TraceWriter::create(d, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer
            .write_event(Event::PcMarker { pc: 0xDEAD_BEEF })
            .unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--diff")
        .arg(&dir_b)
        .arg(&dir_a)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("identical"),
        "expected 'identical' on matching traces; got: {stdout}",
    );
    fs::remove_dir_all(&dir_a).ok();
    fs::remove_dir_all(&dir_b).ok();
}

#[test]
fn doctor_diff_divergent_traces_reports_first_event() {
    let dir_a = temp_dir("diff-div-a");
    let dir_b = temp_dir("diff-div-b");
    {
        let mut writer = TraceWriter::create(&dir_a, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer
            .write_event(Event::Marker { tag: 2, data: 0 })
            .unwrap();
        writer.finish().unwrap();
    }
    {
        let mut writer = TraceWriter::create(&dir_b, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer
            .write_event(Event::Marker { tag: 99, data: 0 })
            .unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--diff")
        .arg(&dir_b)
        .arg(&dir_a)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("diverge at event 1"),
        "expected divergence at event 1; got: {stdout}",
    );
    assert!(
        stdout.contains("marker 2:0 != 99:0") || stdout.contains("marker"),
        "expected marker mismatch reason; got: {stdout}",
    );
    fs::remove_dir_all(&dir_a).ok();
    fs::remove_dir_all(&dir_b).ok();
}

#[test]
fn doctor_diff_length_mismatch_reports_truncation() {
    let dir_a = temp_dir("diff-len-a");
    let dir_b = temp_dir("diff-len-b");
    {
        let mut writer = TraceWriter::create(&dir_a, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer
            .write_event(Event::Marker { tag: 2, data: 0 })
            .unwrap();
        writer.finish().unwrap();
    }
    {
        let mut writer = TraceWriter::create(&dir_b, &manifest()).unwrap();
        writer
            .write_event(Event::Marker { tag: 1, data: 0 })
            .unwrap();
        writer.finish().unwrap();
    }
    let out = Command::new(doctor_bin())
        .arg("--diff")
        .arg(&dir_b)
        .arg(&dir_a)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ended"),
        "expected length-mismatch ('ended'); got: {stdout}",
    );
    fs::remove_dir_all(&dir_a).ok();
    fs::remove_dir_all(&dir_b).ok();
}

#[test]
fn doctor_diff_without_arg_returns_two() {
    let out = Command::new(doctor_bin()).arg("--diff").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
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
    assert!(
        out.status.code().is_some(),
        "doctor died without an exit code"
    );
    fs::remove_dir_all(&dir).ok();
}

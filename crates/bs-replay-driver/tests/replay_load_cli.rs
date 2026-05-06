// SPDX-License-Identifier: MIT
//! Smoke tests for the `replay-load` CLI binary. Cross-platform.

use std::process::{Command, Stdio};

fn binary_path() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("current exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.push("replay-load");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(binary_path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn replay-load: {e}"));
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn help_long_flag_prints_usage_and_exits_zero() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("replay-load"));
    assert!(stdout.contains("<TRACE_DIR>"));
}

#[test]
fn help_short_flag_prints_usage() {
    let (code, stdout, _) = run(&["-h"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage:"));
}

#[cfg(target_os = "linux")]
#[test]
fn missing_args_returns_exit_code_two() {
    let (code, _, stderr) = run(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("missing TRACE_DIR")
            || stderr.contains("missing program"),
        "stderr missing diagnostic: {stderr}",
    );
    assert!(stderr.contains("Usage:"));
}

#[cfg(target_os = "linux")]
#[test]
fn unknown_flag_returns_exit_code_two() {
    let (code, _, stderr) = run(&["--no-such-flag"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("unknown flag"),
        "stderr missing unknown-flag diagnostic: {stderr}",
    );
}

#[cfg(target_os = "linux")]
#[test]
fn nonexistent_trace_dir_fails_cleanly() {
    let (code, _, stderr) = run(&[
        "/tmp/nonexistent-trace-dir-xyz",
        "--",
        "/bin/true",
    ]);
    // Either exit code 1 (trace open failed) or 3 (perms).
    assert!(
        code == 1 || code == 3,
        "expected exit 1 or 3, got {code}; stderr={stderr}",
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_main_reports_platform_skip() {
    let (code, _, stderr) = run(&["/tmp/nope", "--", "/bin/true"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("Linux-only"));
}

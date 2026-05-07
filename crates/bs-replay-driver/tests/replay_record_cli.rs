// SPDX-License-Identifier: MIT
//! Smoke tests for the `replay-record` CLI binary. Cross-platform.

use std::process::{Command, Stdio};

fn binary_path() -> std::path::PathBuf {
    // Built by cargo before tests run; lives next to the test
    // binary because it shares the workspace target dir.
    let mut p = std::env::current_exe().expect("current exe");
    p.pop(); // tests/<test>
    if p.ends_with("deps") {
        p.pop();
    }
    p.push("replay-record");
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
        .unwrap_or_else(|e| panic!("spawn replay-record: {e}"));
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn help_long_flag_prints_usage_and_exits_zero() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0, "--help should exit 0");
    assert!(stdout.contains("Usage:"), "stdout missing `Usage:`: {stdout}");
    assert!(stdout.contains("replay-record"));
    assert!(
        stdout.contains("<TRACE_DIR>"),
        "usage line should reference TRACE_DIR positional",
    );
    // The opt-in flags must show up in --help so users can
    // discover them.
    assert!(stdout.contains("--patch-vdso"), "missing --patch-vdso in help");
    assert!(stdout.contains("--trap-tsc"), "missing --trap-tsc in help");
    assert!(stdout.contains("--overwrite"), "missing --overwrite in help");
    assert!(stdout.contains("--disable-cpuid"), "missing --disable-cpuid in help");
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
    assert_eq!(code, 2, "no args should exit 2 (argv parse error)");
    assert!(
        stderr.contains("missing TRACE_DIR")
            || stderr.contains("missing program"),
        "stderr missing diagnostic: {stderr}",
    );
    assert!(stderr.contains("Usage:"), "expected USAGE printed on parse error");
}

#[cfg(target_os = "linux")]
#[test]
fn missing_separator_returns_exit_code_two() {
    let (code, _, stderr) = run(&["/tmp/nope"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("missing program"),
        "stderr missing missing-program diagnostic: {stderr}",
    );
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

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_main_reports_platform_skip() {
    let (code, _, stderr) = run(&["/tmp/nope", "--", "/bin/true"]);
    // Darwin stub exits 2 with a clear message.
    assert_eq!(code, 2);
    assert!(
        stderr.contains("Linux-only"),
        "stderr missing platform skip note: {stderr}",
    );
}

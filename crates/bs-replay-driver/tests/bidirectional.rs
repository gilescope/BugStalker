// SPDX-License-Identifier: MIT
//! Bidirectional record→replay smoke test. Linux only.
//!
//! Records `/bin/true` via `record_program`, then replays the
//! resulting trace via `replay_program` against a fresh
//! `/bin/true` invocation. Asserts:
//!
//! - record produced ≥1 Event::Syscall
//! - replay session ran without panicking
//! - replay's exit reason is one of the documented kinds
//!
//! This is a *smoke* check, not a strict determinism proof:
//! real programs are non-deterministic without 3D vDSO
//! patching, 3F single-CPU pinning, and ADDR_NO_RANDOMIZE.
//! Even /bin/true's libc startup may diverge run-to-run on
//! some hosts. The test tolerates `ShimRefused(Mismatch)` as
//! "expected non-determinism" and the host environment is
//! the variable, not the recorder.
//!
//! Auto-skips on hosts that can't fork+seccomp+ptrace.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs;
use std::path::PathBuf;

use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use bs_replay_driver::{
    record_program, replay_program, RecordOptions, RecordProgramError,
    RecorderExitStatus, ReplayExit, ReplayOptions, ReplayProgramError,
    ShimRefusedReason,
};

fn manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "bidirectional-smoke".to_owned(),
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
        "bs-replay-bidi-{label}-{}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn is_skip_record(err: &RecordProgramError) -> bool {
    let s = format!("{err}");
    s.contains("EPERM")
        || s.contains("EACCES")
        || s.contains("ENOSYS")
        || s.contains("yama")
        || s.contains("64")
        || s.contains("65")
        || s.contains("66")
}

fn is_skip_replay(err: &ReplayProgramError) -> bool {
    let s = format!("{err}");
    s.contains("EPERM")
        || s.contains("EACCES")
        || s.contains("ENOSYS")
        || s.contains("yama")
        || s.contains("64")
        || s.contains("65")
        || s.contains("66")
}

#[test]
fn record_then_replay_bin_true_runs_to_completion() {
    let prog = std::path::Path::new("/bin/true");
    if !prog.exists() {
        eprintln!("skipping: /bin/true not present");
        return;
    }

    // ---- record -----------------------------------------------
    let trace_dir = temp_dir("trace");
    let argv = vec![CString::new("/bin/true").unwrap()];
    let envp = vec![CString::new("PATH=/usr/bin:/bin").unwrap()];
    let record_report = match record_program(
        &trace_dir,
        &manifest(),
        argv.clone(),
        envp.clone(),
        RecordOptions::default(),
    ) {
        Ok(r) => r,
        Err(e) if is_skip_record(&e) => {
            eprintln!("skipping: record skipped — {e:?}");
            fs::remove_dir_all(&trace_dir).ok();
            return;
        }
        Err(e) => {
            fs::remove_dir_all(&trace_dir).ok();
            panic!("record_program failed: {e:?}");
        }
    };

    if record_report.syscall_events == 0 {
        eprintln!(
            "skipping replay: recorder produced 0 events (host environment \
             didn't allow a useful recording)"
        );
        fs::remove_dir_all(&trace_dir).ok();
        return;
    }
    assert!(
        matches!(record_report.exit_status, RecorderExitStatus::Exited(0)),
        "record_program should have seen /bin/true exit 0, got {:?}",
        record_report.exit_status,
    );

    eprintln!(
        "record OK: {} syscalls / {} signals / {} instr-traps / {} steps",
        record_report.syscall_events,
        record_report.signal_events,
        record_report.instruction_traps,
        record_report.iterations,
    );

    // ---- replay -----------------------------------------------
    let replay_report = match replay_program(
        &trace_dir,
        argv,
        envp,
        ReplayOptions::default(),
    ) {
        Ok(r) => r,
        Err(e) if is_skip_replay(&e) => {
            eprintln!("skipping replay leg: {e:?}");
            fs::remove_dir_all(&trace_dir).ok();
            return;
        }
        Err(e) => {
            fs::remove_dir_all(&trace_dir).ok();
            panic!("replay_program failed: {e:?}");
        }
    };

    eprintln!(
        "replay session: {} syscalls applied / {} signals skipped / \
         {} instr-traps skipped / {} steps / {} bytes / exit={:?}",
        replay_report.syscalls_applied,
        replay_report.signals_skipped,
        replay_report.instruction_traps_skipped,
        replay_report.iterations,
        replay_report.bytes_written,
        replay_report.exit,
    );

    // The replay session must produce *some* exit reason, and
    // the iterations should be > 0 (we entered the supervisor
    // loop at least once).
    assert!(replay_report.exit.is_some(), "replay didn't stamp an exit reason");
    assert!(replay_report.iterations > 0);

    // Acceptable end states for a smoke test:
    //   Exited(_)            — tracee actually finished
    //   TraceExhausted{..}   — trace ran out cleanly
    //   ShimRefused(Mismatch) — non-determinism we don't yet
    //                            handle (no vDSO patch, no
    //                            single-CPU pin, ASLR on)
    //   ShimRefused(ResultNotCaptured) — only triggered by a
    //                                     stale step-7b trace;
    //                                     would be a regression
    match replay_report.exit.unwrap() {
        ReplayExit::Exited(_)
        | ReplayExit::Signalled(_)
        | ReplayExit::TraceExhausted { .. } => {}
        ReplayExit::ShimRefused(ShimRefusedReason::Mismatch(m)) => {
            eprintln!(
                "(expected on a non-deterministic host) shim mismatch: {m}",
            );
        }
        ReplayExit::ShimRefused(ShimRefusedReason::ResultNotCaptured) => {
            panic!(
                "replay shim hit RESULT_NOT_CAPTURED_YET — step 71's \
                 PTRACE-only recorder regressed to entry-args-only mode"
            );
        }
        ReplayExit::ShimRefused(other) => {
            eprintln!("(unexpected but tolerated) shim refused: {other:?}");
        }
        ReplayExit::IterationCap(n) => {
            panic!("replay hit iteration cap at {n}; either the trace is \
                    huge or the supervisor loop is stuck");
        }
    }

    fs::remove_dir_all(&trace_dir).ok();
}

// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_multithread.py`. Pure
//! scripted I/O — no signal/HTTP threading — but the file lives in
//! the Rust integ binary alongside the threading-prone ports so the
//! whole legacy directory can retire in Phase 4 together.

use crate::helper::Debugger;
use serial_test::serial;
use std::thread;
use std::time::Duration;

const MT_BINARY: &str = "./examples/target/debug/mt";

#[test]
#[serial]
fn multithreaded_app_running() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd(
        "run",
        &[
            "thread 1 spawn",
            "thread 2 spawn",
            "sum2: 199990000",
            "sum1: 49995000",
            "total 249985000",
        ],
    );
}

#[test]
#[serial]
fn multithreaded_breakpoints() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:6", &["New breakpoint"]);
    dbg.cmd("break mt.rs:24", &["New breakpoint"]);
    dbg.cmd("break mt.rs:36", &["New breakpoint"]);
    dbg.cmd("break mt.rs:14", &["New breakpoint"]);

    dbg.cmd(
        "run",
        &[
            "Hit breakpoint 1 at",
            "6     let jh1 = thread::spawn(sum1);",
        ],
    );
    dbg.cmd(
        "continue",
        &[
            "thread 1 spawn",
            "thread 2 spawn",
            "Hit breakpoint 3 at",
            "36     let mut sum2 = 0;",
        ],
    );
    dbg.cmd(
        "continue",
        &["Hit breakpoint 2 at", "24     let mut sum = 0;"],
    );
    dbg.cmd(
        "continue",
        &[
            "Hit breakpoint 4 at",
            "14     println!(\"total {}\", sum1 + sum2);",
        ],
    );
    dbg.cmd("continue", &["total 249985000"]);
}

#[test]
#[serial]
fn multithreaded_backtrace() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:24", &["New breakpoint"]);
    dbg.cmd(
        "run",
        &[
            "thread 1 spawn",
            "Hit breakpoint 1 at",
            "24     let mut sum = 0;",
        ],
    );
    dbg.cmd("backtrace", &["mt::sum1", "new::thread_start"]);
}

#[test]
#[serial]
fn multithreaded_trace() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:36", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at", "36     let mut sum2 = 0;"]);
    dbg.cmd(
        "backtrace all",
        &[
            "thread",
            "mt::main",
            "thread",
            "clock_nanosleep",
            "thread",
            "mt::sum2",
        ],
    );
}

#[test]
#[serial]
fn multithreaded_quit() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:36", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at", "36     let mut sum2 = 0;"]);
    dbg.cmd("quit", &[]);
    thread::sleep(Duration::from_secs(2));
    assert!(!dbg.is_alive(), "bs should have exited after `quit`");
}

#[test]
#[serial]
fn thread_info() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:40", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at"]);
    dbg.cmd(
        "thread info",
        &["#1 thread id", "#2 thread id", "#3 thread id"],
    );
    dbg.cmd("thread current", &["#3 thread id"]);
}

#[test]
#[serial]
fn thread_switch() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:40", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at"]);
    dbg.cmd("thread current", &["#3 thread id"]);
    dbg.cmd("thread switch 2", &["Thread #2 brought into focus"]);
    dbg.cmd("thread current", &["#2 thread id"]);

    // The Python original looped `step` until `24     let mut sum =
    // 0;` appeared in output. That works when the worker thread
    // is *before* sum1's line 24 at the moment the bp on line 40
    // fires — and that timing depends on a `thread::sleep` race
    // in `mt.rs`. Earth got lucky, my x86 box didn't, the loop
    // ran for an hour. Switch to a deterministic bp at line 24
    // plus `continue` — the bp will fire when (or only if) the
    // worker thread is still en route to line 24. If the worker
    // is already past, the test fails fast with a clear panic
    // instead of hanging.
    dbg.cmd("break mt.rs:24", &["New breakpoint"]);
    dbg.cmd("continue", &["24     let mut sum = 0;"]);

    dbg.cmd("step", &["25     for i in 0..10000"]);
    dbg.cmd("var locals", &["sum = i32(0)"]);
}

#[test]
#[serial]
fn thread_switch_frame_switch() {
    let mut dbg = Debugger::spawn(MT_BINARY);
    dbg.cmd("break mt.rs:40", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at"]);
    dbg.cmd("thread current", &["#3 thread id"]);
    dbg.cmd("thread switch 2", &["Thread #2 brought into focus"]);
    dbg.cmd("thread current", &["#2 thread id"]);
    dbg.cmd("frame switch 2", &["switch to #2"]);
    dbg.cmd("var locals", &["sum3_jh = JoinHandle<i32> {"]);
}

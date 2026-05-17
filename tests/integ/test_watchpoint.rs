// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_watchpoint.py`.

#![cfg(any(target_os = "linux"))]

use crate::helper::Debugger;
use serial_test::serial;

const CALC_BINARY: &str = "./examples/target/debug/calculations";

fn fresh() -> Debugger {
    Debugger::spawn(CALC_BINARY)
}

#[test]
#[serial]
fn watchpoint() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:20", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch c", &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: c) (w)",
            "old value: u64(3)",
            "new value: u64(1)",
        ],
    );
    dbg.cmd(
        "continue",
        &["Watchpoint 1 (expr: c) end of scope", "old value: u64(1)"],
    );
}

#[test]
#[serial]
fn watchpoint_at_field() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:81", &["New breakpoint"]);
    dbg.cmd("break calculations.rs:89", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch vector[2]", &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: vector[2]) (w)",
            "old value: i32(3)",
            "new value: i32(4)",
        ],
    );
    dbg.cmd("continue", &["Hit breakpoint 2"]);
    dbg.cmd("watch s.b", &["New watchpoint"]);
    dbg.cmd("continue", &["old value: f64(1)", "new value: f64(2)"]);
    dbg.cmd(
        "continue",
        &[
            "Watchpoint 1 (expr: vector[2]) end of scope",
            "old value: i32(4)",
            "Watchpoint 2 (expr: s.b) end of scope",
            "old value: f64(2)",
        ],
    );
}

#[test]
#[serial]
fn watchpoint_at_address() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:18", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("var &a", &[]);
    let addr = dbg
        .search_in_output(r"&u64 \[0x(.*)\]", 10)
        .map(|s| {
            let trimmed: String = s.chars().take(14).collect();
            format!("0x{trimmed}")
        })
        .expect("address of &a not found");
    dbg.cmd(&format!("watch {addr}:8"), &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &["Hit watchpoint", "old value: u64(1)", "new value: u64(6)"],
    );
    dbg.quit();
}

#[test]
#[serial]
fn watchpoint_with_stepping() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:22", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch a", &["New watchpoint"]);
    dbg.cmd("next", &["Hit watchpoint"]);
    dbg.cmd("next", &["23     b += 1;"]);
    dbg.cmd("next", &["24     c -= 2;"]);
    for _ in 0..5 {
        dbg.cmd("next", &[]);
    }
    dbg.cmd("next", &["Watchpoint 1 (expr: a) end of scope"]);
}

#[test]
#[serial]
fn watchpoint_at_undefined_value() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:20", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch e", &["variable or argument to watch not found"]);
}

#[test]
#[serial]
fn watchpoint_address_already_in_use() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:18", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch a", &["New watchpoint"]);
    dbg.cmd(
        "watch a",
        &["memory location observed by another watchpoint"],
    );
}

#[test]
#[serial]
fn watchpoint_remove() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:22", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch a", &["New watchpoint"]);
    dbg.cmd("watch remove 1", &["Removed watchpoint"]);
    dbg.cmd("watch b", &["New watchpoint"]);
    dbg.cmd("watch remove b", &["Removed watchpoint"]);
}

#[test]
#[serial]
fn watchpoint_hw_limit() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:22", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch a", &["New watchpoint"]);
    dbg.cmd("watch b", &["New watchpoint"]);
    dbg.cmd("watch c", &["New watchpoint"]);
    dbg.cmd("watch d", &["New watchpoint"]);
    dbg.cmd("watch GLOBAL_1", &["watchpoint limit is reached"]);
    dbg.cmd("watch remove a", &["Removed watchpoint"]);
    dbg.cmd("watch GLOBAL_1", &["New watchpoint"]);
}

#[test]
#[serial]
fn watchpoint_after_restart() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:22", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch a", &["New watchpoint"]);
    dbg.cmd("watch GLOBAL_1", &["New watchpoint"]);
    dbg.cmd("watch c", &["New watchpoint"]);
    dbg.cmd("watch info", &["3/4 active watchpoints"]);
    dbg.cmd("run", &["Restart a program?"]);
    dbg.cmd("y", &["Hit breakpoint"]);
    dbg.cmd("watch info", &["1/4 active watchpoints"]);
    dbg.cmd("continue", &["Hit watchpoint 2"]);
    dbg.quit();
}

#[test]
#[serial]
fn watchpoint_rw() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:20", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch +rw a", &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &["Hit watchpoint 1 (expr: a) (rw)", "value: u64(1)"],
    );
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: a) (rw)",
            "old value: u64(1)",
            "new value: u64(6)",
        ],
    );
    dbg.cmd(
        "continue",
        &["Hit watchpoint 1 (expr: a) (rw)", "value: u64(6)"],
    );
    dbg.cmd(
        "continue",
        &["Watchpoint 1 (expr: a) end of scope", "old value: u64(6)"],
    );
}

#[test]
#[serial]
fn watchpoint_at_addr_rw() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:20", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("var &a", &[]);
    let addr = dbg
        .search_in_output(r"&u64 \[0x(.*)\]", 10)
        .map(|s| {
            let trimmed: String = s.chars().take(14).collect();
            format!("0x{trimmed}")
        })
        .expect("address of &a not found");
    dbg.cmd(&format!("watch +rw {addr}:8"), &["New watchpoint"]);
    dbg.cmd("continue", &["Hit watchpoint 1 (rw)", "value: u64(1)"]);
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (rw)",
            "old value: u64(1)",
            "new value: u64(6)",
        ],
    );
    dbg.cmd("continue", &["Hit watchpoint 1 (rw)", "value: u64(6)"]);
}

#[test]
#[serial]
fn watchpoint_at_complex_data_types() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:92", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch (~vector2).len", &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: (~vector2).len) (w)",
            "old value: usize(2)",
            "new value: usize(3)",
        ],
    );
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: (~vector2).len) (w)",
            "old value: usize(3)",
            "new value: usize(4)",
        ],
    );
    dbg.cmd(
        "continue",
        &[
            "Watchpoint 1 (expr: (~vector2).len) end of scope",
            "old value: usize(4)",
        ],
    );
}

#[test]
#[serial]
fn watchpoint_at_complex_data_types2() {
    let mut dbg = fresh();
    dbg.cmd("break calculations.rs:96", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("watch (~(~string).vec).len", &["New watchpoint"]);
    dbg.cmd(
        "continue",
        &[
            "Hit watchpoint 1 (expr: (~(~string).vec).len) (w)",
            "old value: usize(3)",
            "new value: usize(7)",
        ],
    );
    dbg.cmd(
        "continue",
        &[
            "Watchpoint 1 (expr: (~(~string).vec).len) end of scope",
            "old value: usize(7)",
        ],
    );
}

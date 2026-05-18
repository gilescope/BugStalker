// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_sharedlib.py`.

use crate::helper::Debugger;
use serial_test::serial;
use std::time::Duration;

const CALC_BIN: &str = "./examples/target/debug/calc_bin";

#[test]
#[serial]
fn lib_info() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd(
        "sharedlib info",
        &["???     ./examples/target/debug/calc_bin", "libcalc_lib.so"],
    );

    dbg.cmd("break main.rs:7", &["New breakpoint"]);
    dbg.cmd("run", &[]);

    dbg.cmd_re(
        "sharedlib info",
        &[
            r"0x.*\./examples/target/debug/calc_bin",
            r"0x.*/libcalc_lib\.so",
        ],
    );
}

#[test]
#[serial]
fn lib_step() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd("break main.rs:7", &["New breakpoint"]);
    dbg.cmd(
        "run",
        &[
            "Hit breakpoint 1",
            "let sum_1_2 = unsafe { calc_add(1, 2) }",
        ],
    );
    dbg.cmd("step", &["lib.rs:3", "3     a + b"]);
    dbg.cmd("step", &["4 }"]);
    dbg.cmd(
        "step",
        &[
            "main.rs:8",
            "8     let sub_2_1 = unsafe { calc_sub(2, 1) };",
        ],
    );
}

#[test]
#[serial]
fn lib_fn_breakpoint() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd("break calc_add", &["New breakpoint 1"]);
    dbg.cmd("run", &["Hit breakpoint 1", "3     a + b"]);
}

#[test]
#[serial]
fn lib_line_breakpoint() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd("b lib.rs:8", &["New breakpoint 1"]);
    dbg.cmd("run", &["Hit breakpoint 1", "8     a - b"]);
}

#[test]
#[serial]
fn dynamic_load_lib_info() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd("b main.rs:8", &["New breakpoint 1"]);
    dbg.cmd("b main.rs:19", &["New breakpoint 2"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("sharedlib info", &[]);

    // Python's try/except: if `libprinter_lib.so` appears before
    // we hit bp 2, the test fails (library shouldn't load that
    // soon). Mirror with `try_expect` — true means "saw it within
    // 1 s" which is the unwanted case.
    if dbg.try_expect("libprinter_lib.so", Duration::from_secs(1)) {
        panic!("libprinter_lib.so loaded before breakpoint 2");
    }
    dbg.cmd("continue", &["Hit breakpoint 2"]);
    dbg.cmd("sharedlib info", &["libprinter_lib.so"]);
}

#[test]
#[serial]
fn dynamic_load_lib_step() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd("break main.rs:24", &["New breakpoint"]);
    dbg.cmd(
        "run",
        &["Hit breakpoint 1", "24         print_sum_fn(sum_1_2);"],
    );
    for _ in 0..8 {
        dbg.cmd("step", &[]);
    }
    dbg.cmd("step", &["3     println!(\"sum is {num}\")"]);
}

#[test]
#[serial]
fn deferred_breakpoint() {
    let mut dbg = Debugger::spawn(CALC_BIN);
    dbg.cmd(
        "break print_sum",
        &["Add deferred breakpoint for future shared library load"],
    );
    dbg.cmd("y", &[]);
    dbg.cmd("run", &["Hit breakpoint"]);
}

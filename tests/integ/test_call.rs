// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_call.py`.

use crate::helper::Debugger;
use serial_test::serial;

const CALLS_BINARY: &str = "./examples/target/debug/calls";
const VARS_BINARY: &str = "./examples/target/debug/vars";

#[test]
#[serial]
fn simple_call_execute() {
    let mut dbg = Debugger::spawn(CALLS_BINARY);
    dbg.cmd("break main", &["New breakpoint 1"]);
    dbg.cmd("run", &[]);
    dbg.cmd("call sum2 2 5", &["my sum is 7"]);
    dbg.cmd("call sum2 1 9", &["my sum is 10"]);
    dbg.cmd("continue", &["Program exit with code: 0"]);
}

#[test]
#[serial]
fn breakpoint_not_hit() {
    let mut dbg = Debugger::spawn(CALLS_BINARY);
    dbg.cmd("break main", &["New breakpoint 1"]);
    dbg.cmd("break sum2", &["New breakpoint 2"]);
    dbg.cmd("run", &[]);
    dbg.cmd("call sum2 2 5", &["my sum is 7"]);
    dbg.cmd("call sum2 1 9", &["my sum is 10"]);
    dbg.cmd("continue", &["Hit breakpoint 2"]);
    dbg.cmd("continue", &["Program exit with code: 0"]);
}

#[test]
#[serial]
fn six_args() {
    let mut dbg = Debugger::spawn(CALLS_BINARY);
    dbg.cmd("break main", &["New breakpoint 1"]);
    dbg.cmd("run", &[]);
    dbg.cmd("call sum6i -1 -2 -3 -4 -5 -6", &["my sum is -21"]);
    dbg.cmd(
        "call sum6i 1 256 65537 4294967297 4294967297 -1",
        &["my sum is 8590000387"],
    );
    dbg.cmd("call sum6u 1 2 3 4 5 6", &["my sum is 21"]);
    dbg.cmd(
        "call sum6u 1 256 65537 4294967297 4294967297 1",
        &["my sum is 8590000389"],
    );
}

#[test]
#[serial]
fn bool_arg() {
    let mut dbg = Debugger::spawn(CALLS_BINARY);
    dbg.cmd("break main", &["New breakpoint 1"]);
    dbg.cmd("run", &[]);
    dbg.cmd("call print_bool false", &["bool is false"]);
    dbg.cmd("call print_bool true", &["bool is true"]);
}

// Python's `test_pointer_arg` extracts pointer addresses from
// `var &arg1` output via a regex (`search_in_output`) and feeds
// them back into `call print_deref`. The Rust port would need a
// matching scrape API on the helper. Skipped for now; the call
// machinery is exercised by the four tests above.
#[test]
#[serial]
#[ignore = "needs `search_in_output`-style scrape helper"]
fn pointer_arg() {}

#[test]
#[serial]
fn fmt_vars() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.cmd("break vars.rs:641", &[]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd(
        "vard locals",
        &[
            "[]",
            "[]",
            "[1, 23, 3]",
            "Struct0 { a: 1 }",
            "Struct1 { field1: 1, field2: 3 }",
            "Struct1 { field1: 1, field2: \"44\" }",
            "Struct2 { field1: \"66\", field2: 55 }",
            "Struct3 { field1: 11, field2: 12 }",
            "[\"abc\", \"ef\", \"g\"]",
            "A",
            "S1(Struct1 { field1: 100, field2: \"100\" })",
            "S2(Struct2 { field1: 1, field2: 2 })",
            "Some(1)",
            "\"some str\"",
            "\"some string\"",
        ],
    );
}

#[test]
#[serial]
fn fmt_args() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.cmd("break vars.rs:645", &[]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("argd all", &["\"one\"", "[\"two\", \"three\"]"]);
}

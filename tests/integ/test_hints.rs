// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_hints.py`. These tests
//! poke at rustyline's tab-completion via raw character writes
//! (`print` without trailing newline).

use crate::helper::Debugger;
use serial_test::serial;

const VARS_BINARY: &str = "./examples/target/debug/vars";

#[test]
#[serial]
fn command_hints() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.print("br\t", &["break"]);
    dbg.print("\n", &[]);
    dbg.print("f\t", &["frame"]);
    dbg.print("\n", &[]);
    dbg.print("ste\t", &["step"]);
    dbg.print("\n", &[]);
    dbg.print(
        "b\t\t",
        &["\x1b[4mb\x1b[0mreak", " backtrace|\x1b[1m\x1b[4mbt\x1b[0m"],
    );
}

#[test]
#[serial]
fn break_command_hints() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.print("b vars.r\t", &["b vars.rs:"]);
    dbg.print("\n", &[]);
}

#[test]
#[serial]
fn var_command_hints() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.cmd("break vars.rs:9", &["New breakpoint"]);
    dbg.cmd("run", &["let int32 = 2_i32;"]);

    dbg.print("var \t\t", &["int8", "int16", "\x1b[4mlocals\x1b[0m"]);
    dbg.print(" int1\t", &["int16"]);

    dbg.print("\n", &[]);

    dbg.print("vard \t\t", &["int8", "int16", "\x1b[4mlocals\x1b[0m"]);
    dbg.print(" int1\t", &["int16"]);
}

#[test]
#[serial]
fn arg_command_hints() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    dbg.cmd("break vars.rs:9", &["New breakpoint"]);
    dbg.cmd("run", &["let int32 = 2_i32;"]);
    // The Python test_arg_command_hints didn't pre-set a frame
    // with args at vars.rs:9 — there's only `arguments` (no real
    // user-supplied args). The completion lists "arguments" as the
    // only option. Mirror that.
    dbg.print("arg\t", &["arguments"]);
    dbg.print("\n", &[]);
}

#[test]
#[serial]
fn sub_command_hints() {
    let mut dbg = Debugger::spawn(VARS_BINARY);
    // `break ` autocompletion lists subcommands.
    dbg.print("break \t\t", &["info"]);
    dbg.print("\n", &[]);
}

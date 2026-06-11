// SPDX-License-Identifier: MIT
//! `Debugger::panic_location` — recover the exact panic site from the
//! `#[track_caller]` `&Location` at a break-on-panic stop, across the
//! panic flavors that route through different entry points / argument
//! shapes (`panic_fmt`, `panic_bounds_check`, …). The asserted lines are
//! the `// PANIC_*_LINE`-tagged statements in
//! `examples/panic_kinds/src/main.rs`; the `(line, col)` values are the
//! ones the panic hook itself prints (`panicked at …:line:col`).
use crate::common::{TestHooks, TestInfo};
use crate::{PANIC_KINDS_APP, prepare_debugee_process};
use bugstalker::debugger::DebuggerBuilder;
use serial_test::serial;

fn assert_panic_loc(arg: &'static str, line: u32, column: u32) {
    let process = prepare_debugee_process(PANIC_KINDS_APP, &[arg]);
    let info = TestInfo::default();
    // Default builder installs break-on-panic auto-traps.
    let mut debugger = DebuggerBuilder::new()
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    debugger.start_debugee().unwrap();
    let loc = debugger
        .panic_location()
        .unwrap_or_else(|| panic!("{arg}: expected a panic location at the break-on-panic stop"));
    assert!(
        loc.file.ends_with("main.rs"),
        "{arg}: file should be the fixture source, got {:?}",
        loc.file
    );
    assert_eq!(
        (loc.line, loc.column),
        (line, column),
        "{arg}: wrong panic location"
    );
}

#[test]
#[serial]
fn panic_loc_str() {
    assert_panic_loc("str", 24, 5);
}

#[test]
#[serial]
fn panic_loc_fmt() {
    assert_panic_loc("fmt", 30, 5);
}

#[test]
#[serial]
fn panic_loc_unwrap() {
    assert_panic_loc("unwrap", 36, 7);
}

#[test]
#[serial]
fn panic_loc_expect() {
    assert_panic_loc("expect", 42, 7);
}

#[test]
#[serial]
fn panic_loc_index() {
    assert_panic_loc("index", 49, 20);
}

#[test]
#[serial]
fn panic_loc_assert() {
    assert_panic_loc("assert", 55, 5);
}

/// A normal (non-panic) stop has no panic location.
#[test]
#[serial]
fn panic_loc_none_when_not_panicking() {
    let process = prepare_debugee_process(PANIC_KINDS_APP, &[]); // no panic
    let info = TestInfo::default();
    let mut debugger = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    debugger.set_breakpoint_at_fn("main").unwrap();
    debugger.start_debugee().unwrap();
    assert!(
        debugger.panic_location().is_none(),
        "a non-panic stop must have no panic location"
    );
}

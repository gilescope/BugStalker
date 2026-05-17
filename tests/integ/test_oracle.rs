// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_oracle.py`.

use crate::helper::Debugger;
use serial_test::serial;

const TOKIO_TICKER: &str = "./examples/target/debug/tokioticker";

fn spawn_with_tokio_oracle() -> Debugger {
    Debugger::spawn_with_oracles(TOKIO_TICKER, &["tokio"])
}

#[test]
#[serial]
fn oracle_unavailable_until_debugee_start() {
    let mut dbg = spawn_with_tokio_oracle();
    dbg.cmd("oracle tokio", &["Oracle not found or not ready"]);
}

#[test]
#[serial]
fn tokio_oracle() {
    let mut dbg = spawn_with_tokio_oracle();
    dbg.cmd("b main.rs:20", &["New breakpoint"]);
    dbg.cmd("b main.rs:32", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd_re("oracle tokio", &[r"[1-9]\d? tasks running"]);
    dbg.cmd("continue", &["Hit breakpoint 2"]);
    dbg.cmd("oracle tokio", &["0 tasks running"]);
    dbg.quit();
}

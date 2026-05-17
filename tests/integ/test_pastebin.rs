// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_pastebin.py`. Same shape as
//! the todos port — debugger drives a Rocket-based pastebin server,
//! HTTP requests fire from `std::thread`s via `curl`. The legacy
//! Python file ran under the multi-threaded forkpty risk too.

use crate::helper::Debugger;
use serial_test::serial;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const PASTEBIN_BINARY: &str = "./examples/target/debug/pastebin";
const PASTEBIN_URL: &str = "http://localhost:8000";
const PAYLOAD: &str = "hello from integration test";

/// Kill any leftover `pastebin` server processes from prior tests
/// before spawning a fresh debuggee — same rationale as
/// `cleanup_todos` in `test_todos.rs`: `bs`'s `q` command detaches
/// from the inferior but doesn't kill it, so a stale Rocket server
/// will hold TCP :8000 across runs.
fn cleanup_pastebin() {
    let _ = Command::new("pkill")
        .args(["-9", "-f", "/pastebin$"])
        .status();
    thread::sleep(Duration::from_millis(200));
}

fn spawn_curl_post() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = Command::new("curl")
            .arg("-sS")
            .args(["--data", PAYLOAD])
            .arg(PASTEBIN_URL)
            .output();
        let _ = tx.send(());
    });
    rx
}

#[test]
#[serial]
fn step_over_until_response() {
    // Mirrors test_pastebin.PastebinTestCase.test_step_over_until_response.
    cleanup_pastebin();
    let mut dbg = Debugger::spawn(PASTEBIN_BINARY);

    dbg.cmd("b main.rs:21", &["New breakpoint"]);
    thread::sleep(Duration::from_secs(3));
    dbg.cmd_re("run", &["Configured for debug."]);

    let done = spawn_curl_post();
    thread::sleep(Duration::from_secs(3));

    // Step over until the HTTP client thread reports the response
    // came back. Capped at 500 iterations so a hung inferior can't
    // make this run for hours.
    let mut got_response = false;
    for _ in 0..500 {
        if done.try_recv().is_ok() {
            got_response = true;
            break;
        }
        dbg.cmd("next", &["next"]);
        thread::sleep(Duration::from_millis(100));
    }
    assert!(got_response, "curl never reported the response back");
    thread::sleep(Duration::from_millis(200));

    dbg.control('c');
    dbg.quit();
    cleanup_pastebin();
}

#[test]
#[serial]
fn continue_until_response() {
    // Mirrors test_pastebin.PastebinTestCase.test_continue_until_response.
    // Steps through several known source lines after the request
    // hits the bp, then `continue`s until the response returns.
    cleanup_pastebin();
    let mut dbg = Debugger::spawn(PASTEBIN_BINARY);

    dbg.cmd("b main.rs:21", &["New breakpoint"]);
    thread::sleep(Duration::from_secs(3));
    dbg.cmd("run", &["Configured for debug."]);

    let done = spawn_curl_post();
    thread::sleep(Duration::from_secs(2));

    dbg.expect_in_output("21     let id = PasteId::new(ID_LENGTH);");
    dbg.cmd("next", &["22     paste"]);
    dbg.cmd("next", &["22     paste"]);
    dbg.cmd("next", &["23         .open(128.kibibytes())"]);
    dbg.cmd("next", &["24         .into_file(id.file_path())"]);
    dbg.cmd("next", &["22     paste"]);
    dbg.cmd("next", &["24         .into_file(id.file_path())"]);
    dbg.cmd("next", &["25         .await?;"]);

    dbg.cmd("continue", &[]);
    // Wait up to ~5 s for the HTTP client thread to see the
    // response, then tear down.
    for _ in 0..50 {
        if done.try_recv().is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    dbg.control('c');
    dbg.quit();
    cleanup_pastebin();
}

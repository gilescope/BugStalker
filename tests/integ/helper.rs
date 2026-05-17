// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/helper.py::Debugger`.
//!
//! Spawns `bs` on a pseudoterminal via `expectrl` (which uses
//! `posix_spawn` under the hood) and exposes a small command +
//! expect API matching the Python helper one-for-one. Anything new
//! we need for the Rust ports gets added here, not duplicated in
//! the test files.
//!
//! Why not `fork`? Because the Python helper deadlocks when the
//! test process has live background threads (HTTP-client threads in
//! `test_todos.py`, signal-sender threads in `test_signals.py`).
//! `posix_spawn` copies the address space via `vfork`/exec without
//! carrying the parent's pthread locks, so the child never inherits
//! a half-held malloc / GIL / log lock from another thread. Future
//! Rust tests with background threads can use this helper without
//! the deprecation warning the Python harness throws.

#![allow(dead_code)]

use expectrl::Regex;
use expectrl::session::Session;
use std::process::Command;
use std::time::Duration;

const BS_BINARY: &str = "./target/release/bs";
const READY_MARKER: &str = "BugStalker greets";

/// Debugger session.
pub struct Debugger {
    session: Session,
}

impl Debugger {
    /// Spawn `bs -t none <debuggee_path>` and wait for the greet
    /// banner. Mirrors `Debugger.__init__(path=…)` in the Python
    /// helper.
    pub fn spawn(debuggee_path: &str) -> Self {
        Self::spawn_with_oracles(debuggee_path, &[])
    }

    /// Spawn `bs -t none [--oracle X]* <debuggee_path>`.
    pub fn spawn_with_oracles(debuggee_path: &str, oracles: &[&str]) -> Self {
        let mut cmd = Command::new(BS_BINARY);
        cmd.args(["-t", "none"]);
        for o in oracles {
            cmd.args(["--oracle", o]);
        }
        cmd.arg(debuggee_path);

        let mut session =
            Session::spawn(cmd).expect("spawn bs failed — was `cargo build --release` run?");
        // A generous default; individual `cmd` calls can be lower
        // when an interaction is supposed to be fast.
        session.set_expect_timeout(Some(Duration::from_secs(30)));
        session
            .expect(READY_MARKER)
            .expect("BugStalker did not print its greet banner");
        Self { session }
    }

    /// `Debugger.cmd(cmd, *should_see)` — send a line, then assert
    /// each `should_see` string appears in subsequent output.
    pub fn cmd(&mut self, cmd: &str, should_see: &[&str]) {
        self.session
            .send_line(cmd)
            .unwrap_or_else(|e| panic!("send_line({cmd:?}): {e}"));
        for needle in should_see {
            self.session
                .expect(*needle)
                .unwrap_or_else(|e| panic!("after {cmd:?}, expected {needle:?}: {e}"));
        }
    }

    /// `Debugger.cmd_re(cmd, *should_see_re)`.
    pub fn cmd_re(&mut self, cmd: &str, regexes: &[&str]) {
        self.session.send_line(cmd).unwrap();
        for re in regexes {
            self.session
                .expect(Regex(re))
                .unwrap_or_else(|e| panic!("after {cmd:?}, expected /{re}/: {e}"));
        }
    }

    /// Send a quit and wait for EOF. Drops the PTY cleanly so the
    /// debuggee subprocess doesn't leak (it would otherwise hold the
    /// inferior's TCP port across tests).
    pub fn quit(&mut self) {
        let _ = self.session.send_line("q");
        // Best-effort drain; the PTY may already be gone if `q`
        // raced with the inferior exiting on its own.
        let _ = self.session.expect(expectrl::Eof);
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        // Best-effort cleanup so panicking tests don't leave the
        // debuggee subprocess pinned to a TCP port.
        let _ = self.session.send_line("q");
    }
}

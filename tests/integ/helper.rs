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

// We expect `target/release/bs` to exist (built by `make
// build-test-rel` or `cargo build --release --features int_test`).
// The Python harness used the release binary too — running under
// debug exposed timing-sensitive behaviour (auto-trap ordering,
// step-over latency near library boundaries) that's papered over
// by `-O`. The CI step now builds release first; locally use
// `make build-test-rel` or set `BS_BINARY=./target/debug/bs` in
// the env if you specifically want to test the debug build.
const BS_BINARY: &str = "./target/release/bs";
const READY_MARKER: &str = "BugStalker greets";

/// Debugger session.
pub struct Debugger {
    session: Session,
    expect_timeout: Option<Duration>,
}

impl Debugger {
    /// Spawn `bs -t none <debuggee_path>` and wait for the greet
    /// banner. Mirrors `Debugger.__init__(path=…)` in the Python
    /// helper.
    pub fn spawn(debuggee_path: &str) -> Self {
        Self::spawn_with_oracles(debuggee_path, &[])
    }

    /// Spawn `bs -t none [--oracle X]* <debuggee_path>`. The
    /// `debuggee_path` argument is shell-split on whitespace, so
    /// `"./examples/target/debug/signals -- single_thread"` passes
    /// the binary plus its CLI args through to bs the same way the
    /// Python `pexpect.spawn(f"…{path}")` string-spawn did.
    pub fn spawn_with_oracles(debuggee_path: &str, oracles: &[&str]) -> Self {
        let mut cmd = Command::new(BS_BINARY);
        cmd.args(["-t", "none"]);
        // Disable terminal color output so `expect_exact("Signal
        // SIGUSR1 received")` matches. bs writes ANSI color codes
        // around keywords (e.g. `Signal \x1b[38;5;13mSIGUSR1\x1b[39m
        // received`) when stdout is a TTY, and `expectrl` uses a
        // PTY, so the codes interleave with the words we want to
        // match. `NO_COLOR=1` is the [community standard](https://no-color.org).
        cmd.env("NO_COLOR", "1");
        for o in oracles {
            cmd.args(["--oracle", o]);
        }
        for piece in debuggee_path.split_whitespace() {
            cmd.arg(piece);
        }

        let mut session =
            Session::spawn(cmd).expect("spawn bs failed — was `cargo build --release` run?");
        // A generous default; individual `cmd` calls can be lower
        // when an interaction is supposed to be fast.
        let expect_timeout = Some(Duration::from_secs(30));
        session.set_expect_timeout(expect_timeout);
        session
            .expect(READY_MARKER)
            .expect("BugStalker did not print its greet banner");
        Self {
            session,
            expect_timeout,
        }
    }

    /// `Debugger.cmd(cmd, *should_see)` — send a line, then assert
    /// each `should_see` string appears in subsequent output.
    pub fn cmd(&mut self, cmd: &str, should_see: &[&str]) {
        // Send one character at a time. `bs` uses `rustyline`,
        // which reads stdin a byte at a time in raw mode and
        // re-renders the line on every keystroke (cursor moves,
        // syntax highlighting, etc.). A single `send_line` write
        // races with the renderer — empirically, only the first
        // byte of the command shows up in the expect buffer
        // before rustyline starts dropping characters somewhere
        // in its decode pipeline. Per-character writes with a
        // 1 ms gap let rustyline keep up and the full line lands.
        for b in cmd.bytes() {
            self.session
                .send([b])
                .unwrap_or_else(|e| panic!("send({cmd:?}): {e}"));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.session
            .send([b'\n'])
            .unwrap_or_else(|e| panic!("send_newline({cmd:?}): {e}"));
        for needle in should_see {
            if let Err(e) = self.session.expect(*needle) {
                // Drain everything available for ~2s so the
                // failure message includes WHAT bs printed
                // instead of `needle`. Without this the error is
                // just "timeout" and we have to bisect by hand.
                let mut collected: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                let drain_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while std::time::Instant::now() < drain_deadline {
                    match self.session.try_read(&mut buf) {
                        Ok(0) => std::thread::sleep(std::time::Duration::from_millis(50)),
                        Ok(n) => collected.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                let tail = String::from_utf8_lossy(&collected).into_owned();
                panic!(
                    "after {cmd:?}, expected {needle:?}: {e}\n\
                     --- buffer drain ({n} bytes) ---\n{tail}\n--- end ---",
                    n = collected.len(),
                );
            }
        }
    }

    /// `Debugger.cmd_re(cmd, *should_see_re)`.
    pub fn cmd_re(&mut self, cmd: &str, regexes: &[&str]) {
        for b in cmd.bytes() {
            self.session.send([b]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.session.send([b'\n']).unwrap();
        for re in regexes {
            if let Err(e) = self.session.expect(Regex(re)) {
                let mut collected: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    match self.session.try_read(&mut buf) {
                        Ok(0) => std::thread::sleep(std::time::Duration::from_millis(50)),
                        Ok(n) => collected.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                let tail = String::from_utf8_lossy(&collected).into_owned();
                panic!(
                    "after {cmd:?}, expected /{re}/: {e}\n\
                     --- buffer drain ({n} bytes) ---\n{tail}\n--- end ---",
                    n = collected.len(),
                );
            }
        }
    }

    /// `Debugger.control(char)` — send a control character (e.g.
    /// `'c'` for `^C`). Used by tests that need to interrupt a
    /// running debuggee before the `q` command can land.
    pub fn control(&mut self, c: char) {
        let code = match c {
            'c' | 'C' => expectrl::ControlCode::EndOfText,
            'd' | 'D' => expectrl::ControlCode::EndOfTransmission,
            'z' | 'Z' => expectrl::ControlCode::Substitute,
            other => panic!("unsupported control char: {other:?}"),
        };
        self.session
            .send(code)
            .unwrap_or_else(|e| panic!("send_control({c:?}): {e}"));
    }

    /// `Debugger.expect_in_output(text)` — wait for `text` in the
    /// output without sending anything first. Useful when an
    /// asynchronous event (HTTP request, signal, thread spawn) is
    /// supposed to produce output we want to gate on before issuing
    /// the next command.
    pub fn expect_in_output(&mut self, text: &str) {
        if let Err(e) = self.session.expect(text) {
            let mut collected: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                match self.session.try_read(&mut buf) {
                    Ok(0) => std::thread::sleep(std::time::Duration::from_millis(50)),
                    Ok(n) => collected.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            let tail = String::from_utf8_lossy(&collected).into_owned();
            panic!(
                "expect_in_output({text:?}): {e}\n\
                 --- buffer drain ({n} bytes) ---\n{tail}\n--- end ---",
                n = collected.len(),
            );
        }
    }

    /// Non-fatal `expect_in_output` with a custom timeout. Returns
    /// `true` if `text` appeared within `timeout`, `false` on
    /// timeout. Used by polling loops like `test_thread_switch`
    /// that step through code until a specific line is reached.
    pub fn try_expect(&mut self, text: &str, timeout: Duration) -> bool {
        let saved = self.expect_timeout;
        self.session.set_expect_timeout(Some(timeout));
        let ok = self.session.expect(text).is_ok();
        self.session.set_expect_timeout(saved);
        ok
    }

    /// `Debugger.is_alive()` — true while the bs process is still
    /// running. Used by `test_multithreaded_quit` to assert that
    /// `q` actually terminated the debugger.
    pub fn is_alive(&mut self) -> bool {
        self.session.is_alive().unwrap_or(false)
    }

    /// Attach the debugger to an already-running process by PID.
    /// Mirrors `Debugger(process=...)` in the Python helper, which
    /// pexpect-spawned the inferior first and then `bs -p <pid>`'d
    /// onto it. The caller is responsible for spawning the
    /// inferior (e.g. with `std::process::Command`).
    pub fn attach_pid(pid: u32) -> Self {
        let mut cmd = Command::new(BS_BINARY);
        cmd.args(["-t", "none", "-p", &pid.to_string()]);
        cmd.env("NO_COLOR", "1");
        let mut session = Session::spawn(cmd).expect("spawn bs failed — was `cargo build` run?");
        let expect_timeout = Some(Duration::from_secs(30));
        session.set_expect_timeout(expect_timeout);
        session
            .expect(READY_MARKER)
            .expect("BugStalker did not print its greet banner");
        Self {
            session,
            expect_timeout,
        }
    }

    /// `Debugger.print(text, *should_see)` — send `text` raw (no
    /// trailing newline) and expect each `should_see` substring.
    /// Used by tab-completion tests: `print("br\t", "break")` types
    /// `br` followed by a tab and expects the prompt to show the
    /// completion `break`.
    pub fn print(&mut self, text: &str, should_see: &[&str]) {
        for b in text.bytes() {
            self.session
                .send([b])
                .unwrap_or_else(|e| panic!("send({text:?}): {e}"));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for needle in should_see {
            self.session
                .expect(*needle)
                .unwrap_or_else(|e| panic!("after print({text:?}), expected {needle:?}: {e}"));
        }
    }

    /// `Debugger.search_in_output(pattern, line_cnt=10)` — read up
    /// to `max_lines` lines from the output and return the first
    /// capture group of the first match. Used by `test_command` to
    /// scrape breakpoint addresses out of `Hit breakpoint N at …`
    /// output and feed them back into address-based commands.
    pub fn search_in_output(&mut self, pattern: &str, max_lines: usize) -> Option<String> {
        let re = regex::Regex::new(pattern).expect("invalid regex");
        // `expectrl`'s expect with the regex needle returns the
        // captures; the first capture group is what the Python
        // helper returned. Reading line-by-line is finicky over
        // PTYs (output may not be \n-terminated), so just expect
        // the regex against the live stream.
        for _ in 0..max_lines {
            if let Ok(caps) = self.session.expect(Regex(pattern)) {
                // expectrl's `Captures` exposes the matched bytes
                // via index 0..N, but the "before" bytes give us
                // the surrounding text. Re-run our own regex over
                // the matched substring to extract group 1.
                let matched = std::str::from_utf8(caps.get(0)?).ok()?.to_string();
                if let Some(m) = re.captures(&matched)
                    && let Some(g) = m.get(1)
                {
                    return Some(g.as_str().to_string());
                }
            }
        }
        None
    }

    /// Regex variant of [`Debugger::expect_in_output`].
    pub fn expect_in_output_re(&mut self, regex: &str) {
        self.session
            .expect(Regex(regex))
            .unwrap_or_else(|e| panic!("expect_in_output_re(/{regex}/): {e}"));
    }

    /// `Debugger.debugee_process().send_signal(sig)` — port. Sends
    /// `sig` to the inferior (the child of `bs`). We discover the
    /// debuggee PID by walking `/proc/<bs_pid>/task/*/children`
    /// (Linux only — the Darwin port can be added when those tests
    /// migrate).
    #[cfg(target_os = "linux")]
    pub fn send_signal_to_debugee(&self, signal: nix::sys::signal::Signal) {
        let bs_pid: i32 = self.session.get_process().pid().as_raw();
        let pid = read_first_child_pid(bs_pid)
            .unwrap_or_else(|| panic!("no debuggee child found for bs pid {bs_pid}"));
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal)
            .unwrap_or_else(|e| panic!("kill({pid}, {signal:?}): {e}"));
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

#[cfg(target_os = "linux")]
fn read_first_child_pid(parent_pid: i32) -> Option<i32> {
    // `/proc/<pid>/task/<tid>/children` lists space-separated PIDs.
    // bs has one main thread, but in principle the debugger could
    // spawn helpers — pick the first PID we find that is the
    // *inferior* (i.e. matches the binary we asked bs to launch).
    // For our tests bs always has exactly one child so we just
    // take the first.
    let task_dir = format!("/proc/{parent_pid}/task");
    for entry in std::fs::read_dir(&task_dir).ok()?.flatten() {
        let children_path = entry.path().join("children");
        let Ok(content) = std::fs::read_to_string(&children_path) else {
            continue;
        };
        if let Some(first) = content.split_whitespace().next()
            && let Ok(pid) = first.parse::<i32>()
        {
            return Some(pid);
        }
    }
    None
}

impl Drop for Debugger {
    fn drop(&mut self) {
        // Best-effort cleanup so panicking tests don't leave the
        // debuggee subprocess pinned to a TCP port.
        let _ = self.session.send_line("q");
    }
}

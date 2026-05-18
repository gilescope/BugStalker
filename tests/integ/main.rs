// SPDX-License-Identifier: MIT
//! Phase 2 of the Python → Rust integration-test migration.
//!
//! `cargo test --test integ` runs Rust integration tests that drive
//! `bs` via a PTY. Unlike the legacy `tests/integration/*.py` runner
//! (which used `pexpect` on top of `os.forkpty()` and deadlocked when
//! the test process also spawned HTTP-client threads), this binary
//! spawns the debugger via `expectrl`, which uses `posix_spawn` and
//! is safe to call from a multi-threaded parent.
//!
//! Each `#[test]` is a full integration test: spawn debugger → spawn
//! debuggee subprocess (via the debugger) → drive scripted I/O.
//! Tests are `#[serial]` because they all share the same machine-wide
//! resources (one of the debuggees binds to TCP :3000).

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod helper;
mod test_async;
mod test_call;
mod test_command;
mod test_external;
mod test_hints;
mod test_multithread;
mod test_oracle;
mod test_pastebin;
mod test_sharedlib;
mod test_signals;
mod test_todos;
mod test_variables;
mod test_watchpoint;

// SPDX-License-Identifier: MIT
//! Test- and bench-side helpers.
//!
//! Phase 8 will lift the full `tests/debugger/` scaffolding into this
//! crate. Phase 1 seeded it with two helpers used by `benches/`:
//!
//! * [`spawn_at_breakpoint`] — install a debugger on a debuggee binary,
//!   set a breakpoint at a source line, run until it hits. Returns the
//!   live `Debugger` for the caller to query.
//! * [`capture_locals`] — at a breakpoint, snapshot every local
//!   variable into an owned `Vec<(String, Value)>` so subsequent
//!   rendering benches can run without keeping the debugger alive.
//!
//! Both panic on failure. They're for benches and tests where a
//! failure means the harness is broken, not a soft user error.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::path::Path;

use bugstalker::debugger::process::{Child, Installed};
use bugstalker::debugger::variable::dqe::{Dqe, Selector};
use bugstalker::debugger::variable::value::Value;
use bugstalker::debugger::{Debugger, DebuggerBuilder, NopHook, rust};

/// Return the harness crate version. Used by Phase 0 acceptance tests
/// to confirm the workspace member compiles and links.
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Spawn a debuggee process. Stdin/stdout/stderr are piped to a
/// background reader thread so the inferior never blocks on a full
/// pipe buffer. Mirrors `tests/debugger/main.rs::prepare_debugee_process`.
pub fn spawn_debuggee(prog: &Path) -> Child<Installed> {
    let (reader, writer) = os_pipe::pipe().expect("os_pipe");
    std::thread::spawn(move || {
        let mut stream = std::io::BufReader::new(reader);
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
        }
    });
    let runner = Child::new(
        prog.to_string_lossy().as_ref(),
        Vec::<&'static str>::new(),
        None::<&Path>,
        writer.try_clone().expect("pipe clone"),
        writer,
    );
    runner.install().expect("Child::install")
}

/// Spawn `prog` and run it until a breakpoint at `file:line` hits.
/// Returns the live `Debugger`. Caller owns it; drop ends the session.
pub fn spawn_at_breakpoint(prog: &Path, file: &str, line: u64) -> Debugger {
    let process = spawn_debuggee(prog);
    rust::Environment::init(None);
    let builder = DebuggerBuilder::new().with_hooks(NopHook {});
    let mut debugger = builder.build(process).expect("build debugger");
    debugger
        .set_breakpoint_at_line(file, line)
        .unwrap_or_else(|e| panic!("set_breakpoint_at_line {file}:{line}: {e}"));
    debugger.start_debugee().expect("start_debugee");
    debugger
}

/// Snapshot every local variable in scope as `(name, owned Value)`.
/// Useful for setting up a bench that times the renderer over a known
/// set of values without keeping the debugger session alive across
/// many iterations.
pub fn capture_locals(debugger: &Debugger) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let results = debugger
        .read_variable(Dqe::Variable(Selector::Any))
        .expect("read_variable Any");
    for qr in results {
        let name = qr
            .identity()
            .name
            .as_deref()
            .unwrap_or("<anonymous>")
            .to_string();
        out.push((name, qr.into_value()));
    }
    out
}

/// Read a single named local. Panics if not present.
pub fn capture_named(debugger: &Debugger, var: &str) -> Value {
    let dqe = Dqe::Variable(Selector::by_name(var, false));
    debugger
        .read_variable(dqe)
        .unwrap_or_else(|e| panic!("read_variable({var}): {e}"))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no result for `{var}`"))
        .into_value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_zero_zero_zero() {
        assert_eq!(version(), "0.0.0");
    }
}

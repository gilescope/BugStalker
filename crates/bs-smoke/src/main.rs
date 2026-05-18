// SPDX-License-Identifier: MIT
//! Phase 1 acceptance smoke driver.
//!
//! Drives the `examples/vars` debuggee through `bugstalker`'s public
//! library API (the same path the integration suite uses), breaks at
//! the line where every Phase 1 stdlib type is in scope, and asserts
//! each rendered value carries its specialised shape — i.e. nothing
//! falls through to a raw `{ field: ... }` struct printout. The plan's
//! acceptance criterion ("manual smoke test on a real-world crate;
//! nothing falls back to raw struct rendering") becomes a single CI
//! step.
//!
//! Run via:
//!   cargo run -p bs-smoke -- --vars examples/target/debug/vars
//!
//! The Earthly `+smoke` target wires the right paths automatically.
//!
//! Why a Rust binary using `bugstalker` as a library and not a
//! pexpect-style PTY driver: the project maintains tooling in Rust,
//! and rustyline's input handling makes PTY-based interaction
//! brittle (typing-echo races, prompt-redraw boundary detection).
//! Using the library directly is deterministic and ten times faster.

use anyhow::{Context, Result, bail};
use bugstalker::debugger::process::{Child, Installed};
use bugstalker::debugger::variable::dqe::{Dqe, Selector};
use bugstalker::debugger::{DebuggerBuilder, NopHook, rust};
use bugstalker::ui::generic::variable::render_value;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Source line in `examples/vars/src/vars.rs` where every Phase 1
/// fixture is in scope (the `nop: Option<u8> = None` anchor at the
/// end of `phase1_specs_b()`).
const VARS_LINE: u64 = 749;

struct Args {
    vars: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut vars = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--vars" => vars = Some(PathBuf::from(iter.next().context("--vars needs a value")?)),
            "-h" | "--help" => {
                eprintln!("usage: bs-smoke --vars <path-to-vars-fixture>");
                std::process::exit(0);
            }
            other => bail!("unrecognised flag: {other}"),
        }
    }
    Ok(Args {
        vars: vars.context("missing --vars")?,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if !args.vars.exists() {
        bail!("vars debuggee not found: {}", args.vars.display());
    }

    let process = spawn_debugee(&args.vars)?;
    rust::Environment::init(None);
    let builder = DebuggerBuilder::new().with_hooks(NopHook {});
    let mut debugger = builder.build(process).context("DebuggerBuilder::build")?;

    debugger
        .set_breakpoint_at_line("vars.rs", VARS_LINE)
        .with_context(|| format!("set_breakpoint_at_line vars.rs:{VARS_LINE}"))?;
    debugger.start_debugee().context("start_debugee")?;
    eprintln!("[bs-smoke] hit breakpoint at vars.rs:{VARS_LINE}");

    let mut failures: Vec<String> = Vec::new();
    let checks = checks();
    for check in checks {
        match read_one(&debugger, check.var) {
            Ok(rendered) => {
                if let Err(reason) = check.evaluate(&rendered) {
                    failures.push(format!("{}: {reason}; rendered={rendered:?}", check.var));
                } else {
                    eprintln!(
                        "[bs-smoke] {:<14} ✓  {}",
                        check.var,
                        rendered.lines().next().unwrap_or("<empty>")
                    );
                }
            }
            Err(e) => failures.push(format!("{}: read failed: {e:#}", check.var)),
        }
    }

    drop(debugger);

    if !failures.is_empty() {
        eprintln!("[bs-smoke] FAILED:");
        for f in &failures {
            eprintln!("  - {f}");
        }
        std::process::exit(1);
    }
    println!(
        "[bs-smoke] OK — {} Phase 1 specialisations rendered",
        checks.len()
    );
    Ok(())
}

/// Build a child process for the vars binary and pipe its stdout/stderr
/// to a background reader thread (so the inferior doesn't block on
/// full pipe buffers). Mirrors `tests/debugger/main.rs::prepare_debugee_process`.
fn spawn_debugee(prog: &Path) -> Result<Child<Installed>> {
    let (reader, writer) = os_pipe::pipe().context("os_pipe")?;
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
        writer.try_clone().context("pipe clone")?,
        writer,
    );
    runner.install().context("Child::install")
}

fn read_one(debugger: &bugstalker::debugger::Debugger, var_name: &str) -> Result<String> {
    let dqe = Dqe::Variable(Selector::by_name(var_name, false));
    let results = debugger
        .read_variable(dqe)
        .with_context(|| format!("read_variable({var_name})"))?;
    let qr = results
        .into_iter()
        .next()
        .with_context(|| format!("no result for `{var_name}`"))?;
    Ok(render_value(qr.value()))
}

/// One assertion per row.
struct Check {
    var: &'static str,
    /// Substring(s) the rendered output must contain. AND across all.
    must_contain: &'static [&'static str],
    /// Forbid the variable's wrapper-type name appearing as `<name> {`
    /// — that's the unmistakable signature of the default Debug-style
    /// raw-struct fallback Phase 1 was supposed to eliminate.
    forbid_wrapper_struct: Option<&'static str>,
}

impl Check {
    const fn new(
        var: &'static str,
        must_contain: &'static [&'static str],
        forbid_wrapper_struct: Option<&'static str>,
    ) -> Self {
        Self {
            var,
            must_contain,
            forbid_wrapper_struct,
        }
    }

    fn evaluate(&self, rendered: &str) -> Result<()> {
        for needle in self.must_contain {
            if !rendered.contains(needle) {
                bail!("missing substring {needle:?}");
            }
        }
        if let Some(name) = self.forbid_wrapper_struct {
            let pattern = format!("{name} {{");
            if rendered.contains(&pattern) {
                bail!("rendered as raw `{pattern}` — wrapper specialisation didn't fire");
            }
        }
        Ok(())
    }
}

fn checks() -> &'static [Check] {
    &CHECKS
}

static CHECKS: [Check; 23] = [
    // S7 — Pin<P>: pinnee transparency, no `Pin {`.
    // (S11 NonNull lives in `phase1_specs` upstream, not the
    // `phase1_specs_b` breakpoint we sit at; the integration test
    // `test_read_nonnull` already covers it.)
    Check::new("pinned_box", &[], Some("Pin")),
    Check::new("pinned_ref", &[], Some("Pin")),
    // S6 — Range family.
    Check::new("r1", &["0..10"], Some("Range")),
    Check::new("r2", &["0..=10"], Some("RangeInclusive")),
    Check::new("r3", &["5.."], Some("RangeFrom")),
    Check::new("r4", &["..10"], Some("RangeTo")),
    // S4 — Duration: human-readable units. Zero is `0s`.
    // The renderer normalises sub-second durations through the
    // largest fitting unit: 1500ms surfaces as `1.500s`, not as
    // a "1500ms" literal.
    Check::new("d_zero", &["0s"], Some("Duration")),
    Check::new("d_ms", &["1.500s"], Some("Duration")),
    Check::new("d_s", &["7s"], Some("Duration")),
    Check::new("d_h", &["1h", "1m"], Some("Duration")),
    // S12 — CString.
    Check::new("cs_hello", &["c\"hello\""], Some("CString")),
    Check::new("cs_empty", &["c\"\""], Some("CString")),
    Check::new("cs_bytes", &["\\x"], Some("CString")),
    // S13/S14 — OsString / PathBuf.
    Check::new("os_str", &["hello"], Some("OsString")),
    Check::new("pb", &["/tmp/foo"], Some("PathBuf")),
    // S10 — MaybeUninit<T>.
    Check::new("mu_init", &["99"], Some("MaybeUninit")),
    // S1 — Mutex<T> / RwLock<T>.
    Check::new("mtx", &["123"], Some("Mutex")),
    Check::new("rwl", &["456"], Some("RwLock")),
    // S2 — Mutex/RwLock guards.
    Check::new("mtx_guard", &["123"], Some("MutexGuard")),
    Check::new("rwl_read", &["456"], Some("RwLockReadGuard")),
    // O — DST companions.
    Check::new("dst_cs", &["hi"], Some("CStr")),
    Check::new("dst_os", &["hi"], Some("OsStr")),
    Check::new("dst_pa", &["/etc"], Some("Path")),
];

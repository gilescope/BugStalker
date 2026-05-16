// SPDX-License-Identifier: MIT
//! `bs --test <script.json5>` — a test runner on top of the same
//! dispatcher the `--script` JSON-RPC loop uses.
//!
//! Reads a JSON5 file, walks the requests in order, and emits TAP 14 on
//! stdout. Every `assert.*` response contributes one TAP line; every
//! other response is silent unless it failed (a non-assert error is a
//! `Bail out!` because the script's premise is broken).
//!
//! Exit code: `0` if every assertion passed and no bail-out happened,
//! `1` otherwise.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::thread;

use os_pipe::PipeReader;
use serde::Deserialize;

use crate::debugger::{Debugger, DebuggerBuilder};
use crate::ui::structured::ResponseBudget;
use crate::ui::structured::commands::assert::AssertResult;
use crate::ui::supervisor::DebugeeSource;

use super::bless;
use super::dispatch;
use super::hook::ScriptHook;
use super::transport::{OutputSink, RequestReader};

/// Mode tweaks applied to a `--test` run.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunOptions {
    /// Rewrite `expect:` blocks for mismatched assertions in place,
    /// using the actual response as the new pattern. Idempotent.
    pub bless: bool,
    /// When `bless` is set, also mask runtime-only fields (addresses)
    /// to `$regex` placeholders.
    pub address_masking: bool,
}

impl RunOptions {
    pub fn bless() -> Self {
        Self {
            bless: true,
            address_masking: true,
        }
    }
}

/// Run a script as a test. Returns the desired process exit code.
pub fn run_test(
    script_path: &Path,
    source: DebugeeSource<'_>,
    oracles: Vec<String>,
) -> anyhow::Result<i32> {
    run_test_with(script_path, source, oracles, RunOptions::default())
}

/// Run a script as a test with explicit options. Same exit-code
/// contract as [`run_test`].
pub fn run_test_with(
    script_path: &Path,
    source: DebugeeSource<'_>,
    oracles: Vec<String>,
    opts: RunOptions,
) -> anyhow::Result<i32> {
    let file = File::open(script_path)
        .map_err(|e| anyhow::anyhow!("failed to open script {:?}: {e}", script_path))?;
    let stdout = io::stdout();
    // Events from the inferior are not interesting in test mode —
    // route them into a sink that drops them. We could record + dump
    // on failure in a future iteration; the surface stays the same.
    let null_sink = OutputSink::new(io::sink());

    let (out_reader, out_writer) = os_pipe::pipe()?;
    let (err_reader, err_writer) = os_pipe::pipe()?;
    spawn_drain(out_reader);
    spawn_drain(err_reader);

    let child = source.create_child(out_writer, err_writer)?;
    let oracle_objs = crate::ui::supervisor::resolve_oracles(&oracles);

    let mut dbg: Debugger = DebuggerBuilder::new()
        .with_hooks(ScriptHook::new(null_sink))
        .with_oracles(oracle_objs)
        .build(child)?;

    let mut reader = RequestReader::new(BufReader::new(file));
    let mut out = stdout.lock();

    // TAP 14 preamble. The `1..N` line comes at the *end* so we don't
    // have to count assertions in advance — TAP allows the plan to
    // trail, which suits a streaming runner.
    writeln!(out, "TAP version 14")?;

    let mut state = RunnerState::default();
    state.bless = opts.bless;

    loop {
        match reader.read() {
            Ok(None) => break,
            Err((_id, err)) => {
                state.bail_out(&mut out, &format!("script parse error: {}", err.message))?;
                break;
            }
            Ok(Some(req)) => {
                let budget = ResponseBudget {
                    max_response_bytes: req.max_response_bytes,
                    include_timestamps: false,
                };
                let method = req.method.clone();
                let result = dispatch::run(&method, &req.params, &mut dbg, &budget);

                match (method.starts_with("assert."), result) {
                    (true, Ok(value)) => state.record_assert(&mut out, &method, value)?,
                    (true, Err(err)) => {
                        state.record_assert_error(&mut out, &method, &err.message)?
                    }
                    (false, Ok(_)) => { /* silent — driver step */ }
                    (false, Err(err)) => {
                        // A non-assert error aborts: the script's
                        // premise is broken (no breakpoint, can't run,
                        // …). Bail-out with the message.
                        state.bail_out(&mut out, &format!("{} failed: {}", method, err.message))?;
                        break;
                    }
                }
            }
        }
    }

    state.finish(&mut out)?;

    if opts.bless && !state.bailed && !state.bless_patches.is_empty() {
        // Re-read the script (it may have been touched on disk between
        // our open and now — `--bless` is interactive enough that we
        // want the latest bytes).
        let source = fs::read_to_string(script_path)?;
        let slots = bless::slots_in(&source);
        let rewritten =
            bless::apply_patches(&source, &slots, &state.bless_patches, opts.address_masking)
                .map_err(anyhow::Error::msg)?;
        if rewritten != source {
            fs::write(script_path, &rewritten)?;
            writeln!(
                out,
                "# bless: rewrote {} expectation(s) in {}",
                state.bless_patches.len(),
                script_path.display()
            )?;
        }
    }

    // Under --bless every mismatch becomes a write; the run is
    // considered successful so CI / `cargo test` don't bail.
    if opts.bless {
        return Ok(if state.bailed { 1 } else { 0 });
    }
    Ok(if state.failed() { 1 } else { 0 })
}

#[derive(Default)]
struct RunnerState {
    n: usize,
    failures: usize,
    bailed: bool,
    bless: bool,
    /// `(assert_idx, new_expect_value)` for every mismatched assertion
    /// we'd like `bless::apply_patches` to splice into the script.
    bless_patches: Vec<(usize, serde_json::Value)>,
}

impl RunnerState {
    fn failed(&self) -> bool {
        self.bailed || self.failures > 0
    }

    fn record_assert<W: Write>(
        &mut self,
        out: &mut W,
        method: &str,
        value: serde_json::Value,
    ) -> io::Result<()> {
        self.n += 1;
        // Every assert.* returns AssertResult; deserialise and decide.
        let result: AssertResult = match AssertResult::deserialize(&value) {
            Ok(r) => r,
            Err(e) => {
                self.failures += 1;
                writeln!(
                    out,
                    "not ok {} - {} (malformed assert response: {})",
                    self.n, method, e
                )?;
                return Ok(());
            }
        };
        let label = result
            .hint
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| method.to_string());
        if result.passed && !(self.bless && result.expect_was_unset) {
            // Normal pass — or, in bless mode, a pass that already had
            // a concrete expectation we shouldn't trample.
            writeln!(out, "ok {} - {}", self.n, label)?;
        } else if self.bless {
            // In bless mode every miss (and every initial-fill) is
            // silently promoted to a write; the TAP line says "ok"
            // with a `# blessed` directive so a human running the
            // script sees the change pending.
            self.bless_patches.push((self.n - 1, result.got.clone()));
            writeln!(out, "ok {} - {} # blessed", self.n, label)?;
        } else {
            self.failures += 1;
            writeln!(out, "not ok {} - {}", self.n, label)?;
            // TAP14 YAML block carries the diff.
            if let Some(m) = &result.mismatch {
                writeln!(out, "  ---")?;
                writeln!(out, "  message: {}", quote_yaml(&m.reason))?;
                writeln!(out, "  path: {}", quote_yaml(&m.path))?;
                writeln!(
                    out,
                    "  expected: {}",
                    serde_json::to_string(&m.expected).unwrap_or_default()
                )?;
                writeln!(
                    out,
                    "  got: {}",
                    serde_json::to_string(&m.got).unwrap_or_default()
                )?;
                writeln!(out, "  ...")?;
            }
        }
        Ok(())
    }

    fn record_assert_error<W: Write>(
        &mut self,
        out: &mut W,
        method: &str,
        message: &str,
    ) -> io::Result<()> {
        self.n += 1;
        self.failures += 1;
        writeln!(out, "not ok {} - {} (dispatch error)", self.n, method)?;
        writeln!(out, "  ---")?;
        writeln!(out, "  message: {}", quote_yaml(message))?;
        writeln!(out, "  ...")?;
        Ok(())
    }

    fn bail_out<W: Write>(&mut self, out: &mut W, message: &str) -> io::Result<()> {
        self.bailed = true;
        writeln!(out, "Bail out! {}", message)?;
        Ok(())
    }

    fn finish<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if !self.bailed {
            writeln!(out, "1..{}", self.n)?;
        }
        Ok(())
    }
}

/// Quote a string for YAML inside the TAP 14 diagnostic block. We only
/// quote single-line strings; the matcher's `reason`/`path` fields are
/// always single-line by construction.
fn quote_yaml(s: &str) -> String {
    if s.is_empty()
        || s.contains(['"', '\\', '\n', ':'])
        || s.starts_with(['&', '*', '#', '?', '|', '<', '>', '=', '!', '%', '@', '`'])
    {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn spawn_drain(mut reader: PipeReader) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_yaml_passes_simple_strings_through() {
        assert_eq!(quote_yaml("simple"), "simple");
        assert_eq!(quote_yaml("with spaces"), "with spaces");
    }

    #[test]
    fn quote_yaml_quotes_special_chars() {
        assert_eq!(quote_yaml("with: colon"), r#""with: colon""#);
        assert_eq!(quote_yaml("with \"quote\""), r#""with \"quote\"""#);
    }

    #[test]
    fn quote_yaml_quotes_leading_special() {
        assert_eq!(quote_yaml("@start"), r#""@start""#);
    }
}

// SPDX-License-Identifier: MIT
//! Top-level entry points for `--script` and `--describe-commands`.
//!
//! `run_script` owns the request loop. `run_describe` is the one-shot
//! schema-dump path.

use std::io::{self, Read, Write};
use std::thread;

use os_pipe::PipeReader;

use crate::debugger::{Debugger, DebuggerBuilder};
use crate::ui::structured::{ResponseBudget, schema};
use crate::ui::supervisor::DebugeeSource;

use super::dispatch;
use super::hook::ScriptHook;
use super::transport::{OutputSink, RequestReader};

/// Pure metadata mode: print the JSON Schema catalogue and exit.
pub fn run_describe<W: Write>(out: &mut W) -> anyhow::Result<()> {
    let descriptor = schema::describe_all(dispatch::catalogue());
    serde_json::to_writer_pretty(&mut *out, &descriptor)?;
    writeln!(out)?;
    Ok(())
}

/// Long-running JSON-RPC loop driving a single debuggee.
pub fn run_script(
    source: DebugeeSource<'_>,
    oracles: Vec<String>,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let sink = OutputSink::new(stdout);
    let event_sink = sink.clone();

    let (out_reader, out_writer) = os_pipe::pipe()?;
    let (err_reader, err_writer) = os_pipe::pipe()?;
    // Inferior stdout/stderr are routed into pipes that we MUST drain —
    // dropping the read end immediately would make the inferior SIGPIPE
    // on its first write. v1 quietly discards the bytes; v2 will surface
    // them as `output` events so agents can see what the debuggee
    // printed.
    spawn_drain(out_reader);
    spawn_drain(err_reader);

    let child = source.create_child(out_writer, err_writer)?;
    let oracle_objs = crate::ui::supervisor::resolve_oracles(&oracles);

    let mut dbg: Debugger = DebuggerBuilder::new()
        .with_hooks(ScriptHook::new(event_sink))
        .with_oracles(oracle_objs)
        .build(child)?;

    let stdin = io::stdin();
    let mut reader = RequestReader::new(stdin.lock());

    // The EventHook fires `ProcessInstalled` itself when the inferior is
    // installed. No need to emit a synthetic copy here.

    loop {
        match reader.read() {
            Ok(Some(req)) => {
                let budget = ResponseBudget {
                    max_response_bytes: req.max_response_bytes,
                    include_timestamps: false,
                };

                let result = dispatch::run(&req.method, &req.params, &mut dbg, &budget);

                // Notifications (no id) get no response — JSON-RPC 2.0
                // spec, §4.1. Errors on notifications are silently
                // dropped; the agent had no way to identify them anyway.
                let Some(id) = req.id else {
                    continue;
                };

                match result {
                    Ok(value) => sink.emit_response_ok(id, value),
                    Err(err) => sink.emit_response_err(id, err),
                }
            }
            Ok(None) => break,
            Err((id, err)) => {
                sink.emit_response_err(id, err);
                // Continue rather than abort — one malformed request
                // shouldn't kill the session.
            }
        }
    }

    Ok(())
}

/// Drain a pipe to /dev/null in a background thread. Holds the read end
/// open for the lifetime of the thread, which is the lifetime of the
/// pipe writer the inferior holds; SIGPIPE is suppressed because there
/// is always a reader.
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

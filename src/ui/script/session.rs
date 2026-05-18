// SPDX-License-Identifier: MIT
//! Top-level entry points for `--script` and `--describe-commands`.
//!
//! `run_script` owns the request loop. `run_describe` is the one-shot
//! schema-dump path.

use std::io::{self, Read, Write};
use std::thread;

use os_pipe::PipeReader;

use crate::debugger::{Debugger, DebuggerBuilder};
use crate::ui::structured::event::{Event, OutputStream};
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
pub fn run_script(source: DebugeeSource<'_>, oracles: Vec<String>) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let sink = OutputSink::new(stdout);
    let event_sink = sink.clone();

    let (out_reader, out_writer) = os_pipe::pipe()?;
    let (err_reader, err_writer) = os_pipe::pipe()?;
    // Inferior stdout/stderr are routed into pipes that we MUST drain —
    // dropping the read end immediately would make the inferior SIGPIPE
    // on its first write. Each line becomes an `output` event on the
    // JSON-RPC notification stream so script agents can assert on
    // what the debuggee printed (the EnC demo's "I patched compute,
    // now show me it prints 15" needs this to verify end-to-end).
    spawn_output_forwarder(out_reader, OutputStream::Stdout, sink.clone());
    spawn_output_forwarder(err_reader, OutputStream::Stderr, sink.clone());

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

/// Forward a pipe's bytes to JSON-RPC `output` events, one event
/// per `\n`-terminated line. Holds the read end open for the
/// thread's lifetime so the inferior never sees SIGPIPE.
///
/// Line-buffered: a partial trailing chunk is held back until the
/// next newline or EOF. The byte stream is decoded as UTF-8 with
/// `from_utf8_lossy` so a binary blob doesn't kill the forwarder
/// — bad bytes land as the `U+FFFD` replacement character. Most
/// debuggees print UTF-8 text.
fn spawn_output_forwarder(mut reader: PipeReader, stream: OutputStream, sink: OutputSink) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut leftover: Vec<u8> = Vec::new();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            leftover.extend_from_slice(&buf[..n]);
            // Emit each complete line; keep any trailing partial
            // line in `leftover` for the next read.
            while let Some(nl) = leftover.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = leftover.drain(..=nl).collect();
                // Strip the trailing `\n` (and optional `\r` for
                // CRLF) so the event payload is the line content.
                let mut end = line.len() - 1; // past the `\n`
                if end > 0 && line[end - 1] == b'\r' {
                    end -= 1;
                }
                let text = String::from_utf8_lossy(&line[..end]).into_owned();
                sink.emit_event(Event::Output { stream, data: text });
            }
        }
        // EOF: flush any non-newline-terminated tail. Debuggees
        // that exit without a final `\n` (e.g. crashed mid-print)
        // shouldn't have their last words swallowed.
        if !leftover.is_empty() {
            let text = String::from_utf8_lossy(&leftover).into_owned();
            sink.emit_event(Event::Output { stream, data: text });
        }
    });
}

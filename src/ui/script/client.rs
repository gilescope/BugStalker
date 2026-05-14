// SPDX-License-Identifier: MIT
//! Type-safe Rust client for `bs --script`.
//!
//! Spawns a `bs --script <debuggee>` subprocess, owns its stdin/stdout
//! pipes, and exposes a single typed entry point:
//!
//! ```no_run
//! use bugstalker::ui::script::client::ScriptClient;
//! use bugstalker::ui::structured::commands::{
//!     r#break::{BreakSet, Location},
//!     print_var::Var,
//!     run::Run,
//! };
//!
//! # fn run() -> anyhow::Result<()> {
//! let mut bs = ScriptClient::spawn("./target/debug/bs", "./my-app")?;
//! bs.call(BreakSet { at: Location::Shorthand("main.rs:42".into()), deferred: false })?;
//! let stop = bs.call(Run::default())?;
//! let dyn_ref = bs.call(Var { name: Some("dyn_ref".into()), expression: None })?;
//! println!("stopped at {}: dyn_ref = {}", stop.address, dyn_ref.items[0].value_text);
//! # Ok(()) }
//! ```
//!
//! The wire is one minified JSON object per line. The client reads
//! lines, distinguishes responses (have an `id`) from events (don't),
//! and queues events into a buffer agents can drain at their own pace.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::ui::structured::StructuredCommand;
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::event::Event;

/// Errors a client call can produce.
#[derive(Debug)]
pub enum ClientError {
    /// The server returned an error response.
    Server(BsError),
    /// I/O failed (subprocess died, pipe broke).
    Io(std::io::Error),
    /// The server's stdout closed mid-call. The child may still hold
    /// state — call `shutdown` to reap it.
    UnexpectedEof,
    /// The server emitted a line we couldn't parse as JSON-RPC.
    Wire(String),
    /// Response value did not deserialise into the expected `C::Response`.
    Deserialise(serde_json::Error, serde_json::Value),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Server(e) => write!(f, "server: {e}"),
            ClientError::Io(e) => write!(f, "io: {e}"),
            ClientError::UnexpectedEof => f.write_str("subprocess stdout closed unexpectedly"),
            ClientError::Wire(s) => write!(f, "malformed wire data: {s}"),
            ClientError::Deserialise(e, v) => write!(f, "deserialise: {e} (value: {v})"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Io(e) => Some(e),
            ClientError::Deserialise(e, _) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<BsError> for ClientError {
    fn from(e: BsError) -> Self {
        ClientError::Server(e)
    }
}

pub type ClientResult<T> = Result<T, ClientError>;

/// One JSON-RPC subprocess + its in-memory event queue.
///
/// `stdin` is held inside an `Option` so `shutdown` can drop it (sending
/// EOF to the server) while the rest of the struct is still needed for
/// the final reaping loop. Outside `shutdown` it is always `Some`.
pub struct ScriptClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
    events: Vec<Event>,
}

impl ScriptClient {
    /// Spawn `bs --script <debuggee>`. Inherits stderr; agents that want
    /// the bs log noise out of the way can wrap the call to redirect.
    pub fn spawn(bs_binary: impl AsRef<Path>, debuggee: impl AsRef<Path>) -> ClientResult<Self> {
        Self::spawn_with(bs_binary, debuggee, &[])
    }

    /// Same as `spawn`, with extra `bs` CLI args (e.g. `["-o", "tokio"]`
    /// to enable an oracle).
    pub fn spawn_with(
        bs_binary: impl AsRef<Path>,
        debuggee: impl AsRef<Path>,
        extra_args: &[&str],
    ) -> ClientResult<Self> {
        let mut child = Command::new(bs_binary.as_ref())
            .arg("--script")
            .args(extra_args)
            .arg(debuggee.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .expect("Stdio::piped() always installs stdin");
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .expect("Stdio::piped() always installs stdout"),
        );

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: AtomicU64::new(1),
            events: Vec::new(),
        })
    }

    fn stdin_mut(&mut self) -> ClientResult<&mut ChildStdin> {
        self.stdin.as_mut().ok_or_else(|| {
            ClientError::Wire(
                "client has been shut down; spawn a new ScriptClient to continue".into(),
            )
        })
    }

    /// Connect to a `bs` binary discovered via the `BS_BIN` environment
    /// variable, or fall back to `which bs`. Convenient for in-tree
    /// integration tests.
    pub fn spawn_default(debuggee: impl AsRef<Path>) -> ClientResult<Self> {
        let bs = std::env::var_os("BS_BIN").map(PathBuf::from).unwrap_or_else(|| {
            // Walk up from CARGO_MANIFEST_DIR looking for target/debug/bs.
            // Falls back to `bs` on PATH if not found.
            let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
            if let Some(start) = manifest {
                for ancestor in start.ancestors() {
                    let candidate = ancestor.join("target/debug/bs");
                    if candidate.exists() {
                        return candidate;
                    }
                }
            }
            PathBuf::from("bs")
        });
        Self::spawn(bs, debuggee)
    }

    /// Send a typed request, await the response, deserialise to
    /// `C::Response`. Events received while waiting are queued.
    pub fn call<C: StructuredCommand>(&mut self, request: C) -> ClientResult<C::Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = WireRequest {
            jsonrpc: "2.0",
            id,
            method: C::METHOD,
            params: serde_json::to_value(&request)
                .map_err(|e| ClientError::Wire(format!("serialise request: {e}")))?,
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|e| ClientError::Wire(format!("serialise envelope: {e}")))?;
        {
            let stdin = self.stdin_mut()?;
            writeln!(stdin, "{line}")?;
            stdin.flush()?;
        }

        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf)?;
            if n == 0 {
                return Err(ClientError::UnexpectedEof);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| ClientError::Wire(format!("non-JSON line: {trimmed:?} ({e})")))?;
            // Notifications (events) are interleaved with responses on
            // the wire. Queue them and keep looking for the matching id.
            if let Some(method) = value.get("method").and_then(|m| m.as_str())
                && method == "event"
            {
                let params = value.get("params").cloned().unwrap_or(serde_json::Value::Null);
                let ev: Event = serde_json::from_value(params.clone())
                    .map_err(|e| ClientError::Deserialise(e, params))?;
                self.events.push(ev);
                continue;
            }
            // Otherwise it must be our response; verify the id matches.
            let response_id = value.get("id").and_then(|i| i.as_u64());
            if response_id != Some(id) {
                return Err(ClientError::Wire(format!(
                    "expected response for id {id}, got line: {trimmed}"
                )));
            }
            if let Some(err_obj) = value.get("error") {
                return Err(ClientError::Server(parse_server_error(err_obj)));
            }
            let result = value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return serde_json::from_value::<C::Response>(result.clone())
                .map_err(|e| ClientError::Deserialise(e, result));
        }
    }

    /// Untyped call escape hatch. Use when the wire has gained a method
    /// the Rust DTOs don't yet expose, or when writing negative tests
    /// that target an unknown method.
    pub fn call_raw(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> ClientResult<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = WireRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|e| ClientError::Wire(format!("serialise envelope: {e}")))?;
        {
            let stdin = self.stdin_mut()?;
            writeln!(stdin, "{line}")?;
            stdin.flush()?;
        }

        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf)?;
            if n == 0 {
                return Err(ClientError::UnexpectedEof);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| ClientError::Wire(format!("non-JSON line: {trimmed:?} ({e})")))?;
            if let Some(method) = value.get("method").and_then(|m| m.as_str())
                && method == "event"
            {
                let params = value.get("params").cloned().unwrap_or(serde_json::Value::Null);
                if let Ok(ev) = serde_json::from_value::<Event>(params) {
                    self.events.push(ev);
                }
                continue;
            }
            let response_id = value.get("id").and_then(|i| i.as_u64());
            if response_id != Some(id) {
                return Err(ClientError::Wire(format!(
                    "expected response for id {id}, got line: {trimmed}"
                )));
            }
            if let Some(err_obj) = value.get("error") {
                return Err(ClientError::Server(parse_server_error(err_obj)));
            }
            return Ok(value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    /// Drain the queue of events received so far. Empties the buffer.
    /// Events arrive between `call`s as well — call `pump_events` to
    /// pull any that landed without an active call.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Non-blocking read of any pending lines that are events.
    /// Returns the events appended this round.
    pub fn pump_events(&mut self) -> ClientResult<Vec<Event>> {
        // For simplicity v1 returns empty; the read_line above is
        // blocking so there is no way to peek without OS-specific
        // non-blocking IO. Agents can inspect `drain_events` after a
        // `call`. v2 will switch to a background thread that pushes into
        // a `mpsc::Receiver`.
        Ok(Vec::new())
    }

    /// Send a notification (no `id`, no response). Use for fire-and-forget
    /// requests — JSON-RPC 2.0 §4.1.
    pub fn notify<C: StructuredCommand>(&mut self, request: C) -> ClientResult<()> {
        let envelope = WireNotification {
            jsonrpc: "2.0",
            method: C::METHOD,
            params: serde_json::to_value(&request)
                .map_err(|e| ClientError::Wire(format!("serialise request: {e}")))?,
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|e| ClientError::Wire(format!("serialise envelope: {e}")))?;
        let stdin = self.stdin_mut()?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(())
    }

    /// Close stdin (signalling EOF to the server) and wait for the
    /// subprocess to exit. Subsequent `call`s return `Wire` errors.
    /// Returns the child exit status. Idempotent.
    pub fn shutdown(&mut self) -> ClientResult<std::process::ExitStatus> {
        // Send EOF.
        self.stdin = None;
        // Drain any remaining stdout into the event queue. Best-effort.
        let mut line = String::new();
        loop {
            line.clear();
            match self.stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim())
                        && v.get("method").and_then(|m| m.as_str()) == Some("event")
                        && let Some(params) = v.get("params")
                        && let Ok(ev) = serde_json::from_value::<Event>(params.clone())
                    {
                        self.events.push(ev);
                    }
                }
            }
        }
        Ok(self.child.wait()?)
    }

    /// Return the spawned child process pid. Useful for diagnostics.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ScriptClient {
    fn drop(&mut self) {
        // Best-effort cleanup if the user didn't call shutdown(): kill
        // the child to avoid orphans. EOF on stdin should be enough but
        // a hung debugger session shouldn't outlive the test.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Serialize)]
struct WireNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: serde_json::Value,
}

fn parse_server_error(obj: &serde_json::Value) -> BsError {
    let code = obj.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603) as i32;
    let message = obj
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("(server error without message)")
        .to_string();
    let data = obj.get("data").cloned();
    BsError {
        code: ErrorCode::from_wire(code),
        message,
        data,
    }
}

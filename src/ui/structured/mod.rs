// SPDX-License-Identifier: MIT
//! Structured-command core for the Phase 9 AI-bot scripting front-end.
//!
//! See `doc/plans/phase-9-ai-bot-scripting.md`. This layer is a parallel
//! dispatch over the same `Debugger` instance the console drives, but it
//! never produces a human-shaped string: every command returns a
//! typed, JSON-serialisable response or a `BsError` envelope.
//!
//! The transport that reads requests and writes responses lives in
//! `crate::ui::script`. This module is transport-agnostic — DAP could
//! delegate `customRequest`s to it just as easily as the JSON-RPC
//! front-end does.

pub mod commands;
pub mod envelope;
pub mod error;
pub mod event;
pub mod schema;

pub use envelope::{Cursor, ListResponse, ResponseBudget};
pub use error::{BsError, ErrorCode};
pub use event::Event;
pub use schema::{CommandDescriptor, describe_all};

use crate::debugger::Debugger;

/// One side of the dispatch table. Every script-visible command implements
/// this trait. The transport reads `params` as JSON, deserialises it into
/// `Self`, runs `execute`, and serialises the response.
///
/// The same trait is the client-side contract too: a Rust embedder using
/// `crate::ui::script::client::ScriptClient` calls `client.call(req)`
/// where `req: C: StructuredCommand`, and gets back a typed
/// `C::Response`. The `execute` method is irrelevant on the client side
/// (no `Debugger`); the bounds are written so client crates don't have
/// to call it.
pub trait StructuredCommand:
    serde::Serialize + serde::de::DeserializeOwned + schemars::JsonSchema
{
    /// Method name on the wire (`"break.set"`, `"var"`, etc.).
    const METHOD: &'static str;

    /// One-line description used by `--describe-commands`.
    const SUMMARY: &'static str;

    /// The reply payload type. Round-trips through serde_json (Serialize
    /// for the server, Deserialize for the client) and is schema-able
    /// for `--describe-commands`.
    type Response: serde::Serialize + serde::de::DeserializeOwned + schemars::JsonSchema;

    /// Run the command. The trait is intentionally infallible at the
    /// signature level — fallible work returns `Err(BsError)`. The
    /// transport never sees mixed Ok/Err state.
    fn execute(self, dbg: &mut Debugger, budget: &ResponseBudget)
    -> Result<Self::Response, BsError>;
}

/// Macro-style shortcut for the dispatch table. Each invocation generates:
///   - a method-name → handler pair for the request loop, and
///   - a method-name → schema entry for `--describe-commands`.
///
/// Adding a new command is one line in `crate::ui::script::dispatch::table`
/// plus an `impl StructuredCommand` in `commands::*`.
pub fn dispatch_one<C: StructuredCommand>(
    dbg: &mut Debugger,
    budget: &ResponseBudget,
    raw_params: &serde_json::Value,
) -> Result<serde_json::Value, BsError> {
    let cmd: C = serde_json::from_value(raw_params.clone()).map_err(|e| BsError {
        code: ErrorCode::InvalidParams,
        message: format!("invalid params for {}: {e}", C::METHOD),
        data: None,
    })?;
    let resp = cmd.execute(dbg, budget)?;
    serde_json::to_value(&resp).map_err(|e| BsError {
        code: ErrorCode::Internal,
        message: format!("failed to serialise response for {}: {e}", C::METHOD),
        data: None,
    })
}

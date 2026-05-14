// SPDX-License-Identifier: MIT
//! `--describe-commands` JSON Schema dump.
//!
//! Each command contributes one `CommandDescriptor` to the catalogue. The
//! transport handles `--describe-commands` by serialising
//! `describe_all()` to stdout and exiting.

use schemars::{JsonSchema, schema_for};
use serde::Serialize;

use super::StructuredCommand;
use super::event::Event;

#[derive(Debug, Serialize)]
pub struct CommandDescriptor {
    pub method: &'static str,
    pub summary: &'static str,
    pub params: schemars::schema::RootSchema,
    pub result: schemars::schema::RootSchema,
}

impl CommandDescriptor {
    pub fn for_command<C: StructuredCommand>() -> Self {
        Self {
            method: C::METHOD,
            summary: C::SUMMARY,
            params: schema_for!(C),
            result: schema_for!(C::Response),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DescribeOutput {
    /// JSON-RPC version this server speaks.
    pub jsonrpc: &'static str,
    /// Server name + version. Pinning here lets agents detect upgrades.
    pub server: ServerInfo,
    /// Schema for the per-request envelope (id, method, params,
    /// optional max_response_bytes hint).
    pub envelope: EnvelopeSchema,
    /// Schema for the standard error object every method may return.
    pub error: schemars::schema::RootSchema,
    /// Schema for the event stream.
    pub event: schemars::schema::RootSchema,
    /// Per-method schemas.
    pub methods: Vec<CommandDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
pub struct EnvelopeSchema {
    pub request: schemars::schema::RootSchema,
    pub response: schemars::schema::RootSchema,
    pub notification: schemars::schema::RootSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WireRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id. Numbers and strings are both allowed; agents MAY
    /// omit it for notifications, in which case no response is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    /// Method name; see `methods[].method` for the catalogue.
    pub method: String,
    /// Method-specific parameters; see `methods[].params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// Optional truncation hint. Server reports `truncated: true` early
    /// if it would otherwise exceed this byte budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WireResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    /// Exactly one of `result` or `error` is populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WireError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WireNotification {
    pub jsonrpc: String,
    /// Always `"event"` for the BugStalker scripting front-end.
    pub method: String,
    pub params: super::event::Event,
}

/// Build the full catalogue. The transport calls this once for
/// `--describe-commands` and once on connect for any future
/// `initialize` handshake.
pub fn describe_all(methods: Vec<CommandDescriptor>) -> DescribeOutput {
    DescribeOutput {
        jsonrpc: "2.0",
        server: ServerInfo {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
        },
        envelope: EnvelopeSchema {
            request: schema_for!(WireRequest),
            response: schema_for!(WireResponse),
            notification: schema_for!(WireNotification),
        },
        error: schema_for!(super::error::BsError),
        event: schema_for!(Event),
        methods,
    }
}

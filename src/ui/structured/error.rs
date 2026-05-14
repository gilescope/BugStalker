// SPDX-License-Identifier: MIT
//! Single error envelope for the structured front-end.
//!
//! Codes are stable across versions; messages can evolve. Agents pin on
//! `code` (machine) and surface `message` (human / debug). Optional `data`
//! carries structured detail (file/line, expected/actual, etc.).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable error code. Adding new variants is allowed within a major
/// version; renaming or removing existing variants is a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// JSON-RPC parse error (malformed JSON5).
    ParseError,
    /// Method name unknown to this server.
    MethodNotFound,
    /// `params` shape did not match the method's schema.
    InvalidParams,
    /// Internal debugger error not better-classified.
    Internal,

    // Domain-specific codes —
    /// No debuggee process running.
    ProcessNotStarted,
    /// Debuggee already exited.
    ProcessExited,
    /// Variable / argument not found in scope.
    VarNotFound,
    /// Frame number out of range.
    FrameNotFound,
    /// Thread number / pid not found.
    ThreadNotFound,
    /// Breakpoint number not found.
    BreakpointNotFound,
    /// Source location did not resolve to a code address.
    UnresolvedLocation,
    /// Watchpoint number not found.
    WatchpointNotFound,
    /// User asked to step / continue but the operation hit a non-stop
    /// terminator (process exited, was signalled, etc.). The `data` field
    /// carries the stop reason.
    StoppedAbnormally,
    /// Operation refused because no replay session is loaded.
    NoReplaySession,
    /// Operation requires an active focused thread but none is available.
    NoFocusedThread,
    /// User passed an address / hex value that did not parse.
    BadAddress,
    /// Expression / DQE failed to parse.
    BadExpression,
}

/// The single error envelope returned by every structured command.
///
/// Mapped to JSON-RPC `error` objects by the transport (`code` → integer
/// code via `ErrorCode::wire_code`, `message` → `message`, `data` →
/// `data`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BsError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl BsError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl std::fmt::Display for BsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for BsError {}

impl ErrorCode {
    /// Best-effort inverse of `wire_code`. Unknown integer codes map to
    /// `Internal` so client code never has to handle a "?" variant.
    /// Used by `crate::ui::script::client` when parsing error responses
    /// from the wire.
    pub fn from_wire(code: i32) -> ErrorCode {
        match code {
            -32700 => ErrorCode::ParseError,
            -32601 => ErrorCode::MethodNotFound,
            -32602 => ErrorCode::InvalidParams,
            -32603 => ErrorCode::Internal,
            -32001 => ErrorCode::ProcessNotStarted,
            -32002 => ErrorCode::ProcessExited,
            -32010 => ErrorCode::VarNotFound,
            -32011 => ErrorCode::FrameNotFound,
            -32012 => ErrorCode::ThreadNotFound,
            -32013 => ErrorCode::BreakpointNotFound,
            -32014 => ErrorCode::UnresolvedLocation,
            -32015 => ErrorCode::WatchpointNotFound,
            -32016 => ErrorCode::StoppedAbnormally,
            -32017 => ErrorCode::NoReplaySession,
            -32018 => ErrorCode::NoFocusedThread,
            -32019 => ErrorCode::BadAddress,
            -32020 => ErrorCode::BadExpression,
            _ => ErrorCode::Internal,
        }
    }

    /// JSON-RPC numeric code. Domain-specific codes use the
    /// implementation-defined range (-32099..-32000) per JSON-RPC 2.0.
    pub fn wire_code(self) -> i32 {
        match self {
            // Reserved JSON-RPC codes.
            ErrorCode::ParseError => -32700,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::Internal => -32603,
            // Implementation-defined.
            ErrorCode::ProcessNotStarted => -32001,
            ErrorCode::ProcessExited => -32002,
            ErrorCode::VarNotFound => -32010,
            ErrorCode::FrameNotFound => -32011,
            ErrorCode::ThreadNotFound => -32012,
            ErrorCode::BreakpointNotFound => -32013,
            ErrorCode::UnresolvedLocation => -32014,
            ErrorCode::WatchpointNotFound => -32015,
            ErrorCode::StoppedAbnormally => -32016,
            ErrorCode::NoReplaySession => -32017,
            ErrorCode::NoFocusedThread => -32018,
            ErrorCode::BadAddress => -32019,
            ErrorCode::BadExpression => -32020,
        }
    }
}

impl From<crate::debugger::Error> for BsError {
    fn from(err: crate::debugger::Error) -> Self {
        use crate::debugger::Error as DErr;
        let code = match &err {
            DErr::ProcessNotStarted => ErrorCode::ProcessNotStarted,
            DErr::ProcessExit(_) => ErrorCode::ProcessExited,
            DErr::FrameNotFound(_) => ErrorCode::FrameNotFound,
            _ => ErrorCode::Internal,
        };
        BsError {
            code,
            message: format!("{err}"),
            data: None,
        }
    }
}

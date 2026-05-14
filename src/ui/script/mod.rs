// SPDX-License-Identifier: MIT
//! JSON-RPC 2.0 stdin/stdout front-end for AI-bot driving.
//!
//! See `doc/plans/phase-9-ai-bot-scripting.md`. This is the transport
//! layer; the command logic lives in `crate::ui::structured`.
//!
//! Wire format
//! -----------
//! - **Input** is one or more JSON5 values per line, or one large JSON5
//!   value spanning multiple lines (the parser auto-detects). JSON5
//!   permits `// line comments`, `/* block */` and trailing commas, so a
//!   script file is human-editable.
//! - **Output** is one minified JSON object per line. Responses carry
//!   `id` matching the request; events use the `"event"` method and no
//!   `id`.
//! - One transport per process. Concurrent inferiors are out of scope for
//!   v1 — spawn one BugStalker per debuggee.

pub mod client;
pub mod dispatch;
pub mod hook;
pub mod session;
pub mod transport;

pub use client::{ClientError, ClientResult, ScriptClient};
pub use session::run_describe;
pub use session::run_script;

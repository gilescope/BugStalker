// SPDX-License-Identifier: MIT
//! Server-initiated events. Distinct from command responses: events have
//! no `id`, are emitted whenever the debuggee state changes, and
//! interleave freely with responses on the wire.
//!
//! See `EventHook` in `crate::debugger` — `crate::ui::script::hook` is
//! the adapter that fans `EventHook` calls out as JSON-RPC notifications.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All event variants carried as the JSON-RPC notification's `params`.
/// `method` on the wire is `"event"`; the `kind` discriminator inside
/// `params` distinguishes variants so agents can dispatch on a single
/// field.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Debuggee paused at a user breakpoint.
    BreakpointHit {
        breakpoint_id: u32,
        thread: i32,
        frame: Option<EventFrame>,
    },
    /// Debuggee paused after a step command finished.
    Step {
        thread: i32,
        frame: Option<EventFrame>,
    },
    /// Debuggee stopped on an OS signal (SIGSEGV, SIGINT, etc.).
    Signal {
        signal: i32,
        signal_name: String,
    },
    /// Debuggee exited.
    Exit { code: i32 },
    /// Watchpoint activated.
    WatchpointHit {
        watchpoint_id: u32,
        thread: i32,
        condition: String,
        dqe: Option<String>,
        old_value: Option<String>,
        new_value: Option<String>,
        end_of_scope: bool,
    },
    /// Async-step finished on the focused task.
    AsyncStep {
        task_id: u64,
        task_completed: bool,
        frame: Option<EventFrame>,
    },
    /// Debuggee process installed (initial start or restart).
    ProcessInstalled { pid: i32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventFrame {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub address: String,
}

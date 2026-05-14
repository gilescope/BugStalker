// SPDX-License-Identifier: MIT
//! `bt` — backtrace of a thread.
//!
//! Default behaviour is "current focused thread", matching the console.
//! `params: { all: true }` returns one entry per running thread.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::debugger::address::RelocatedAddress;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Backtrace {
    /// If true, return one backtrace per running thread. Default: only
    /// the currently focused thread.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Frame {
    pub function: Option<String>,
    pub address: String,
    pub function_start: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ThreadBacktrace {
    pub thread: super::ThreadDto,
    pub frames: Vec<Frame>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BacktraceResponse {
    pub threads: Vec<ThreadBacktrace>,
}

impl StructuredCommand for Backtrace {
    const METHOD: &'static str = "bt";
    const SUMMARY: &'static str = "Stack backtrace of one or all threads";
    type Response = BacktraceResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let threads = if self.all {
            dbg.thread_state()?
        } else {
            // Single-thread backtrace via the focused tracee.
            let pid = dbg.ecx().pid_on_focus();
            let bt = dbg.backtrace(pid)?;
            let tracees = dbg.thread_state()?;
            let focused = tracees
                .into_iter()
                .find(|t| t.thread.pid == pid)
                .map(|mut snap| {
                    snap.bt = Some(bt);
                    snap
                });
            focused.into_iter().collect()
        };

        let response = BacktraceResponse {
            threads: threads
                .into_iter()
                .map(|snap| ThreadBacktrace {
                    thread: super::ThreadDto::from(&snap.thread),
                    frames: snap
                        .bt
                        .unwrap_or_default()
                        .into_iter()
                        .map(|f| Frame {
                            function: f.func_name,
                            address: hex(f.ip),
                            function_start: f.fn_start_ip.map(hex),
                            file: f.place.as_ref().map(|p| p.file.display().to_string()),
                            line: f.place.as_ref().map(|p| p.line_number),
                        })
                        .collect(),
                })
                .collect(),
        };
        Ok(response)
    }
}

fn hex(addr: RelocatedAddress) -> String {
    format!("0x{:x}", addr.as_u64())
}

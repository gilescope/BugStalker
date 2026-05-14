// SPDX-License-Identifier: MIT
//! Per-command `StructuredCommand` impls. One module per command family.
//!
//! Each module declares its request struct, its response struct, and a
//! `StructuredCommand` impl. The catalogue is assembled in
//! `crate::ui::script::dispatch`.

pub mod backtrace;
pub mod r#break;
pub mod r#continue;
pub mod frame;
pub mod print_var;
pub mod run;
pub mod sharedlib;
pub mod step;
pub mod thread;
pub mod watch;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::{Tracee, TraceeStatus};

/// Common DTOs reused across responses.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadDto {
    pub number: u32,
    pub pid: i32,
    pub status: String,
}

impl From<&Tracee> for ThreadDto {
    fn from(t: &Tracee) -> Self {
        Self {
            number: t.number,
            pid: t.pid.as_raw(),
            status: format_status(&t.status),
        }
    }
}

fn format_status(s: &TraceeStatus) -> String {
    // The runtime enum uses Debug for display in the console — mirror
    // that here. Stable across versions because TraceeStatus is part of
    // the public schema.
    format!("{s:?}")
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StopReason {
    pub kind: StopKind,
    pub thread: i32,
    pub address: String,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopKind {
    Breakpoint,
    Step,
    Signal,
    Exit,
    Watchpoint,
    Other,
}

/// Build a stop reason from the debugger's current focus + a reason
/// hint. Stateful commands (`run`, `continue`, `step*`) use this on
/// successful completion so the agent can correlate without waiting for
/// the next event.
pub fn current_stop(
    dbg: &crate::debugger::Debugger,
    kind: StopKind,
) -> Result<StopReason, crate::ui::structured::error::BsError> {
    let ecx = dbg.ecx();
    let pid = ecx.pid_on_focus();
    let pc = ecx.location().pc;
    // Try to resolve the function name + place at PC. Failing gracefully
    // — none of these absences are errors.
    let bt = dbg.backtrace(pid).ok();
    let top = bt.and_then(|mut frames| frames.drain(..).next());
    Ok(StopReason {
        kind,
        thread: pid.as_raw(),
        address: format!("0x{:x}", pc.as_u64()),
        function: top.as_ref().and_then(|f| f.func_name.clone()),
        file: top
            .as_ref()
            .and_then(|f| f.place.as_ref().map(|p| p.file.display().to_string())),
        line: top.as_ref().and_then(|f| f.place.as_ref().map(|p| p.line_number)),
    })
}

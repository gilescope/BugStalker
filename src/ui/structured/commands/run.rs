// SPDX-License-Identifier: MIT
//! `run` — start (or restart) the debuggee.
//!
//! Returns a `StopReason` describing where execution paused. If the
//! debuggee runs to completion the response carries `kind: "exit"`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::{Debugger, StopReason as DbgStop};
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

use super::{StopKind, StopReason, current_stop};

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// If true and the debuggee is already running, restart it from the
    /// beginning (matches console behaviour).
    #[serde(default)]
    pub restart: bool,
}

impl StructuredCommand for Run {
    const METHOD: &'static str = "run";
    const SUMMARY: &'static str = "Start (or restart) the debuggee. Blocks until next stop.";
    type Response = StopReason;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        if self.restart {
            dbg.restart_debugee()?;
            return current_stop(dbg, classify_after_restart(dbg));
        }
        let reason = dbg.start_debugee_with_reason()?;
        kind_from_stop(dbg, reason)
    }
}

fn kind_from_stop(dbg: &Debugger, reason: DbgStop) -> Result<StopReason, BsError> {
    let kind = stop_kind(&reason);
    current_stop(dbg, kind)
}

fn classify_after_restart(_dbg: &Debugger) -> StopKind {
    StopKind::Other
}

pub(super) fn stop_kind(reason: &DbgStop) -> StopKind {
    match reason {
        DbgStop::Breakpoint(_, _) => StopKind::Breakpoint,
        DbgStop::DebugeeExit(_) => StopKind::Exit,
        DbgStop::DebugeeStart => StopKind::Other,
        DbgStop::SignalStop(_, _) => StopKind::Signal,
        DbgStop::Watchpoint(_, _, _) => StopKind::Watchpoint,
        DbgStop::NoSuchProcess(_) => StopKind::Exit,
    }
}

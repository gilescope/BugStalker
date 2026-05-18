// SPDX-License-Identifier: MIT
//! `continue` — resume the debuggee until the next stop.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

use super::{StopReason, current_stop, run::stop_kind};

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Continue {}

impl StructuredCommand for Continue {
    const METHOD: &'static str = "continue";
    const SUMMARY: &'static str = "Resume the debuggee until the next stop event";
    type Response = StopReason;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let reason = dbg.continue_debugee_with_reason()?;
        current_stop(dbg, stop_kind(&reason))
    }
}

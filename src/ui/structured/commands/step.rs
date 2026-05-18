// SPDX-License-Identifier: MIT
//! `step.into`, `step.over`, `step.out`, `step.instruction`.
//!
//! Returns a `StopReason` describing the new focus location.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

use super::{StopKind, StopReason, current_stop};

macro_rules! step_command {
    ($name:ident, $method:literal, $summary:literal, $call:ident) => {
        #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {}

        impl StructuredCommand for $name {
            const METHOD: &'static str = $method;
            const SUMMARY: &'static str = $summary;
            type Response = StopReason;

            fn execute(
                self,
                dbg: &mut Debugger,
                _budget: &ResponseBudget,
            ) -> Result<Self::Response, BsError> {
                dbg.$call()?;
                current_stop(dbg, StopKind::Step)
            }
        }
    };
}

step_command!(
    StepInto,
    "step.into",
    "Step into the next source line",
    step_into
);
step_command!(
    StepOver,
    "step.over",
    "Step over the next source line",
    step_over
);
step_command!(
    StepOut,
    "step.out",
    "Step out of the current function",
    step_out
);
step_command!(
    StepInstruction,
    "step.instruction",
    "Step exactly one machine instruction",
    stepi
);

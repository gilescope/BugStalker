// SPDX-License-Identifier: MIT
//! `sharedlib.info` — list mapped shared libraries.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::ui::structured::envelope::ListResponse;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SharedlibInfo {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SharedlibEntry {
    pub path: String,
    pub has_debug_info: bool,
    pub from: Option<String>,
    pub to: Option<String>,
}

impl StructuredCommand for SharedlibInfo {
    const METHOD: &'static str = "sharedlib.info";
    const SUMMARY: &'static str = "List loaded shared libraries (regions in the inferior's VAS)";
    type Response = ListResponse<SharedlibEntry>;

    fn execute(
        self,
        dbg: &mut Debugger,
        budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let entries: Vec<SharedlibEntry> = dbg
            .shared_libs()
            .into_iter()
            .map(|r| SharedlibEntry {
                path: r.path.display().to_string(),
                has_debug_info: r.has_debug_info,
                from: r
                    .range
                    .as_ref()
                    .map(|rg| format!("0x{:x}", rg.from.as_u64())),
                to: r.range.as_ref().map(|rg| format!("0x{:x}", rg.to.as_u64())),
            })
            .collect();
        Ok(ListResponse::from_iter(entries, budget.item_cap()))
    }
}

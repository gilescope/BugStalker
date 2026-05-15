// SPDX-License-Identifier: MIT
//! `thread.info` — list all running debuggee threads.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::ui::structured::envelope::ListResponse;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

use super::ThreadDto;

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadInfo {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ThreadInfoEntry {
    #[serde(flatten)]
    pub thread: ThreadDto,
    pub in_focus: bool,
    pub address: Option<String>,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl StructuredCommand for ThreadInfo {
    const METHOD: &'static str = "thread.info";
    const SUMMARY: &'static str = "List all running debuggee threads";
    type Response = ListResponse<ThreadInfoEntry>;

    fn execute(
        self,
        dbg: &mut Debugger,
        budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let snapshots = dbg.thread_state()?;
        let entries: Vec<ThreadInfoEntry> = snapshots
            .into_iter()
            .map(|snap| {
                let top_frame = snap.bt.as_ref().and_then(|bt| bt.first());
                ThreadInfoEntry {
                    thread: ThreadDto::from(&snap.thread),
                    in_focus: snap.in_focus,
                    address: top_frame.map(|f| format!("0x{:x}", f.ip.as_u64())),
                    function: top_frame.and_then(|f| f.func_name.clone()),
                    file: snap.place.as_ref().map(|p| p.file.display().to_string()),
                    line: snap.place.as_ref().map(|p| p.line_number),
                }
            })
            .collect();
        Ok(ListResponse::from_iter(entries, budget.item_cap()))
    }
}

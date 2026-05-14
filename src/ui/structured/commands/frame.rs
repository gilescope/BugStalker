// SPDX-License-Identifier: MIT
//! `frame.info` — current focused frame metadata.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::ui::structured::error::BsError;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrameInfo {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FrameInfoResponse {
    pub num: u32,
    pub function: Option<String>,
    pub address: String,
    pub function_start: Option<String>,
    pub base_addr: String,
    pub cfa: String,
    pub return_addr: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl StructuredCommand for FrameInfo {
    const METHOD: &'static str = "frame.info";
    const SUMMARY: &'static str = "Information about the currently focused stack frame";
    type Response = FrameInfoResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let info = dbg.frame_info()?;
        Ok(FrameInfoResponse {
            num: info.num,
            function: info.frame.func_name.clone(),
            address: format!("0x{:x}", info.frame.ip.as_u64()),
            function_start: info.frame.fn_start_ip.map(|a| format!("0x{:x}", a.as_u64())),
            base_addr: format!("0x{:x}", info.base_addr.as_u64()),
            cfa: format!("0x{:x}", info.cfa.as_u64()),
            return_addr: info.return_addr.map(|a| format!("0x{:x}", a.as_u64())),
            file: info
                .frame
                .place
                .as_ref()
                .map(|p| p.file.display().to_string()),
            line: info.frame.place.as_ref().map(|p| p.line_number),
        })
    }
}

// SPDX-License-Identifier: MIT
//! Watchpoint commands: `watch.set`, `watch.remove`, `watch.info`.
//!
//! v1 surfaces only memory-address watchpoints from the script
//! transport. Expression watchpoints accept a DQE source string but go
//! through the existing parser; agents that want them can compose the
//! string the same way the console does.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::debugger::address::RelocatedAddress;
use crate::debugger::register::debug::{BreakCondition, BreakSize};
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchCondition {
    /// Trip on writes (default).
    Write,
    /// Trip on reads or writes.
    ReadWrite,
}

impl Default for WatchCondition {
    fn default() -> Self {
        Self::Write
    }
}

impl From<WatchCondition> for BreakCondition {
    fn from(c: WatchCondition) -> Self {
        match c {
            WatchCondition::Write => BreakCondition::DataWrites,
            WatchCondition::ReadWrite => BreakCondition::DataReadsWrites,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchSize {
    Bytes1,
    Bytes2,
    Bytes4,
    Bytes8,
}

impl Default for WatchSize {
    fn default() -> Self {
        Self::Bytes8
    }
}

impl From<WatchSize> for BreakSize {
    fn from(s: WatchSize) -> Self {
        match s {
            WatchSize::Bytes1 => BreakSize::Bytes1,
            WatchSize::Bytes2 => BreakSize::Bytes2,
            WatchSize::Bytes4 => BreakSize::Bytes4,
            WatchSize::Bytes8 => BreakSize::Bytes8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WatchpointEntry {
    pub number: u32,
    pub address: String,
    pub condition: String,
    pub size: u8,
    pub source_dqe: Option<String>,
}

impl<'a> From<&crate::debugger::WatchpointView<'a>> for WatchpointEntry {
    fn from(w: &crate::debugger::WatchpointView<'a>) -> Self {
        Self {
            number: w.number,
            address: format!("0x{:x}", w.address.as_u64()),
            condition: format!("{}", w.condition),
            size: match w.size {
                BreakSize::Bytes1 => 1,
                BreakSize::Bytes2 => 2,
                BreakSize::Bytes4 => 4,
                BreakSize::Bytes8 => 8,
            },
            source_dqe: w.source_dqe.as_ref().map(|s| s.to_string()),
        }
    }
}

// ---------- watch.info --------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchInfo {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchInfoResponse {
    pub watchpoints: Vec<WatchpointEntry>,
}

impl StructuredCommand for WatchInfo {
    const METHOD: &'static str = "watch.info";
    const SUMMARY: &'static str = "List all currently set watchpoints";
    type Response = WatchInfoResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        Ok(WatchInfoResponse {
            watchpoints: dbg
                .watchpoint_list()
                .iter()
                .map(WatchpointEntry::from)
                .collect(),
        })
    }
}

// ---------- watch.set ---------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchSet {
    pub address: String,
    #[serde(default)]
    pub size: WatchSize,
    #[serde(default)]
    pub condition: WatchCondition,
    #[serde(default)]
    pub temporary: bool,
}

impl StructuredCommand for WatchSet {
    const METHOD: &'static str = "watch.set";
    const SUMMARY: &'static str = "Set a watchpoint on a memory address";
    type Response = WatchpointEntry;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let trimmed = self
            .address
            .strip_prefix("0x")
            .or_else(|| self.address.strip_prefix("0X"))
            .unwrap_or(&self.address);
        let raw = u64::from_str_radix(trimmed, 16).map_err(|_| {
            BsError::new(
                ErrorCode::BadAddress,
                format!("invalid hex address: {}", self.address),
            )
        })?;
        let addr: RelocatedAddress = raw.into();
        let view = dbg.set_watchpoint_on_memory(
            addr,
            self.size.into(),
            self.condition.into(),
            self.temporary,
        )?;
        Ok(WatchpointEntry::from(&view))
    }
}

// ---------- watch.remove ------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchRemove {
    /// Either a `number` or an `address`. Exactly one must be set.
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchRemoveResponse {
    pub removed: Option<WatchpointEntry>,
}

impl StructuredCommand for WatchRemove {
    const METHOD: &'static str = "watch.remove";
    const SUMMARY: &'static str = "Remove a watchpoint by number or by address";
    type Response = WatchRemoveResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let removed = match (self.number, self.address) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(BsError::new(
                    ErrorCode::InvalidParams,
                    "pass exactly one of `number` or `address`",
                ));
            }
            (Some(n), None) => dbg.remove_watchpoint_by_number(n)?,
            (None, Some(addr)) => {
                let trimmed = addr
                    .strip_prefix("0x")
                    .or_else(|| addr.strip_prefix("0X"))
                    .unwrap_or(&addr);
                let raw = u64::from_str_radix(trimmed, 16).map_err(|_| {
                    BsError::new(
                        ErrorCode::BadAddress,
                        format!("invalid hex address: {addr}"),
                    )
                })?;
                dbg.remove_watchpoint_by_addr(raw.into())?
            }
        };
        Ok(WatchRemoveResponse {
            removed: removed.as_ref().map(WatchpointEntry::from),
        })
    }
}

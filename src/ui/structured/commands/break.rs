// SPDX-License-Identifier: MIT
//! Breakpoint commands: `break.set`, `break.remove`, `break.info`.
//!
//! Locations are tagged unions on the wire:
//!
//! ```jsonc
//! { method: "break.set", params: { at: { kind: "line", file: "main.rs", line: 42 } } }
//! { method: "break.set", params: { at: { kind: "function", name: "main" } } }
//! { method: "break.set", params: { at: { kind: "address", address: "0x55a..." } } }
//! ```
//!
//! Or, for ergonomic CLI-shaped input, a string variant:
//!
//! ```jsonc
//! { method: "break.set", params: { at: "main.rs:42" } }
//! { method: "break.set", params: { at: "my_app::handler" } }
//! { method: "break.set", params: { at: "0x55a01200" } }
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::debugger::address::Address;
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::{ResponseBudget, StructuredCommand};

/// Tagged-union location. Accepts either an object form or the
/// shorthand string form.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Location {
    Object(LocationObject),
    Shorthand(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocationObject {
    Line { file: String, line: u64 },
    Function { name: String },
    Address { address: String },
}

impl Location {
    fn parse(self) -> Result<LocationObject, BsError> {
        match self {
            Location::Object(o) => Ok(o),
            Location::Shorthand(s) => parse_shorthand(&s),
        }
    }
}

fn parse_shorthand(s: &str) -> Result<LocationObject, BsError> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        let _ = usize::from_str_radix(rest, 16).map_err(|_| {
            BsError::new(
                ErrorCode::BadAddress,
                format!("not a valid hex address: {s}"),
            )
        })?;
        return Ok(LocationObject::Address {
            address: s.to_string(),
        });
    }
    // file:line if there's exactly one ':' followed by all digits.
    if let Some((file, line)) = s.rsplit_once(':')
        && let Ok(n) = line.parse::<u64>()
        && !file.is_empty()
    {
        return Ok(LocationObject::Line {
            file: file.to_string(),
            line: n,
        });
    }
    // Otherwise treat as a function name.
    Ok(LocationObject::Function {
        name: s.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BreakpointEntry {
    pub number: u32,
    pub address: String,
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl<'a> From<&crate::debugger::BreakpointView<'a>> for BreakpointEntry {
    fn from(v: &crate::debugger::BreakpointView<'a>) -> Self {
        Self {
            number: v.number,
            address: format!("{}", v.addr),
            file: v.place.as_ref().map(|p| p.file.display().to_string()),
            line: v.place.as_ref().map(|p| p.line_number),
        }
    }
}

// ---------- break.set ---------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BreakSet {
    pub at: Location,
    /// If true and the location does not currently resolve, register a
    /// deferred breakpoint that activates when the relevant shared
    /// library loads. Default: false.
    #[serde(default)]
    pub deferred: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakSetResponse {
    pub breakpoints: Vec<BreakpointEntry>,
    /// True when `deferred: true` was passed and the location did not
    /// resolve at the time of the call. The breakpoint will activate
    /// later when the matching shared library is loaded.
    pub deferred: bool,
    /// When the source line mapped to multiple candidate addresses
    /// (typical with monomorphization and inlining), this records what
    /// the chooser saw and what it picked. Empty when there was only
    /// one candidate or when the request didn't target a source line
    /// (function-name and address forms skip the line-table search).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<LineCandidateEntry>,
    /// True when no canonical candidate was found and the chooser fell
    /// back to inline copies of the source line — the breakpoint will
    /// land inside a different function where the line got inlined.
    /// Set this expectation in the agent before any var-read fails.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inline_fallback_used: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LineCandidateEntry {
    pub address: String,
    pub function: Option<String>,
    pub decl_file: Option<String>,
    pub status: LineCandidateStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineCandidateStatus {
    /// Used as a breakpoint address.
    Selected,
    /// Another candidate in the same subprogram won the dedup.
    DuplicateSubprogram,
    /// Source line was only present here via inlining — the enclosing
    /// subprogram is declared in a different file.
    InlineCopy,
}

impl From<crate::debugger::LineCandidate> for LineCandidateEntry {
    fn from(c: crate::debugger::LineCandidate) -> Self {
        Self {
            address: format!("0x{:x}", u64::from(c.address)),
            function: c.function,
            decl_file: c.decl_file.map(|p| p.display().to_string()),
            status: match c.status {
                crate::debugger::LineCandidateStatus::Selected => LineCandidateStatus::Selected,
                crate::debugger::LineCandidateStatus::DuplicateSubprogram => {
                    LineCandidateStatus::DuplicateSubprogram
                }
                crate::debugger::LineCandidateStatus::InlineCopy => LineCandidateStatus::InlineCopy,
            },
        }
    }
}

impl StructuredCommand for BreakSet {
    const METHOD: &'static str = "break.set";
    const SUMMARY: &'static str = "Set a breakpoint at a source line, function name, or address";
    type Response = BreakSetResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let loc = self.at.parse()?;
        let mut candidates: Vec<LineCandidateEntry> = vec![];
        let mut inline_fallback_used = false;

        let result = match &loc {
            LocationObject::Line { file, line } => {
                match dbg.set_breakpoint_at_line_with_diagnostics(file, *line) {
                    Ok((views, diags)) => {
                        for d in diags {
                            inline_fallback_used |= d.inline_fallback_used;
                            for c in d.candidates {
                                candidates.push(LineCandidateEntry::from(c));
                            }
                        }
                        Ok(views)
                    }
                    Err(e) => Err(e),
                }
            }
            LocationObject::Function { name } => dbg.set_breakpoint_at_fn(name),
            LocationObject::Address { address } => {
                let addr = parse_addr(address)?;
                dbg.set_breakpoint_at_addr(addr.into()).map(|v| vec![v])
            }
        };

        match result {
            Ok(views) => Ok(BreakSetResponse {
                breakpoints: views.iter().map(BreakpointEntry::from).collect(),
                deferred: false,
                candidates,
                inline_fallback_used,
            }),
            Err(e) if self.deferred => {
                // Register as deferred and report success.
                match &loc {
                    LocationObject::Line { file, line } => dbg.add_deferred_at_line(file, *line),
                    LocationObject::Function { name } => dbg.add_deferred_at_function(name),
                    LocationObject::Address { address } => {
                        let addr = parse_addr(address)?;
                        dbg.add_deferred_at_addr(addr.into());
                    }
                }
                let _ = e;
                Ok(BreakSetResponse {
                    breakpoints: vec![],
                    deferred: true,
                    candidates,
                    inline_fallback_used,
                })
            }
            Err(e) => Err(BsError::from(e)),
        }
    }
}

// ---------- break.remove ------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BreakRemove {
    /// Either a `number` or an `at` location. Exactly one must be set.
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub at: Option<Location>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakRemoveResponse {
    pub removed: Vec<BreakpointEntry>,
}

impl StructuredCommand for BreakRemove {
    const METHOD: &'static str = "break.remove";
    const SUMMARY: &'static str = "Remove a breakpoint by number or by location";
    type Response = BreakRemoveResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let removed = match (self.number, self.at) {
            (Some(_), Some(_)) => {
                return Err(BsError::new(
                    ErrorCode::InvalidParams,
                    "pass exactly one of `number` or `at`",
                ));
            }
            (Some(n), None) => dbg
                .remove_breakpoint_by_number(n)?
                .map(|v| vec![BreakpointEntry::from(&v)])
                .unwrap_or_default(),
            (None, Some(loc)) => match loc.parse()? {
                LocationObject::Line { file, line } => dbg
                    .remove_breakpoint_at_line(&file, line)?
                    .iter()
                    .map(BreakpointEntry::from)
                    .collect(),
                LocationObject::Function { name } => dbg
                    .remove_breakpoint_at_fn(&name)?
                    .iter()
                    .map(BreakpointEntry::from)
                    .collect(),
                LocationObject::Address { address } => {
                    let addr = parse_addr(&address)?;
                    dbg.remove_breakpoint(Address::Relocated(addr.into()))?
                        .iter()
                        .map(BreakpointEntry::from)
                        .collect()
                }
            },
            (None, None) => {
                return Err(BsError::new(
                    ErrorCode::InvalidParams,
                    "pass exactly one of `number` or `at`",
                ));
            }
        };
        Ok(BreakRemoveResponse { removed })
    }
}

// ---------- break.info --------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BreakInfo {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BreakInfoResponse {
    pub breakpoints: Vec<BreakpointEntry>,
}

impl StructuredCommand for BreakInfo {
    const METHOD: &'static str = "break.info";
    const SUMMARY: &'static str = "List all currently set breakpoints";
    type Response = BreakInfoResponse;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        Ok(BreakInfoResponse {
            breakpoints: dbg
                .breakpoints_snapshot()
                .iter()
                .map(BreakpointEntry::from)
                .collect(),
        })
    }
}

fn parse_addr(s: &str) -> Result<usize, BsError> {
    let trimmed = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    usize::from_str_radix(trimmed, 16)
        .map_err(|_| BsError::new(ErrorCode::BadAddress, format!("invalid hex address: {s}")))
}

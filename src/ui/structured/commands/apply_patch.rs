// SPDX-License-Identifier: MIT
//! `patch.apply` — apply a wild-emitted patch file to the running
//! debuggee. JSON-RPC wrapper around the existing
//! [`crate::ui::command::apply_patch::Handler`] so the AOT
//! edit-and-continue pipeline can be driven by a `bs --script` /
//! `bs --test` session.
//!
//! Typical EnC flow:
//!
//! ```text
//!   bs --script ./your_program
//!     ↓
//!   { jsonrpc: "2.0", id: 1, method: "break.set",
//!     params: { at: "main.rs:42" } }
//!   { jsonrpc: "2.0", id: 2, method: "run" }       // stop at bp
//!   <user edits main.rs and rebuilds:
//!    cargo build → wild emits /tmp/foo.patch>
//!   { jsonrpc: "2.0", id: 3, method: "patch.apply",
//!     params: { path: "/tmp/foo.patch" } }
//!   { jsonrpc: "2.0", id: 4, method: "continue" }
//! ```
//!
//! The response carries everything the DAP layer surfaces to VSCode:
//! how many entries landed, how many were skipped for drift (process
//! bytes diverged from wild's `old_bytes` — usually a build
//! mismatch), how many were skipped for read-only protection, and
//! per-entry drift diagnostics.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::debugger::Debugger;
use crate::ui::command::apply_patch;
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchApply {
    /// Filesystem path to the wild-emitted patch file (the `--emit-
    /// patch=<path>` output of the linker). Both v2 (with pre-image
    /// drift verification) and v3 (with blake3 image-hash header)
    /// formats are accepted.
    pub path: String,
    /// Explicit base address for entry-offset translation. Hex
    /// (`"0x55…"`) or decimal as string; numeric also accepted.
    /// When omitted, BugStalker resolves the executable's runtime
    /// load address from its own DWARF mapping table.
    #[serde(default)]
    pub base: Option<serde_json::Value>,
    /// When `true` (the default), refuse to apply if the patch's
    /// `# old-blake3` header doesn't match the on-disk executable
    /// hash — a guard against applying a patch built for a
    /// different binary. Set `false` to skip the guard (you've
    /// renamed the binary between build and apply, etc.).
    #[serde(default = "default_verify")]
    pub verify_hash: bool,
}

fn default_verify() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct PatchApplyResult {
    /// Number of patch entries successfully written to the inferior.
    pub entries_applied: usize,
    /// Total bytes written across all applied entries.
    pub bytes_written: usize,
    /// Entries skipped because the process's current bytes didn't
    /// match wild's `old_bytes` — symptomatic of a build mismatch
    /// (different commit, different RUSTFLAGS, the running binary
    /// isn't what the linker thought it was diffing against).
    pub entries_skipped_drift: usize,
    /// Entries that landed in a page whose `max_protection` forbids
    /// write (sealed `__DATA_CONST`, code-signature blob, etc.).
    /// Mostly a macOS concern; Linux text pages are writable after
    /// `mprotect`.
    pub entries_skipped_readonly: usize,
    /// Hex-stringed runtime addresses of every successfully applied
    /// entry. The DAP layer uses these to decide whether the patch
    /// touched the currently-paused function and an auto-restart-
    /// frame is wanted; script callers can do the same.
    pub applied_runtime_addrs: Vec<String>,
    /// Per-entry drift detail capped at 16 entries. Each carries
    /// `(file_offset, runtime_addr, symbol, expected_hex,
    /// actual_hex)` so a rustc-style "expected X, found Y at
    /// offset Z" report is buildable without re-reading process
    /// memory.
    pub drift_details: Vec<DriftDetailJson>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DriftDetailJson {
    pub offset: u64,
    pub runtime_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub expected_hex: String,
    pub actual_hex: String,
}

impl StructuredCommand for PatchApply {
    const METHOD: &'static str = "patch.apply";
    const SUMMARY: &'static str = "Apply a wild-emitted patch file to the running debuggee \
         (AOT edit-and-continue). Returns counts + drift diagnostics.";
    type Response = PatchApplyResult;

    fn execute(
        self,
        dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<PatchApplyResult, BsError> {
        let base = match self.base {
            None => None,
            Some(serde_json::Value::Null) => None,
            Some(v) => Some(parse_base(&v)?),
        };
        let cmd = apply_patch::Command::ApplyPatch {
            path: PathBuf::from(&self.path),
            base,
            verify_executable_hash: self.verify_hash,
        };
        let report = apply_patch::Handler::new(dbg)
            .handle(cmd)
            .map_err(|e| BsError {
                code: ErrorCode::Internal,
                message: format!("patch.apply failed: {e}"),
                data: None,
            })?;
        Ok(PatchApplyResult {
            entries_applied: report.entries_applied,
            bytes_written: report.bytes_written,
            entries_skipped_drift: report.entries_skipped_drift,
            entries_skipped_readonly: report.entries_skipped_readonly,
            applied_runtime_addrs: report
                .applied_runtime_addrs
                .into_iter()
                .map(|a| format!("0x{a:x}"))
                .collect(),
            drift_details: report
                .drift_details
                .into_iter()
                .map(|d| DriftDetailJson {
                    offset: d.offset,
                    runtime_addr: format!("0x{:x}", d.runtime_addr),
                    symbol: d.symbol,
                    expected_hex: d.expected_hex,
                    actual_hex: d.actual_hex,
                })
                .collect(),
        })
    }
}

/// Accept either a number (treated as decimal) or a string in
/// hex (`"0x55…"`) or decimal form. Mirrors the lenient address
/// parsing every other bs CLI/script command uses.
fn parse_base(v: &serde_json::Value) -> Result<libc::uintptr_t, BsError> {
    if let Some(n) = v.as_u64() {
        return Ok(n as libc::uintptr_t);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        let parsed = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            libc::uintptr_t::from_str_radix(rest, 16)
        } else {
            s.parse::<libc::uintptr_t>()
        };
        return parsed.map_err(|e| BsError {
            code: ErrorCode::InvalidParams,
            message: format!("invalid base address {s:?}: {e}"),
            data: None,
        });
    }
    Err(BsError {
        code: ErrorCode::InvalidParams,
        message: "base must be a number or a hex/decimal string".to_string(),
        data: None,
    })
}

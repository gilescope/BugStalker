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
    /// When `true` (the default) AND the patch lands in the
    /// currently-paused function, the debugger automatically:
    ///
    /// 1. Restarts the top frame (rewinds PC to the function's
    ///    entry, restores the function-entry SP + callee-saved
    ///    register set the unwinder reconstructed from frame 1's
    ///    CFA).
    /// 2. Continues execution past any intermediate breakpoints
    ///    in the function body, capped at 64 skips.
    /// 3. Stops when execution reaches the PC the user was
    ///    paused at — i.e. the breakpoint that paused the
    ///    debugger before the patch.
    ///
    /// End result: the user sees their breakpoint hit again,
    /// inside the *patched* function body, without manual
    /// intervention. Mirrors the DAP `bs/applyPatch` flow.
    /// Set `false` to apply the patch but leave the debugger
    /// at the pre-restart PC (useful when a script wants to
    /// inspect intermediate state).
    #[serde(default = "default_restart")]
    pub restart: bool,
}

fn default_verify() -> bool {
    true
}

fn default_restart() -> bool {
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
    /// Hex-stringed function-entry address when the script auto-
    /// restarted the top frame. `null` when no restart happened —
    /// either the patch didn't touch the currently-paused
    /// function, or restart was disabled (via `restart: false` on
    /// the request).
    ///
    /// When set, the script ran the patched function from its
    /// entry back to the user's original breakpoint, so the next
    /// `var` / `bt` reflects the post-patch behaviour without the
    /// caller needing a manual `continue`. Mirrors the DAP path's
    /// `restartedFrameFnStart` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restarted_frame_fn_start: Option<String>,
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

        // Disable every active breakpoint before writing patch
        // bytes — any INT3 (0xCC) the debugger has injected on
        // top of the patched text region would cause wild's pre-
        // image check to drift (the patch's `old_bytes` were
        // computed against a clean binary, not the bp-laden one).
        // We snapshot the addresses so we can re-arm afterwards.
        // Per the user's flow: only the breakpoint at the user's
        // original PC stays armed during the auto-resume, so it
        // can catch the patched code re-emerging there.
        let suspended_bps = dbg.disable_all_breakpoints();

        let cmd = apply_patch::Command::ApplyPatch {
            path: PathBuf::from(&self.path),
            base,
            verify_executable_hash: self.verify_hash,
        };
        let report = match apply_patch::Handler::new(dbg).handle(cmd) {
            Ok(r) => r,
            Err(e) => {
                // Best-effort restore on apply failure so the
                // user isn't left with no breakpoints when the
                // patch couldn't even be parsed.
                dbg.enable_breakpoints_at(&suspended_bps);
                return Err(BsError {
                    code: ErrorCode::Internal,
                    message: format!("patch.apply failed: {e}"),
                    data: None,
                });
            }
        };

        // Auto-restart-frame: when the patch lands in the same
        // function the focused thread is paused inside, rewind PC
        // to the function entry and continue past intermediate
        // breakpoints back to the user's original PC. Mirrors the
        // DAP `bs/applyPatch` flow so a `bs --script` agent
        // gets the same "edit, save, your breakpoint hits in the
        // patched code" semantics as VSCode users.
        let restarted_frame_fn_start = if self.restart && report.entries_applied > 0 {
            auto_restart_after_patch(dbg, &report.applied_runtime_addrs, &suspended_bps)?
        } else {
            None
        };

        // Restore every breakpoint we suspended (the auto-
        // resume helper already restored the user's target bp if
        // it ran). Idempotent — `enable_breakpoints_at` is a no-
        // op for bps that are already enabled.
        dbg.enable_breakpoints_at(&suspended_bps);

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
            restarted_frame_fn_start: restarted_frame_fn_start.map(|a| format!("0x{a:x}")),
        })
    }
}

/// Cap on the number of intermediate breakpoint stops the auto-
/// resume loop will skip past. Mirrors the DAP path's constant
/// so the two flows have identical edge-case behaviour. 64 is
/// "way more than any single function should have" — past that,
/// surface the most recent stop and let the caller decide.
const AUTO_CONTINUE_MAX_SKIPS: usize = 64;

/// Detect whether the patch touched the currently-paused function;
/// if so, restart its top frame and drive execution back to the
/// user's pre-patch PC. Returns the function-entry address when a
/// restart happened, `None` otherwise (no restart was needed, or
/// no backtrace was available).
///
/// Skip-on-continue strategy: any intermediate `Breakpoint` stop
/// whose address doesn't match the captured pre-patch PC gets
/// stepped past (the next `continue_debugee_with_reason` re-runs
/// the BRK single-step then resumes). Capped at
/// `AUTO_CONTINUE_MAX_SKIPS` so a breakpoint storm in the same
/// function can't loop indefinitely.
fn auto_restart_after_patch(
    dbg: &mut Debugger,
    applied_addrs: &[usize],
    suspended_bps: &[crate::debugger::address::RelocatedAddress],
) -> Result<Option<u64>, BsError> {
    let pid = dbg.ecx().pid_on_focus();
    let bt = dbg.backtrace(pid).unwrap_or_default();
    let Some(current_fn_start) = bt.first().and_then(|f| f.fn_start_ip) else {
        return Ok(None);
    };
    let landed_in_current_fn = applied_addrs
        .iter()
        .any(|&addr| dbg.function_start_ip_at(addr) == Some(current_fn_start));
    if !landed_in_current_fn {
        return Ok(None);
    }

    // Snapshot the original PC before restart_top_frame moves it
    // to fn_start. The skip-on-continue loop below uses this as
    // the target for the auto-resume.
    let original_pc = dbg.ecx().location().pc.as_u64();
    dbg.restart_top_frame(pid, current_fn_start.as_u64())
        .map_err(|e| BsError {
            code: ErrorCode::Internal,
            message: format!("auto-restart-frame failed after patch: {e}"),
            data: None,
        })?;

    // Re-arm only the breakpoint at the user's original PC (if
    // present in the suspended list). All other bps stay
    // disabled during the auto-resume so the patched function
    // runs through cleanly to where the user was paused — no
    // intermediate stops, no skip-on-continue gymnastics.
    let original_addr = crate::debugger::address::RelocatedAddress::from(original_pc as usize);
    if suspended_bps.contains(&original_addr) {
        dbg.enable_breakpoints_at(&[original_addr]);
    }

    // Auto-continue until we land at original_pc, the inferior
    // exits, or we hit something unexpected (signal, watchpoint).
    // With only the target bp armed this almost always terminates
    // on the first continue; the cap is a safety net for
    // pathological cases (e.g. a panic stop or auto-trap firing
    // inside the patched function before the bp).
    let mut skipped = 0usize;
    loop {
        let stop = dbg.continue_debugee_with_reason().map_err(|e| BsError {
            code: ErrorCode::Internal,
            message: format!("auto-resume after restart failed: {e}"),
            data: None,
        })?;
        use crate::debugger::StopReason;
        let landed_at_target = match &stop {
            StopReason::Breakpoint(_, addr) => addr.as_u64() == original_pc,
            // Any non-Breakpoint stop (signal, exit, watchpoint)
            // is intentional — surface it rather than loop.
            _ => true,
        };
        if landed_at_target {
            break;
        }
        skipped += 1;
        if skipped >= AUTO_CONTINUE_MAX_SKIPS {
            log::warn!(
                target: "patch_apply",
                "auto-resume hit {skipped} non-target breakpoints without reaching original PC 0x{original_pc:x}; \
                 surfacing the current stop instead of looping further"
            );
            break;
        }
    }
    Ok(Some(current_fn_start.as_u64()))
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

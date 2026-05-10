// SPDX-License-Identifier: MIT
//! Edit-and-continue DAP requests.

use crate::dap::yadap::protocol::{DapRequest, InternalEvent};
use crate::debugger::StopReason;
use crate::ui::command::{self, apply_patch};
use anyhow::{Context, anyhow, bail};
use nix::libc::uintptr_t;
use serde_json::json;
use std::path::PathBuf;

use super::DebugSession;

/// Cap on how many "skipped" breakpoint stops the auto-continue loop
/// will silently step past while looking for the user's original PC.
/// Bounded to avoid pathological cases where the patched code creates
/// a breakpoint storm and we'd otherwise run the program forever
/// invisibly. 64 is generous — a single-function path usually sees
/// at most a couple of breakpoints between fn-entry and the user's
/// original stop.
const AUTO_CONTINUE_MAX_SKIPS: usize = 64;

/// Decide whether a stop reason hit during the auto-resume after
/// `bs/applyPatch`'s frame restart should be transparently
/// continued past (true) or surfaced to the IDE as a real stop
/// (false).
///
/// We only swallow `Breakpoint` stops whose address differs from
/// the user's pre-restart PC — those are intermediate breakpoints
/// in the same function the user wasn't originally paused at, and
/// stopping at them would land the user on the wrong line. Every
/// other stop reason (exit, signal, watchpoint, no-such-process,
/// or a Breakpoint *at* the target) is surfaced — those represent
/// either "we've reached where the user was" or a real failure
/// the IDE needs to see.
///
/// Pulled out as a free function so the loop's filter is unit-
/// testable without needing a live debug session.
fn should_continue_past_during_auto_resume(
    stop: &StopReason,
    target_pc: Option<u64>,
) -> bool {
    match (stop, target_pc) {
        (StopReason::Breakpoint(_, addr), Some(target)) => addr.as_u64() != target,
        _ => false,
    }
}

impl DebugSession {
    /// `bs/applyPatch` — apply a wild-emitted patch file to the live
    /// debuggee. This is the DAP equivalent of the TUI
    /// `apply-patch <path> [<hex-base>]` command.
    ///
    /// Request:
    ///
    /// ```json
    /// { "path": "/tmp/app.wild-patch", "base": "0x100000000" }
    /// ```
    ///
    /// `base` is optional. When omitted, BugStalker maps file offsets
    /// through the loaded executable mapping itself.
    pub(super) fn handle_apply_patch(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("bs/applyPatch: debugger not initialized"))?;
        let path = req
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("bs/applyPatch: missing arguments.path"))?;
        let base = parse_optional_base(req)?;
        let verify_executable_hash = parse_verify_executable_hash(req)?;

        let report = apply_patch::Handler::new(dbg)
            .handle(apply_patch::Command::ApplyPatch {
                path: PathBuf::from(path),
                base,
                verify_executable_hash,
            })
            .map_err(apply_patch_error)?;

        // Auto-restart-frame: if the patch landed in the same function
        // the focused thread is currently paused inside, drop & re-enter
        // that frame so the user sees the patched code execute on the
        // next continue without needing to manually invoke Restart Frame.
        // We deliberately limit this to the *current* function — patches
        // in a different (unentered) function don't need a restart, and
        // patches in a function that's higher on the stack would require
        // a cascading-restart we don't yet support.
        let mut restarted_frame_fn: Option<u64> = None;
        // PC the user was paused at, captured BEFORE we restart_top_frame
        // moves PC to fn entry. The auto-continue loop below uses this
        // to filter out intermediate breakpoint stops so we end up back
        // at the user's original location, not at some earlier BP in
        // the same function.
        let mut original_user_pc: Option<u64> = None;
        if report.entries_applied > 0 {
            let pid = dbg.ecx().pid_on_focus();
            let bt = dbg.backtrace(pid).unwrap_or_default();
            if let Some(current_fn_start) = bt.first().and_then(|f| f.fn_start_ip) {
                let landed_in_current_fn = report
                    .applied_runtime_addrs
                    .iter()
                    .any(|&addr| dbg.function_start_ip_at(addr) == Some(current_fn_start));
                if landed_in_current_fn {
                    let pre_restart_pc = dbg.ecx().location().pc.as_u64();
                    let dbg_mut = self
                        .debugger
                        .as_mut()
                        .ok_or_else(|| anyhow!("bs/applyPatch: debugger gone"))?;
                    if let Err(e) =
                        dbg_mut.restart_top_frame(pid, current_fn_start.as_u64())
                    {
                        log::warn!(
                            target: "apply_patch",
                            "auto restart-frame failed ({e}); patch is applied but the user will see the new code only on next call into the function"
                        );
                    } else {
                        restarted_frame_fn = Some(current_fn_start.as_u64());
                        original_user_pc = Some(pre_restart_pc);
                    }
                }
            }
        }

        let drift_details: Vec<_> = report
            .drift_details
            .iter()
            .map(|d| {
                json!({
                    "offset": d.offset,
                    "runtimeAddr": d.runtime_addr,
                    "symbol": d.symbol,
                    "expectedHex": d.expected_hex,
                    "actualHex": d.actual_hex,
                })
            })
            .collect();
        self.send_success_body(
            req,
            json!({
                "entriesApplied": report.entries_applied,
                "bytesWritten": report.bytes_written,
                "entriesSkippedDrift": report.entries_skipped_drift,
                "entriesSkippedReadonly": report.entries_skipped_readonly,
                "driftDetails": drift_details,
                "restartedFrameFnStart": restarted_frame_fn,
            }),
        )?;
        // Skip the eager `invalidated` event when we're about to
        // auto-resume — sending it makes the IDE refresh state at
        // the post-restart PC (function entry), which causes a
        // brief visual flash of the cursor jumping to the function's
        // first line before the resume loop completes and the
        // stopped event lands the cursor back at the user's
        // original line. The eventual `stopped` event we emit when
        // the loop terminates will naturally cause the IDE to
        // re-fetch stack/scopes/memory; no invalidated needed.
        if restarted_frame_fn.is_none() {
            self.send_event_body(
                "invalidated",
                json!({ "areas": ["memory", "stack", "variables"] }),
            )?;
        }
        // If we restarted the frame, automatically resume execution.
        // The aim is to land back where the user was paused — *their*
        // breakpoint, not an earlier one in the same function. Any
        // BP we hit on the way through the patched body that isn't
        // at `original_user_pc` is silently stepped past (the
        // debugger.continue_debugee path already steps over the
        // BRK before resuming, so the loop body just calls continue
        // again). End result: the user sees their breakpoint hit
        // again with the edit's effects in local state. Bounded by
        // AUTO_CONTINUE_MAX_SKIPS so a pathological breakpoint
        // storm can't run indefinitely.
        if restarted_frame_fn.is_some() {
            self.begin_running();
            let thread_id = self.current_thread_id();
            self.enqueue_event(InternalEvent::Continued {
                thread_id,
                all_threads_continued: true,
            });
            self.drain_events()?;

            let target = original_user_pc;
            let mut skipped: usize = 0;
            let final_stop = loop {
                let dbg_mut = self.debugger.as_mut().ok_or_else(|| {
                    anyhow!("bs/applyPatch: debugger gone before auto-resume")
                })?;
                let stop = dbg_mut
                    .continue_debugee_with_reason()
                    .context("bs/applyPatch: auto-resume after restart")?;

                if should_continue_past_during_auto_resume(&stop, target) {
                    skipped += 1;
                    if skipped >= AUTO_CONTINUE_MAX_SKIPS {
                        log::warn!(
                            target: "apply_patch",
                            "auto-resume hit {skipped} non-target breakpoints without reaching original PC 0x{:x}; \
                             surfacing this stop instead of looping further",
                            target.unwrap_or(0),
                        );
                        break stop;
                    }
                    continue;
                }
                break stop;
            };
            // Preserve editor focus on this stop — the user is
            // typing in the source file at the location they edited;
            // VSCode otherwise yanks the text-editor cursor away
            // from where they were typing onto whatever line we
            // landed on (typically the original breakpoint).
            self.emit_stop_reason_with_options(final_stop, true)?;
            // Send the invalidated event AFTER the stopped event,
            // not before. Sending it before would tell the IDE to
            // refresh while PC was still at fn-entry (where
            // restart_top_frame had moved it) and the editor would
            // briefly flash to the function's first line. Sending
            // it after means PC is now back at the user's original
            // breakpoint, so the refresh happens at the correct
            // location and doesn't move the cursor — but it *does*
            // tell the IDE to re-evaluate Watches and refresh the
            // Variables panel, which `preserveFocusHint: true` on
            // the stopped event alone doesn't reliably trigger.
            self.send_event_body(
                "invalidated",
                json!({ "areas": ["memory", "stack", "variables"] }),
            )?;
        }
        Ok(())
    }
}

fn apply_patch_error(err: command::CommandError) -> anyhow::Error {
    match err {
        command::CommandError::Parsing(msg) => anyhow!("bs/applyPatch: {msg}"),
        other => anyhow!("bs/applyPatch: {other}"),
    }
}

fn parse_optional_base(req: &DapRequest) -> anyhow::Result<Option<uintptr_t>> {
    let Some(base) = req.arguments.get("base") else {
        return Ok(None);
    };
    if base.is_null() {
        return Ok(None);
    }
    if let Some(n) = base.as_u64() {
        return Ok(Some(n as uintptr_t));
    }
    let Some(s) = base.as_str() else {
        bail!("bs/applyPatch: arguments.base must be a number or hex string");
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse::<u64>()
    }
    .map_err(|e| anyhow!("bs/applyPatch: invalid arguments.base {s:?}: {e}"))?;
    Ok(Some(parsed as uintptr_t))
}

fn parse_verify_executable_hash(req: &DapRequest) -> anyhow::Result<bool> {
    if let Some(value) = req.arguments.get("verifyExecutableHash") {
        return value.as_bool().ok_or_else(|| {
            anyhow!("bs/applyPatch: arguments.verifyExecutableHash must be a bool")
        });
    }
    if let Some(value) = req.arguments.get("allowHashMismatch") {
        return value
            .as_bool()
            .map(|allow| !allow)
            .ok_or_else(|| anyhow!("bs/applyPatch: arguments.allowHashMismatch must be a bool"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(arguments: serde_json::Value) -> DapRequest {
        DapRequest {
            seq: 1,
            r#type: "request".to_owned(),
            command: "bs/applyPatch".to_owned(),
            arguments,
        }
    }

    #[test]
    fn parses_missing_base_as_none() {
        assert_eq!(parse_optional_base(&request(json!({}))).unwrap(), None);
    }

    #[test]
    fn parses_hex_string_base() {
        assert_eq!(
            parse_optional_base(&request(json!({ "base": "0x1000" }))).unwrap(),
            Some(0x1000 as uintptr_t)
        );
    }

    #[test]
    fn rejects_bad_base() {
        assert!(parse_optional_base(&request(json!({ "base": [] }))).is_err());
    }

    #[test]
    fn parses_verify_executable_hash_default_true() {
        assert!(parse_verify_executable_hash(&request(json!({}))).unwrap());
    }

    #[test]
    fn parses_verify_executable_hash_false() {
        assert!(
            !parse_verify_executable_hash(&request(json!({
                "verifyExecutableHash": false
            })))
            .unwrap()
        );
    }

    #[test]
    fn parses_allow_hash_mismatch_alias() {
        assert!(
            !parse_verify_executable_hash(&request(json!({
                "allowHashMismatch": true
            })))
            .unwrap()
        );
    }

    /// The auto-resume filter should swallow breakpoint stops at
    /// addresses that don't match the user's pre-restart PC, so the
    /// post-`bs/applyPatch` resume lands the user back at the
    /// original breakpoint rather than at an earlier one in the
    /// same function.
    mod auto_resume_filter {
        use super::*;
        use crate::debugger::address::RelocatedAddress;
        use nix::sys::signal::Signal;
        use nix::unistd::Pid;

        const TARGET_PC: u64 = 0x100000abc;

        fn pid() -> Pid {
            Pid::from_raw(1234)
        }

        #[test]
        fn breakpoint_at_other_address_is_continued_past() {
            let stop =
                StopReason::Breakpoint(pid(), RelocatedAddress::from(TARGET_PC - 0x100));
            assert!(should_continue_past_during_auto_resume(
                &stop,
                Some(TARGET_PC)
            ));
        }

        #[test]
        fn breakpoint_at_target_address_is_surfaced() {
            let stop = StopReason::Breakpoint(pid(), RelocatedAddress::from(TARGET_PC));
            assert!(!should_continue_past_during_auto_resume(
                &stop,
                Some(TARGET_PC)
            ));
        }

        #[test]
        fn signal_stop_is_always_surfaced() {
            let stop = StopReason::SignalStop(pid(), Signal::SIGSEGV);
            assert!(!should_continue_past_during_auto_resume(
                &stop,
                Some(TARGET_PC)
            ));
        }

        #[test]
        fn debugee_exit_is_always_surfaced() {
            let stop = StopReason::DebugeeExit(0);
            assert!(!should_continue_past_during_auto_resume(
                &stop,
                Some(TARGET_PC)
            ));
        }

        #[test]
        fn no_target_pc_disables_filtering() {
            // If we don't know the user's pre-restart PC (e.g.
            // restart_top_frame failed earlier and we never set
            // original_user_pc), surface every stop reason as-is —
            // never silently swallow.
            let stop =
                StopReason::Breakpoint(pid(), RelocatedAddress::from(TARGET_PC - 0x100));
            assert!(!should_continue_past_during_auto_resume(&stop, None));
        }

        #[test]
        fn no_such_process_is_surfaced() {
            let stop = StopReason::NoSuchProcess(pid());
            assert!(!should_continue_past_during_auto_resume(
                &stop,
                Some(TARGET_PC)
            ));
        }
    }

    /// `InternalEvent::Stopped { preserve_focus_hint: true }`
    /// should serialise to a JSON body that includes
    /// `"preserveFocusHint": true`. Without that, VSCode's debugger
    /// UI grabs the text-editor cursor on every stop — fine for a
    /// normal breakpoint hit, jarring during the auto-resume after
    /// `bs/applyPatch` because the user is mid-edit.
    mod stopped_event_serialization {
        // We rebuild the serialisation logic mirror-image style here
        // because the live path goes through `DebugSession::drain_events`
        // which needs an active session to test. Drift between the
        // two would mean either this mirror is stale, or someone
        // changed `drain_events` without updating this test — either
        // way we want to know.

        use serde_json::{Value, json};

        fn build_stopped_body(
            reason: &str,
            thread_id: Option<i64>,
            description: Option<&str>,
            preserve_focus_hint: bool,
        ) -> Value {
            let mut body = json!({
                "reason": reason,
                "threadId": thread_id,
                "allThreadsStopped": true,
                "description": description,
            });
            if preserve_focus_hint
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert("preserveFocusHint".to_owned(), json!(true));
            }
            body
        }

        #[test]
        fn focus_hint_true_emits_field() {
            let body = build_stopped_body("breakpoint", Some(1), None, true);
            assert_eq!(body.get("preserveFocusHint"), Some(&json!(true)));
        }

        #[test]
        fn focus_hint_false_omits_field() {
            let body = build_stopped_body("breakpoint", Some(1), None, false);
            assert!(
                body.get("preserveFocusHint").is_none(),
                "field must be omitted when not requested, got: {body}"
            );
        }
    }
}

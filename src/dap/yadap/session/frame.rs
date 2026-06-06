// SPDX-License-Identifier: MIT
use anyhow::{Context, anyhow};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::ThreadFocusByPid;
use crate::dap::yadap::protocol::DapRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    Locals,
    Arguments,
    /// File-scope `static`s — variables-view §5.4. Shown for *all*
    /// crates, organised into a lazy `::` namespace tree; the tree (not
    /// a crate filter) is what keeps std / dep statics from flooding the
    /// pane (design-principles.md §2, §4).
    Statics,
    /// `thread_local!`s — variables-view §5.4. Current crate only (few,
    /// and per-thread mutable, so read eagerly).
    ThreadLocals,
}

/// Whether a backtrace frame belongs to the Rust panic / unwind runtime
/// (`core::panicking`, `std::panicking`, the `rust_panic` / `_Unwind_*`
/// shims, the various `panic_*` lang-item checks). Used to detect a
/// *break-on-panic stop* by its top frame: when the stack tops out in
/// the panic runtime, the caller deemphasizes the machinery down to the
/// first user frame so VS Code focuses the culprit (microsoft/vscode
/// #64193, #211855). Detecting only the panic *top frame* — rather than
/// deemphasizing all library frames everywhere — keeps a deliberate
/// any-frame step into a library (`shift+alt+right`) focusing that frame.
fn is_panic_runtime_frame(func_name: Option<&str>) -> bool {
    let Some(n) = func_name else { return false };
    n.contains("panicking::")
        || n.contains("_Unwind_")
        || n.starts_with("rust_panic")
        || n == "rust_begin_unwind"
        || n.contains("begin_panic")
        || n.contains("panic_fmt")
        || n.contains("panic_bounds_check")
        || n.contains("panic_misaligned_pointer_dereference")
        || n.contains("__rust_start_panic")
}

impl super::DebugSession {
    pub(super) fn handle_stack_trace(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        let thread_id = req
            .arguments
            .get("threadId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("stackTrace: missing arguments.threadId"))?;

        if self.has_replay_session() {
            if thread_id != super::replay::REPLAY_THREAD_ID {
                return self.send_err(req, "stackTrace: unknown replay thread");
            }
            let event_index = self.replay_position().unwrap_or_default();
            let pc = self.replay_current_pc()?;
            let name = match pc {
                Some(pc) => format!("replay event {event_index} @ 0x{pc:x}"),
                None => format!("replay event {event_index}"),
            };
            let frame = json!({
                "id": super::replay::REPLAY_FRAME_ID,
                "name": name,
                "line": 0,
                "column": 0,
            });
            return self.send_success_body(req, json!({"stackFrames": [frame], "totalFrames": 1}));
        }

        let pid = self
            .thread_cache
            .get(&thread_id)
            .copied()
            .unwrap_or_else(|| Pid::from_raw(thread_id as i32));

        let start = Instant::now();
        let bt = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("stackTrace: debugger not initialized"))?
            .backtrace(pid)
            .unwrap_or_default();
        let elapsed = start.elapsed();
        if elapsed > super::DEBUGGER_RESPONSE_TIMEOUT {
            return self.send_err(
                req,
                format!(
                    "stackTrace: debugger response timed out after {}ms",
                    super::DEBUGGER_RESPONSE_TIMEOUT.as_millis()
                ),
            );
        }
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        // A break-on-panic trap leaves the panic/unwind runtime *and* the
        // trait glue that called it (`Option::unwrap`, `[]` index, …) at
        // the top of the stack, above the user frame that actually
        // panicked. When the top frame is panic runtime, deemphasize every
        // frame down to — but not including — the first user frame, so VS
        // Code skips the machinery and focuses the culprit (microsoft/
        // vscode #64193). Gated on the panic top-frame so ordinary stops
        // (incl. a deliberate any-frame step into a library) are untouched.
        use crate::debugger::{FrameKind, classify_source_path};
        // `focusPanicCulprit` (launch arg / user setting) gates the whole
        // panic-focus behaviour: deemphasis, source omission, and the
        // `&Location` line correction below. Off → vanilla stack.
        let is_panic_stop = self.focus_panic_culprit
            && bt
                .first()
                .map(|f| is_panic_runtime_frame(f.func_name.as_deref()))
                .unwrap_or(false);
        let first_user_idx = is_panic_stop.then(|| {
            bt.iter().position(|f| {
                matches!(
                    classify_source_path(f.place.as_ref().map(|p| p.file.as_path())),
                    FrameKind::UserCode,
                )
            })
        });
        // At a break-on-panic stop the culprit (first user) frame's DWARF
        // line is the statement *enclosing* the panic call — for a macro or
        // multi-line site that lands a line or two early (the user saw
        // `mod.rs:9` for a panic that's really on `:10`). The
        // `#[track_caller]` `&Location` threaded into the panic machinery is
        // exact by construction; use it to correct that one frame's
        // line/column. Guarded by a file-suffix match so a stale/foreign
        // Location can never relabel the wrong frame.
        let culprit_idx = match first_user_idx {
            Some(Some(u)) => Some(u),
            _ => None,
        };
        let panic_site = culprit_idx
            .and(self.debugger.as_ref())
            .and_then(|d| d.panic_location());

        let mut frames = Vec::new();
        for (i, f) in bt.iter().enumerate() {
            if self.consume_cancellation(req, None)? {
                return Ok(());
            }
            let (path, mut line, mut col, source_reference) = match f.place.as_ref() {
                Some(p) => (
                    Some(p.file.to_string_lossy().to_string()),
                    Some(p.line_number as i64),
                    Some(p.column_number as i64),
                    None,
                ),
                None => {
                    let addr = f.ip.as_usize();
                    let Some(disasm) = self.disasm_source_for_address(req, addr)? else {
                        return Ok(());
                    };
                    (None, Some(1), Some(1), Some(disasm.reference))
                }
            };
            // Correct the culprit frame with the exact panic `Location`
            // (see `panic_site` above). The suffix match tolerates the
            // Location's relative path (`crate/src/foo.rs`) vs the frame's
            // absolute DWARF path.
            if Some(i) == culprit_idx
                && let (Some(ps), Some(p)) = (panic_site.as_ref(), path.as_ref())
                && p.ends_with(&ps.file)
            {
                line = Some(ps.line as i64);
                col = Some(ps.column as i64);
            }
            let name = f.func_name.as_deref().unwrap_or("<unknown>").to_string();
            let frame_id = (thread_id << 16) | (i as i64);
            // Frame is part of the panic machinery above the culprit.
            let deemphasize = matches!(first_user_idx, Some(Some(u)) if i < u);
            // A deemphasized panic-runtime frame omits `source` *entirely*.
            // The DAP `deemphasize` hint alone is advisory and VS Code still
            // auto-reveals the top frame's source on a stop — popping a
            // toolchain tab (`panic_info.rs`, `panicking.rs`) the user never
            // asked for. A *name-only* source is worse: VS Code treats it as an
            // unavailable-but-present source and errors with "Could not load
            // source: missing source.path". With no `source` at all the frame
            // is unavailable, so VS Code's on-stop focus predicate
            // (`source && source.available && presentationHint != 'deemphasize'`)
            // skips it and reveals the first frame that *does* carry a source —
            // the panicking user frame. The frame stays visible, greyed via the
            // `subtle` frame presentationHint below; it's just not navigable.
            let source = if deemphasize {
                None
            } else if let Some(path) = path {
                let p = self.source_map.map_target_to_client(&path);
                Some(json!({ "path": p }))
            } else if let Some(source_reference) = source_reference {
                let addr = f.ip.as_usize();
                let name = self
                    .disasm_cache_by_addr
                    .get(&addr)
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| format!("disasm @ 0x{addr:x}"));
                Some(json!({ "name": name, "sourceReference": source_reference }))
            } else {
                None
            };
            // Variables-view §5.5: tag this frame with its
            // recursion count when its function name appears
            // ≥ 2 times in the backtrace. The vscode-extension
            // uses this to render `[rec N]` next to the frame
            // name and to flag red when N exceeds the threshold.
            let ip_hex = format!("0x{:x}", f.ip.as_usize());
            let mut frame_obj = json!({
                "id": frame_id,
                "name": name,
                "source": source,
                "line": line.unwrap_or(0),
                "column": col.unwrap_or(0),
                "instructionPointerReference": ip_hex,
            });
            if deemphasize {
                frame_obj["presentationHint"] = json!("subtle");
            }
            if let Some(rec_count) = f.func_name.as_ref().and_then(|n| {
                bt.iter()
                    .filter(|s| s.func_name.as_ref() == Some(n))
                    .count()
                    .checked_sub(0)
                    .filter(|c| *c >= 2)
            }) {
                frame_obj["bugstalker.recursionCount"] = json!(rec_count);
            }
            frames.push(frame_obj);
        }
        // Variables-view §5.5: attach a stack-health snapshot to
        // the response so the variables-pane header pill + the
        // threads-pane budget bar can render without a separate
        // round-trip. Computed once per stack-trace request from
        // the just-fetched backtrace + proc_maps lookup.
        let stack_health = self
            .debugger
            .as_ref()
            .map(|dbg| crate::debugger::stack_health::compute(dbg, pid, &bt));
        let mut body = json!({
            "stackFrames": frames,
            "totalFrames": frames.len(),
        });
        if let Some(h) = stack_health {
            let mut sh = json!({
                "frameCount": h.frame_count,
                "maxRecursion": h.max_recursion,
            });
            if let Some(total) = h.thread_stack_size {
                sh["threadStackSize"] = json!(total);
            }
            if let Some(used) = h.thread_stack_used {
                sh["threadStackUsed"] = json!(used);
            }
            if let Some(pct) = h.used_pct() {
                sh["threadStackUsedPct"] = json!(pct);
            }
            body["bugstalker.stackHealth"] = sh;
        }
        self.send_success_body(req, body)
    }

    pub(super) fn handle_scopes(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.has_replay_session() {
            return self.send_success_body(req, json!({ "scopes": [] }));
        }

        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("scopes: debugger not initialized"))?;

        // Variables-view §5.3 refresh-on-stop: re-read proc_maps so
        // post-startup heap allocations (Box::new etc.) appear in
        // the segment index by the time the per-variable storage
        // classifier and heap-overlay lookup run. The variables-
        // pane query that follows this `scopes` request will hit
        // the freshly-rebuilt index. Best-effort — failure is
        // logged but doesn't block the scopes response (a stale
        // index just means heap-overlay misses on a few rows).
        let _ = dbg.refresh_segment_index();

        let frame_id = req
            .arguments
            .get("frameId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("scopes: missing arguments.frameId"))?;
        let (thread_id, frame_num) = Self::decode_frame_id(frame_id);
        let pid = self
            .thread_cache
            .get(&thread_id)
            .copied()
            .unwrap_or_else(|| Pid::from_raw(thread_id as i32));

        // Focus selected thread/frame to make variable evaluation consistent.
        let _ = dbg.set_thread_into_focus_by_pid(pid);
        let _ = dbg.set_frame_into_focus(frame_num);

        // NOTE: avoid borrowing `self` mutably while `dbg` is borrowed.
        let locals_ref = if let Some(r) = self
            .scope_cache
            .get(&(thread_id, frame_num, ScopeKind::Locals))
            .copied()
        {
            r
        } else {
            let locals = super::data::read_locals(dbg).unwrap_or_default();
            let r = self.vars.alloc(locals);
            self.scope_cache
                .insert((thread_id, frame_num, ScopeKind::Locals), r);
            r
        };

        let args_ref = if let Some(r) = self
            .scope_cache
            .get(&(thread_id, frame_num, ScopeKind::Arguments))
            .copied()
        {
            r
        } else {
            let args = super::data::read_args(dbg).unwrap_or_default();
            let r = self.vars.alloc(args);
            self.scope_cache
                .insert((thread_id, frame_num, ScopeKind::Arguments), r);
            r
        };

        // Variables-view §5.4: file-scope statics and TLS as
        // first-class DAP scopes. Both default to "current crate"
        // filtering to keep the pane signal-to-noise high — the
        // user's crate is usually what they want, not std/dep
        // internals.
        //
        // Deferred (design-principles.md §2): reading every static's
        // value here would cost ~tens of ms on a dependency-rich binary
        // *on every stop*, for a pane usually never opened. Instead hand
        // back a placeholder ref marked `expensive: true` and record it
        // in `pending_scopes`; `handle_variables` enumerates only when
        // the user expands the node. `dbg` is not borrowed below, so the
        // mutable `self` access is clean.
        let statics_ref = self.alloc_lazy_scope(thread_id, frame_num, ScopeKind::Statics);
        let tls_ref = self.alloc_lazy_scope(thread_id, frame_num, ScopeKind::ThreadLocals);

        let scopes = vec![
            json!({"name": "Locals", "variablesReference": locals_ref, "expensive": false}),
            json!({"name": "Arguments", "variablesReference": args_ref, "expensive": false}),
            json!({"name": "Statics", "variablesReference": statics_ref, "expensive": true}),
            json!({"name": "Thread-locals", "variablesReference": tls_ref, "expensive": true}),
        ];

        self.send_success_body(req, json!({"scopes": scopes}))
    }

    /// Allocate (or reuse) a deferred file-scope scope reference. The
    /// slot starts empty; `pending_scopes` records the
    /// `(thread, frame, kind)` so `handle_variables` can re-focus the
    /// right frame and enumerate on first expand. Reusing the cached ref
    /// within a stop means the statics are read at most once even if the
    /// client requests `scopes` repeatedly.
    fn alloc_lazy_scope(&mut self, thread_id: i64, frame_num: u32, kind: ScopeKind) -> i64 {
        if let Some(r) = self.scope_cache.get(&(thread_id, frame_num, kind)).copied() {
            return r;
        }
        let r = self.vars.alloc(Vec::new());
        self.scope_cache.insert((thread_id, frame_num, kind), r);
        self.pending_scopes.insert(r, (thread_id, frame_num, kind));
        r
    }

    pub fn handle_restart_frame(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("restartFrame: debugger not initialized"))?;

        let frame_id = req
            .arguments
            .get("frameId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("restartFrame: missing arguments.frameId"))?;
        if frame_id < 0 {
            return self.send_err(req, "restartFrame: frameId must be non-negative");
        }
        let (thread_id, frame_num) = Self::decode_frame_id(frame_id);
        if frame_num != 0 {
            return self.send_err(req, "restartFrame: only the top frame (0) can be restarted");
        }

        let pid = self
            .thread_cache
            .get(&thread_id)
            .copied()
            .unwrap_or_else(|| Pid::from_raw(thread_id as i32));
        let _ = dbg.set_thread_into_focus_by_pid(pid);

        let bt = dbg.backtrace(pid).unwrap_or_default();
        let frame = bt
            .get(frame_num as usize)
            .ok_or_else(|| anyhow!("restartFrame: frame {frame_num} not found"))?;
        let Some(start_ip) = frame.fn_start_ip else {
            return self.send_err(req, "restartFrame: function start address is unavailable");
        };

        // Full state restoration on aarch64: SP, LR, callee-saved
        // regs all reset to function-entry values. On x86_64 this
        // currently falls back to set_pc only (writing the return
        // address to the new stack slot is a Phase-2 follow-up).
        if let Err(e) = dbg.restart_top_frame(pid, start_ip.as_u64()) {
            log::warn!(
                target: "restart_frame",
                "full state restore failed ({e}); falling back to PC-only"
            );
            dbg.set_pc(start_ip.as_u64())
                .context("restartFrame: set pc fallback")?;
        }
        let _ = dbg.set_frame_into_focus(0);

        self.send_success(req)?;
        self.emit_manual_stop("restart", None)
    }

    pub fn refresh_threads_with_events(&mut self) -> anyhow::Result<Vec<Value>> {
        if self.has_replay_session() && self.debugger.is_none() {
            let id = super::replay::REPLAY_THREAD_ID;
            let existing_ids: HashSet<i64> = self.thread_cache.keys().copied().collect();
            if !existing_ids.contains(&id) {
                self.enqueue_thread_event("started", id);
            }
            for old in existing_ids.into_iter().filter(|old| *old != id) {
                self.enqueue_thread_event("exited", old);
            }
            self.thread_cache = HashMap::from([(id, Pid::from_raw(id as i32))]);
            return Ok(vec![json!({
                "id": id,
                "name": "replay trace",
            })]);
        }

        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("threads: debugger not initialized"))?;

        let threads = dbg.thread_state().unwrap_or_default();
        let existing_ids: HashSet<i64> = self.thread_cache.keys().copied().collect();
        let mut new_ids = HashSet::new();
        let mut new_cache = HashMap::new();
        let mut out = Vec::new();
        for t in threads {
            let id = t.thread.pid.as_raw() as i64;
            new_ids.insert(id);
            new_cache.insert(id, t.thread.pid);
            out.push(json!({
                "id": id,
                "name": format!("thread#{} ({})", t.thread.number, t.thread.pid),
            }));
        }

        for id in new_ids.difference(&existing_ids) {
            self.enqueue_thread_event("started", *id);
        }
        for id in existing_ids.difference(&new_ids) {
            self.enqueue_thread_event("exited", *id);
        }
        self.thread_cache = new_cache;
        Ok(out)
    }

    fn refresh_threads(&mut self) -> anyhow::Result<Vec<Value>> {
        self.refresh_threads_with_events()
    }

    pub(super) fn handle_threads(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let threads = self.refresh_threads()?;
        self.send_success_body(req, json!({"threads": threads}))
    }
}

#[cfg(test)]
mod tests {
    use super::is_panic_runtime_frame as p;

    #[test]
    fn panic_runtime_frames_detected() {
        // The frames between a break-on-panic trap and the user code.
        assert!(p(Some("core::panicking::panic_fmt")));
        assert!(p(Some("std::panicking::begin_panic_handler")));
        assert!(p(Some("std::panicking::rust_panic_with_hook")));
        assert!(p(Some("rust_panic")));
        assert!(p(Some("rust_begin_unwind")));
        assert!(p(Some("core::panicking::panic_bounds_check")));
        assert!(p(Some("_Unwind_RaiseException")));
    }

    #[test]
    fn user_and_library_frames_not_deemphasized() {
        // User code and ordinary library frames stay focusable — only the
        // panic chain is deemphasized.
        assert!(!p(Some("my_app::tx_parser::extract_output_cbors")));
        assert!(!p(Some("main")));
        assert!(!p(Some("alloc::vec::Vec<T>::push")));
        assert!(!p(Some("core::option::Option<T>::unwrap"))); // the culprit, keep it focusable
        assert!(!p(None));
    }
}

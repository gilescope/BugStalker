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
    /// File-scope `static`s — variables-view §5.4. Filtered by
    /// `variablesView.statics.scope` (default: current crate only,
    /// to avoid flooding the pane with std / dep statics).
    Statics,
    /// `thread_local!`s — variables-view §5.4. Same filter as
    /// `Statics`.
    ThreadLocals,
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
        let mut frames = Vec::new();
        for (i, f) in bt.iter().enumerate() {
            if self.consume_cancellation(req, None)? {
                return Ok(());
            }
            let (path, line, col, source_reference) = match f.place.as_ref() {
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
            let name = f.func_name.as_deref().unwrap_or("<unknown>").to_string();
            let frame_id = (thread_id << 16) | (i as i64);
            let source = if let Some(path) = path {
                let p = self.source_map.map_target_to_client(&path);
                Some(json!({"path": p}))
            } else if let Some(source_reference) = source_reference {
                let addr = f.ip.as_usize();
                let name = self
                    .disasm_cache_by_addr
                    .get(&addr)
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| format!("disasm @ 0x{addr:x}"));
                Some(json!({"name": name, "sourceReference": source_reference}))
            } else {
                None
            };
            frames.push(json!({
                "id": frame_id,
                "name": name,
                "source": source,
                "line": line.unwrap_or(0),
                "column": col.unwrap_or(0),
            }));
        }
        self.send_success_body(
            req,
            json!({"stackFrames": frames, "totalFrames": frames.len()}),
        )
    }

    pub(super) fn handle_scopes(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.has_replay_session() {
            return self.send_success_body(req, json!({ "scopes": [] }));
        }

        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("scopes: debugger not initialized"))?;

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
        // internals. Cached per (thread, frame) since the values
        // can change between steps.
        let statics_ref = if let Some(r) = self
            .scope_cache
            .get(&(thread_id, frame_num, ScopeKind::Statics))
            .copied()
        {
            r
        } else {
            let v = super::data::read_statics(dbg).unwrap_or_default();
            let r = self.vars.alloc(v);
            self.scope_cache
                .insert((thread_id, frame_num, ScopeKind::Statics), r);
            r
        };
        let tls_ref = if let Some(r) = self
            .scope_cache
            .get(&(thread_id, frame_num, ScopeKind::ThreadLocals))
            .copied()
        {
            r
        } else {
            let v = super::data::read_thread_locals(dbg).unwrap_or_default();
            let r = self.vars.alloc(v);
            self.scope_cache
                .insert((thread_id, frame_num, ScopeKind::ThreadLocals), r);
            r
        };

        let scopes = vec![
            json!({"name": "Locals", "variablesReference": locals_ref, "expensive": false}),
            json!({"name": "Arguments", "variablesReference": args_ref, "expensive": false}),
            json!({"name": "Statics", "variablesReference": statics_ref, "expensive": false}),
            json!({"name": "Thread-locals", "variablesReference": tls_ref, "expensive": false}),
        ];

        self.send_success_body(req, json!({"scopes": scopes}))
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

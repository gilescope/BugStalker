// SPDX-License-Identifier: MIT
//! Stop-level live reverse stepping for normal debuggee sessions.
//!
//! Captures a ring buffer of writable-memory + per-thread-register
//! snapshots at every stop; `stepBack` rewinds the focused thread to
//! the snapshot taken at the *previous* stop. Symmetric across macOS
//! (Mach `mach_vm_*` primitives) and Linux (`/proc/<pid>/{maps,mem}`
//! + ptrace `GETREGS`/`SETREGS`).
//!
//! Distinct from trace-driven replay (`replay.rs`): no recording up
//! front, no `bs --record` step. The ring lives in the supervisor
//! process and survives only the current debug session.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::VecDeque;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::ThreadFocusByPid;
use crate::dap::yadap::protocol::DapRequest;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::dap::yadap::protocol::InternalEvent;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::debugger::register::RegisterMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::{Context as _, anyhow};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd::Pid;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::json;

// Cross-platform bridge for the writable-state primitive lives in
// `crate::debugger::platform_checkpoint` — shared with the EnC
// snapshot path so both consumers get the same `WritableState`
// alias and the same capture/restore semantics.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::debugger::platform_checkpoint::{self, WritableState};

#[cfg(any(target_os = "linux", target_os = "macos"))]
const LIVE_REVERSE_CAPACITY: usize = 32;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
pub(super) struct LiveReverseHistory {
    entries: VecDeque<LiveReverseCheckpoint>,
    next_sequence: u64,
    capabilities_announced: bool,
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Default)]
pub(super) struct LiveReverseHistory;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone)]
struct LiveReverseCheckpoint {
    sequence: u64,
    proc_pid: Pid,
    focused_pid: Pid,
    pc: u64,
    writable: WritableState,
    registers: Vec<ThreadRegisters>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone)]
struct ThreadRegisters {
    pid: Pid,
    registers: RegisterMap,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy)]
struct RestoredLiveReverse {
    sequence: u64,
    focused_pid: Pid,
    pc: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LiveReverseHistory {
    fn has_announced_capabilities(&self) -> bool {
        self.capabilities_announced
    }

    fn mark_capabilities_announced(&mut self) {
        self.capabilities_announced = true;
    }

    fn push(&mut self, mut checkpoint: LiveReverseCheckpoint) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        checkpoint.sequence = self.next_sequence;
        if self.entries.len() == LIVE_REVERSE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(checkpoint);
    }

    fn previous(&mut self) -> Option<LiveReverseCheckpoint> {
        if self.entries.len() < 2 {
            return None;
        }
        self.entries.pop_back();
        self.entries.back().cloned()
    }
}

impl super::DebugSession {
    pub(super) fn capture_live_reverse_stop(&mut self) {
        if self.has_replay_session() || self.debugger.is_none() {
            return;
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        match self.capture_live_reverse_stop_inner() {
            Ok(true) if !self.live_reverse.has_announced_capabilities() => {
                self.live_reverse.mark_capabilities_announced();
                self.enqueue_capabilities(json!({ "supportsStepBack": true }));
            }
            Ok(_) => {}
            Err(err) => {
                log::warn!(target: "dap", "live reverse checkpoint capture failed: {err:#}");
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            log::debug!(target: "dap", "live reverse checkpoints are unsupported on this host");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn capture_live_reverse_stop_inner(&mut self) -> anyhow::Result<bool> {
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("live reverse capture: debugger not initialized"))?;
        let proc_pid = dbg.process().pid();
        let focused_pid = dbg.ecx().pid_on_focus();
        let pc = dbg.ecx().location().pc.as_u64();

        let mut pids = dbg
            .thread_state()
            .unwrap_or_default()
            .into_iter()
            .map(|thread| thread.thread.pid)
            .collect::<Vec<_>>();
        if !pids.contains(&focused_pid) {
            pids.push(focused_pid);
        }
        pids.sort_by_key(|pid| pid.as_raw());
        pids.dedup_by(|a, b| a.as_raw() == b.as_raw());

        let mut registers = Vec::new();
        for pid in pids {
            match RegisterMap::current(pid) {
                Ok(registers_for_thread) => registers.push(ThreadRegisters {
                    pid,
                    registers: registers_for_thread,
                }),
                Err(err) => {
                    log::debug!(
                        target: "dap",
                        "live reverse register capture skipped thread {pid}: {err}",
                    );
                }
            }
        }
        if registers.is_empty() {
            return Err(anyhow!(
                "live reverse capture: no thread registers captured"
            ));
        }

        let writable = platform_checkpoint::capture(proc_pid)?;
        self.live_reverse.push(LiveReverseCheckpoint {
            sequence: 0,
            proc_pid,
            focused_pid,
            pc,
            writable,
            registers,
        });
        Ok(true)
    }

    pub(super) fn handle_live_step_back(
        &mut self,
        req: &DapRequest,
        thread_id: i64,
    ) -> anyhow::Result<()> {
        // On unsupported hosts the `return` short-circuits the
        // function; the platform-specific block below is the
        // real implementation.
        #[allow(clippy::needless_return)]
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = thread_id;
            return self.send_err(
                req,
                "stepBack: live reverse execution is not implemented on this host",
            );
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _requested_pid = self
                .thread_cache
                .get(&thread_id)
                .copied()
                .unwrap_or_else(|| Pid::from_raw(thread_id as i32));
            let Some(checkpoint) = self.live_reverse.previous() else {
                return self.send_err(req, "stepBack: no earlier live checkpoint is available");
            };
            let restored = self.restore_live_reverse_checkpoint(&checkpoint)?;

            self.send_success_body(
                req,
                json!({
                    "sequence": restored.sequence,
                    "pc": restored.pc,
                }),
            )?;
            self.begin_stop_epoch();
            let _ = self.refresh_threads_with_events();
            let (source_path, line, column, stack_trace) = self.current_stop_snapshot();
            self.last_stop = Some(super::control::LastStop {
                reason: "step".to_string(),
                description: Some(format!("Restored checkpoint {}", restored.sequence)),
                signal: None,
                source_path,
                line,
                column,
                stack_trace,
            });
            self.enqueue_invalidated(vec![
                "variables".to_string(),
                "stack".to_string(),
                "memory".to_string(),
            ]);
            self.enqueue_event(InternalEvent::Stopped {
                reason: "step".to_string(),
                thread_id: Some(restored.focused_pid.as_raw() as i64),
                description: Some(format!("Restored checkpoint {}", restored.sequence)),
                preserve_focus_hint: false,
            });
            self.drain_events()
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restore_live_reverse_checkpoint(
        &mut self,
        checkpoint: &LiveReverseCheckpoint,
    ) -> anyhow::Result<RestoredLiveReverse> {
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("stepBack: debugger not initialized"))?;
        let report = platform_checkpoint::restore(checkpoint.proc_pid, &checkpoint.writable)?;
        if report.skipped > 0 {
            log::warn!(
                target: "dap",
                "live reverse restore skipped {} writable region(s)",
                report.skipped,
            );
        }

        let mut focused_restored = false;
        for thread in &checkpoint.registers {
            match thread.registers.clone().persist(thread.pid) {
                Ok(()) => {
                    focused_restored |= thread.pid == checkpoint.focused_pid;
                }
                Err(err) if thread.pid == checkpoint.focused_pid => {
                    return Err(err).with_context(|| {
                        format!("restore focused thread registers for {}", thread.pid)
                    });
                }
                Err(err) => {
                    log::warn!(
                        target: "dap",
                        "live reverse skipped register restore for thread {}: {err}",
                        thread.pid,
                    );
                }
            }
        }
        if !focused_restored {
            return Err(anyhow!(
                "stepBack: focused thread {} was not restored",
                checkpoint.focused_pid
            ));
        }

        let _ = dbg.set_thread_into_focus_by_pid(checkpoint.focused_pid);
        let _ = dbg.set_frame_into_focus(0);
        Ok(RestoredLiveReverse {
            sequence: checkpoint.sequence,
            focused_pid: checkpoint.focused_pid,
            pc: checkpoint.pc,
        })
    }
}

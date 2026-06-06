// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::path::Path;

use super::ThreadFocusByPid;
use crate::dap::yadap::protocol::{DapRequest, InternalEvent};
use crate::debugger;
use crate::debugger::LineInstrCount;
use crate::debugger::address::{GlobalAddress, RelocatedAddress};
use crate::debugger::variable::dqe::Literal;
use crate::ui::command::parser::expression as bs_expr;
use anyhow::{Context, anyhow};
use chumsky::Parser as _;
use chumsky::prelude::end;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde_json::json;

/// Max single-steps `step_over_or_count` will take before falling back to a
/// normal step-over (a hot same-line loop). ~4k traps ≈ tens of ms — snappy for
/// the common short line; bigger lines use the rusage estimate (accurate at that
/// scale anyway). See `debug-step-costs.md`.
const STEP_COUNT_BUDGET: u64 = 4_096;

/// DAP step requests (`next`/`stepIn`/`stepOut`) carry an optional
/// `granularity`; the Disassembly View sends `"instruction"` to advance one
/// machine instruction rather than a whole source line.
fn wants_instruction_step(req: &DapRequest) -> bool {
    req.arguments
        .get("granularity")
        .and_then(serde_json::Value::as_str)
        == Some("instruction")
}

#[derive(Debug, Clone)]
pub struct LastStop {
    pub reason: String,
    pub description: Option<String>,
    pub signal: Option<i32>,
    pub source_path: Option<String>,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub stack_trace: Option<String>,
}

impl super::DebugSession {
    pub fn current_thread_id(&mut self) -> Option<i64> {
        self.debugger
            .as_ref()
            .map(|d| d.ecx().pid_on_focus().as_raw() as i64)
            .or_else(|| {
                self.has_replay_session()
                    .then_some(super::replay::REPLAY_THREAD_ID)
            })
    }

    fn literal_truthy(literal: &Literal) -> bool {
        match literal {
            Literal::String(value) => !value.is_empty(),
            Literal::Int(value) => *value != 0,
            Literal::Float(value) => *value != 0.0,
            Literal::Address(value) => *value != 0,
            Literal::Bool(value) => *value,
            Literal::EnumVariant(_, _) => true,
            Literal::Array(items) => !items.is_empty(),
            Literal::AssocArray(items) => !items.is_empty(),
        }
    }

    fn value_truthy(value: &debugger::variable::value::Value) -> bool {
        if let Some(literal) = value.as_literal() {
            return Self::literal_truthy(&literal);
        }
        let rendered = super::data::render_value_to_string(value);
        if rendered == "<unavailable>" {
            return false;
        }
        let trimmed = rendered.trim();
        !(trimmed.is_empty()
            || trimmed == "0"
            || trimmed == "0x0"
            || trimmed == "false"
            || trimmed == "False")
    }

    fn evaluate_condition_expression(&mut self, expr: &str) -> anyhow::Result<bool> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Ok(true);
        }
        if let Ok(literal) = bs_expr::literal()
            .then_ignore(end())
            .parse(trimmed)
            .into_result()
        {
            return Ok(Self::literal_truthy(&literal));
        }
        let dqe = bs_expr::parser()
            .parse(trimmed)
            .into_result()
            .map_err(|e| anyhow!("condition parse error: {e:?}"))?;
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("evaluate condition: debugger not initialized"))?;
        let results = dbg
            .read_variable(dqe)
            .context("evaluate condition read_variable")?;
        let Some(result) = results.into_iter().next() else {
            return Ok(false);
        };
        let (_id, value) = result.into_identified_value();
        Ok(Self::value_truthy(&value))
    }

    fn with_breakpoint_record_mut<T>(
        &mut self,
        addr: debugger::address::Address,
        f: impl FnOnce(&mut super::breakpoint::BreakpointRecord) -> T,
    ) -> Option<T> {
        for records in self.breakpoints_by_source.values_mut() {
            if let Some(record) = records
                .iter_mut()
                .find(|record| record.addresses.contains(&addr))
            {
                return Some(f(record));
            }
        }
        if let Some(record) = self
            .function_breakpoints
            .iter_mut()
            .find(|record| record.addresses.contains(&addr))
        {
            return Some(f(record));
        }
        if let Some(record) = self
            .instruction_breakpoints
            .iter_mut()
            .find(|record| record.addresses.contains(&addr))
        {
            return Some(f(record));
        }
        None
    }

    fn record_breakpoint_hit(
        &mut self,
        addr: debugger::address::Address,
    ) -> Option<super::breakpoint::BreakpointHitInfo> {
        self.with_breakpoint_record_mut(addr, |record| {
            record.hit_count = record.hit_count.saturating_add(1);
            super::breakpoint::BreakpointHitInfo {
                id: record.id,
                condition: record.condition.clone(),
                hit_condition: record.hit_condition.clone(),
                log_message: record.log_message.clone(),
                hit_count: record.hit_count,
            }
        })
    }

    fn evaluate_expression_string(&mut self, expr: &str) -> anyhow::Result<String> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }
        if let Ok(literal) = bs_expr::literal()
            .then_ignore(end())
            .parse(trimmed)
            .into_result()
        {
            return Ok(literal.to_string());
        }
        let dqe = bs_expr::parser()
            .parse(trimmed)
            .into_result()
            .map_err(|e| anyhow!("log point parse error: {e:?}"))?;
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("log point: debugger not initialized"))?;
        let results = dbg.read_variable(dqe).context("log point read_variable")?;
        if results.is_empty() {
            return Ok("<no result>".to_string());
        }
        let result = results.into_iter().next().unwrap();
        let (_id, val) = result.into_identified_value();
        Ok(super::data::render_value_to_string(&val))
    }

    fn format_log_message(&mut self, template: &str) -> anyhow::Result<String> {
        let mut output = String::new();
        let mut chars = template.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '{' {
                let mut expr = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '}' {
                        break;
                    }
                    expr.push(next);
                }
                if expr.is_empty() {
                    output.push_str("{}");
                } else {
                    match self.evaluate_expression_string(expr.trim()) {
                        Ok(val) => output.push_str(&val),
                        Err(err) => output.push_str(&format!("<error: {err}>")),
                    }
                }
            } else {
                output.push(ch);
            }
        }
        Ok(output)
    }

    fn should_skip_breakpoint(&mut self, pid: Pid, addr: RelocatedAddress) -> anyhow::Result<bool> {
        let Some(hit) = self.record_breakpoint_hit(debugger::address::Address::Relocated(addr))
        else {
            return Ok(false);
        };

        {
            let dbg = self
                .debugger
                .as_mut()
                .ok_or_else(|| anyhow!("breakpoint: debugger not initialized"))?;
            let _ = dbg.set_thread_into_focus_by_pid(pid);
        }

        if let Some(condition) = hit.condition.as_deref() {
            match self.evaluate_condition_expression(condition) {
                Ok(false) => return Ok(true),
                Ok(true) => {}
                Err(err) => {
                    let output = format!("Breakpoint {} condition error: {err}\n", hit.id);
                    self.enqueue_event(InternalEvent::Output {
                        category: "console",
                        output,
                    });
                    self.drain_events()?;
                    return Ok(false);
                }
            }
        }

        if let Some(hit_condition) = &hit.hit_condition {
            if let super::breakpoint::HitCondition::Invalid(raw) = hit_condition {
                let output = format!("Breakpoint {} hitCondition invalid: {raw}\n", hit.id);
                self.enqueue_event(InternalEvent::Output {
                    category: "console",
                    output,
                });
                self.drain_events()?;
            } else if !hit_condition.matches(hit.hit_count) {
                return Ok(true);
            }
        }

        if let Some(log_message) = hit.log_message.as_deref() {
            let mut output = self.format_log_message(log_message)?;
            if !output.ends_with('\n') {
                output.push('\n');
            }
            self.enqueue_event(InternalEvent::Output {
                category: "console",
                output,
            });
            self.drain_events()?;
            return Ok(true);
        }

        Ok(false)
    }

    fn exception_filter_for_stop(stop: &debugger::StopReason) -> Option<&'static str> {
        match stop {
            debugger::StopReason::SignalStop(_, _) => Some(super::EXCEPTION_FILTER_SIGNAL),
            debugger::StopReason::NoSuchProcess(_) => Some(super::EXCEPTION_FILTER_PROCESS),
            _ => None,
        }
    }

    fn should_stop_on_exception(&self, stop: &debugger::StopReason) -> bool {
        let Some(filter) = Self::exception_filter_for_stop(stop) else {
            return true;
        };
        self.exception_filters.iter().any(|value| value == filter)
    }

    pub fn emit_stop_reason(&mut self, stop: debugger::StopReason) -> anyhow::Result<()> {
        self.emit_stop_reason_with_options(stop, false)
    }

    /// `emit_stop_reason` variant with the DAP `preserveFocusHint`
    /// flag. Set the flag on stops that fire while the user is
    /// editing — VSCode otherwise grabs the text-editor cursor away
    /// from where they're typing onto the stop line. Used by
    /// `bs/applyPatch`'s auto-resume path: when a frame restart
    /// lands the user back at their original breakpoint, we don't
    /// want their typing focus snatched.
    pub fn emit_stop_reason_with_options(
        &mut self,
        stop: debugger::StopReason,
        preserve_focus_hint: bool,
    ) -> anyhow::Result<()> {
        let mut stop = stop;
        let _focus_hint_for_event = preserve_focus_hint;
        loop {
            while !self.should_stop_on_exception(&stop) {
                let dbg = self
                    .debugger
                    .as_mut()
                    .ok_or_else(|| anyhow!("continue: debugger not initialized"))?;
                stop = dbg
                    .continue_debugee_with_reason()
                    .context("continue after exception filter")?;
            }

            if let debugger::StopReason::Breakpoint(pid, addr) = stop
                && self.should_skip_breakpoint(pid, addr)?
            {
                let dbg = self
                    .debugger
                    .as_mut()
                    .ok_or_else(|| anyhow!("continue: debugger not initialized"))?;
                stop = dbg
                    .continue_debugee_with_reason()
                    .context("continue after breakpoint filter")?;
                continue;
            }
            break;
        }

        let (reason, thread_id, description, exited, last_reason, signal) = match stop {
            debugger::StopReason::DebugeeExit(code) => (
                "exited".to_string(),
                None,
                None,
                Some(code),
                "exited".to_string(),
                None,
            ),
            debugger::StopReason::DebugeeStart => (
                "entry".to_string(),
                self.current_thread_id(),
                Some("Debugee started".to_string()),
                None,
                "entry".to_string(),
                None,
            ),
            debugger::StopReason::Breakpoint(pid, _) => (
                "breakpoint".to_string(),
                Some(pid.as_raw() as i64),
                None,
                None,
                "breakpoint".to_string(),
                None,
            ),
            debugger::StopReason::Watchpoint(pid, _, _) => (
                "data breakpoint".to_string(),
                Some(pid.as_raw() as i64),
                None,
                None,
                "data breakpoint".to_string(),
                None,
            ),
            debugger::StopReason::SignalStop(pid, sign) => (
                "exception".to_string(),
                Some(pid.as_raw() as i64),
                Some(format!("Signal: {sign:?}")),
                None,
                super::EXCEPTION_FILTER_SIGNAL.to_string(),
                Some(sign as i32),
            ),
            debugger::StopReason::NoSuchProcess(pid) => (
                "exception".to_string(),
                Some(pid.as_raw() as i64),
                Some("No such process".to_string()),
                None,
                super::EXCEPTION_FILTER_PROCESS.to_string(),
                None,
            ),
        };

        if let Some(code) = exited {
            self.last_stop = None;
            self.enqueue_event(InternalEvent::Exited { code });
            self.drain_events()?;
            return Ok(());
        }

        self.begin_stop_epoch();
        let _ = self.refresh_threads_with_events();
        self.capture_live_reverse_stop();

        let (source_path, line, column, stack_trace) = self
            .debugger
            .as_ref()
            .and_then(|dbg| {
                let pid = dbg.ecx().pid_on_focus();
                let bt = dbg.backtrace(pid).unwrap_or_default();
                if bt.is_empty() {
                    return None;
                }
                let (source_path, line, column) = bt.first().and_then(|frame| {
                    frame.place.as_ref().map(|place| {
                        let path = place.file.to_string_lossy();
                        let mapped = self.source_map.map_target_to_client(path.as_ref());
                        (
                            Some(mapped),
                            Some(place.line_number as i64),
                            Some(place.column_number as i64),
                        )
                    })
                })?;
                let stack_trace = bt
                    .iter()
                    .enumerate()
                    .map(|(idx, frame)| {
                        let name = frame.func_name.as_deref().unwrap_or("<unknown>");
                        if let Some(place) = frame.place.as_ref() {
                            let path = place.file.to_string_lossy();
                            let mapped = self.source_map.map_target_to_client(path.as_ref());
                            format!(
                                "#{idx} {name} ({mapped}:{}:{})",
                                place.line_number, place.column_number
                            )
                        } else {
                            format!("#{idx} {name} (<unknown>)")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some((source_path, line, column, Some(stack_trace)))
            })
            .unwrap_or((None, None, None, None));

        self.last_stop = Some(LastStop {
            reason: last_reason,
            description: description.clone(),
            signal,
            source_path,
            line,
            column,
            stack_trace,
        });

        self.enqueue_event(InternalEvent::Stopped {
            reason,
            thread_id,
            description,
            preserve_focus_hint: _focus_hint_for_event,
        });
        self.drain_events()?;
        Ok(())
    }

    pub(super) fn handle_continue(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.begin_running();

        let thread_id = self.current_thread_id();
        self.enqueue_event(InternalEvent::Continued {
            thread_id,
            all_threads_continued: true,
        });
        self.send_success_body(req, json!({"allThreadsContinued": true}))?;
        self.drain_events()?;
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("continue: debugger not initialized"))?;
        let stop = dbg.continue_debugee_with_reason().context("continue")?;
        self.emit_stop_reason(stop)
    }

    pub(super) fn handle_pause(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = match self.debugger.as_mut() {
            None => {
                self.send_err(req, "no active debug session")?;
                return Ok(());
            }
            Some(dbg) => dbg,
        };

        // Pause should stop the debugee and then emit a `stopped` event.
        match dbg.pause_debugee() {
            Ok(()) => {
                self.send_success(req)?;

                self.begin_stop_epoch();

                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Stopped {
                    reason: "pause".to_string(),
                    thread_id,
                    description: Some("Paused".to_string()),
                    preserve_focus_hint: false,
                });
            }
            Err(e) => {
                self.send_err(req, format!("pause failed: {e}"))?;
            }
        }

        Ok(())
    }

    pub(super) fn handle_restart(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.session_mode != Some(super::init::SessionMode::Launch) {
            return self.send_err(req, "restart is only supported for launch sessions");
        }

        self.begin_running();

        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("restart: debugger not initialized"))?;

        let stop = dbg
            .start_debugee_force_with_reason()
            .context("restart debugee")?;
        self.send_success(req)?;
        self.emit_stop_reason(stop)
    }

    pub(super) fn handle_next(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        // Disassembly View sends `granularity: "instruction"`, or the Source+ASM
        // webview has told us it is focused — either way step one machine instruction.
        if wants_instruction_step(req) || self.asm_view_focused {
            return self.step_one_instruction(req, "next");
        }
        self.begin_running();

        // With the perf overlay on, step-count no-call lines for an EXACT
        // instruction count (single-stepping ≡ step-over for them); otherwise a
        // plain step-over. `Some(n)` is stashed so the perf body reports the true
        // count instead of the rusage estimate.
        let exact_mode = self.perf_overlay_enabled();
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("next: debugger not initialized"))?;
        let result: Result<Option<u64>, debugger::Error> = if exact_mode {
            match dbg.step_over_or_count(STEP_COUNT_BUDGET) {
                Ok(LineInstrCount::Exact(n)) => Ok(Some(n)),
                Ok(LineInstrCount::Capped(_)) => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            dbg.step_over().map(|()| None)
        };

        match result {
            Ok(exact) => {
                self.set_perf_exact_instructions(exact);
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.begin_stop_epoch();
                self.capture_live_reverse_stop();

                self.last_stop = Some(LastStop {
                    reason: "pause".to_string(),
                    description: Some("Paused".to_string()),
                    signal: None,
                    source_path: None,
                    line: None,
                    column: None,
                    stack_trace: None,
                });

                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Stopped {
                    reason: "step".to_string(),
                    thread_id,
                    description: None,
                    preserve_focus_hint: false,
                });
                self.drain_events()
            }
            Err(debugger::Error::ProcessExit(code)) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.enqueue_event(InternalEvent::Exited { code });
                self.drain_events()
            }
            Err(e) => self.send_err(req, format!("next failed: {e}")),
        }
    }

    pub(super) fn handle_step_in(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        // Plain DAP `stepIn` (toolbar button, non-keybinding clients):
        // classic descend-anywhere behaviour. The "skip libraries"
        // mode is reached only through the `bs/stepIn` custom request,
        // which the keybindings send explicitly. (Open question in the
        // phase-12 plan: whether the toolbar default should flip to
        // skip-libraries; left as AnyFrame to preserve behaviour.)
        if wants_instruction_step(req) || self.asm_view_focused {
            return self.step_one_instruction(req, "stepIn");
        }
        self.step_in_with_mode(req, "stepIn", debugger::StepIntoMode::AnyFrame)
    }

    /// Single machine-instruction step (DAP `granularity: "instruction"`),
    /// shared by `next` and `stepIn` when the Disassembly View is focused.
    /// Emits the same `continued`/`stopped`/`exited` events as a line step.
    fn step_one_instruction(&mut self, req: &DapRequest, op: &str) -> anyhow::Result<()> {
        self.begin_running();
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("{op}: debugger not initialized"))?;
        match dbg.stepi() {
            Ok(()) => {
                // A single machine-instruction step is, by definition, exactly
                // one instruction. Tell the perf overlay so it reports 1 rather
                // than the rusage delta — which is dominated by the ~11k kernel
                // instructions of the trap itself (the trap floor).
                self.set_perf_exact_instructions(Some(1));
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.begin_stop_epoch();
                self.capture_live_reverse_stop();
                self.last_stop = Some(LastStop {
                    reason: "pause".to_string(),
                    description: Some("Paused".to_string()),
                    signal: None,
                    source_path: None,
                    line: None,
                    column: None,
                    stack_trace: None,
                });
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Stopped {
                    reason: "step".to_string(),
                    thread_id,
                    description: None,
                    // Instruction steps must not steal focus from the
                    // Source+ASM panel — without this VS Code refocuses
                    // the source editor after every stepi, clearing
                    // asm_view_focused before the next step fires.
                    preserve_focus_hint: true,
                });
                self.drain_events()
            }
            Err(debugger::Error::ProcessExit(code)) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.enqueue_event(InternalEvent::Exited { code });
                self.drain_events()
            }
            Err(e) => self.send_err(req, format!("{op} (instruction) failed: {e}")),
        }
    }

    /// BugStalker custom request: Step-In with an explicit "just my
    /// code" choice. `arguments.skipLibraries` (bool, default false)
    /// selects [`StepIntoMode::SkipLibraries`]; the VS Code keybindings
    /// (`alt+right` / `shift+alt+right`) map onto this so the modifier
    /// becomes a flag rather than fighting VS Code's `stepIn`
    /// bookkeeping. Emits the same `continued`/`stopped` events as a
    /// normal step.
    ///
    /// [`StepIntoMode::SkipLibraries`]: crate::debugger::StepIntoMode::SkipLibraries
    pub(super) fn handle_step_in_skip_libs(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        // When the Source+ASM panel is focused, alt+right / shift+alt+right
        // should step one machine instruction too — not just plain stepIn.
        // Otherwise the skip-libs path runs a full source step-in, inflating
        // the per-step instruction count.
        if wants_instruction_step(req) || self.asm_view_focused {
            return self.step_one_instruction(req, "bs/stepIn");
        }
        let skip = req
            .arguments
            .get("skipLibraries")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mode = if skip {
            debugger::StepIntoMode::SkipLibraries
        } else {
            debugger::StepIntoMode::AnyFrame
        };
        self.step_in_with_mode(req, "bs/stepIn", mode)
    }

    /// Shared engine for plain `stepIn` and the `bs/stepIn` custom
    /// request: dispatch to `step_into_with(mode)` and emit DAP events.
    fn step_in_with_mode(
        &mut self,
        req: &DapRequest,
        command: &str,
        mode: debugger::StepIntoMode,
    ) -> anyhow::Result<()> {
        self.begin_running();

        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("{command}: debugger not initialized"))?;

        match dbg.step_into_with(mode) {
            Ok(()) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.begin_stop_epoch();
                self.capture_live_reverse_stop();

                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Stopped {
                    reason: "step".to_string(),
                    thread_id,
                    description: None,
                    preserve_focus_hint: false,
                });
                self.drain_events()
            }
            Err(debugger::Error::ProcessExit(code)) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.enqueue_event(InternalEvent::Exited { code });
                self.drain_events()
            }
            Err(e) => self.send_err(req, format!("{command} failed: {e}")),
        }
    }

    pub(super) fn handle_step_out(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.begin_running();

        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("stepOut: debugger not initialized"))?;

        match dbg.step_out() {
            Ok(()) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.begin_stop_epoch();
                self.capture_live_reverse_stop();

                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Stopped {
                    reason: "step".to_string(),
                    thread_id,
                    description: None,
                    preserve_focus_hint: false,
                });
                self.drain_events()
            }
            Err(debugger::Error::ProcessExit(code)) => {
                let thread_id = self.current_thread_id();
                self.enqueue_event(InternalEvent::Continued {
                    thread_id,
                    all_threads_continued: true,
                });
                self.send_success_body(req, json!({"allThreadsContinued": true}))?;
                self.enqueue_event(InternalEvent::Exited { code });
                self.drain_events()
            }
            Err(e) => self.send_err(req, format!("stepOut failed: {e}")),
        }
    }

    pub(super) fn current_stop_snapshot(
        &self,
    ) -> (Option<String>, Option<i64>, Option<i64>, Option<String>) {
        self.debugger
            .as_ref()
            .and_then(|dbg| {
                let pid = dbg.ecx().pid_on_focus();
                let bt = dbg.backtrace(pid).unwrap_or_default();
                if bt.is_empty() {
                    return None;
                }
                let (source_path, line, column) = bt.first().and_then(|frame| {
                    frame.place.as_ref().map(|place| {
                        let path = place.file.to_string_lossy();
                        let mapped = self.source_map.map_target_to_client(path.as_ref());
                        (
                            Some(mapped),
                            Some(place.line_number as i64),
                            Some(place.column_number as i64),
                        )
                    })
                })?;
                let stack_trace = bt
                    .iter()
                    .enumerate()
                    .map(|(idx, frame)| {
                        let name = frame.func_name.as_deref().unwrap_or("<unknown>");
                        if let Some(place) = frame.place.as_ref() {
                            let path = place.file.to_string_lossy();
                            let mapped = self.source_map.map_target_to_client(path.as_ref());
                            format!(
                                "#{idx} {name} ({mapped}:{}:{})",
                                place.line_number, place.column_number
                            )
                        } else {
                            format!("#{idx} {name} (<unknown>)")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some((source_path, line, column, Some(stack_trace)))
            })
            .unwrap_or((None, None, None, None))
    }

    pub(super) fn emit_manual_stop(
        &mut self,
        reason: &str,
        description: Option<String>,
    ) -> anyhow::Result<()> {
        self.begin_stop_epoch();
        let _ = self.refresh_threads_with_events();
        self.capture_live_reverse_stop();

        let (source_path, line, column, stack_trace) = self.current_stop_snapshot();

        self.last_stop = Some(LastStop {
            reason: reason.to_string(),
            description: description.clone(),
            signal: None,
            source_path,
            line,
            column,
            stack_trace,
        });

        let thread_id = self.current_thread_id();
        self.enqueue_event(InternalEvent::Stopped {
            reason: reason.to_string(),
            thread_id,
            description,
            preserve_focus_hint: false,
        });
        self.drain_events()
    }

    pub(super) fn handle_goto(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("goto: debugger not initialized"))?;

        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("goto: arguments must be object"))?;
        if args.get("targetId").is_some() && args.get("targetId").and_then(|v| v.as_i64()).is_none()
        {
            return self.send_err(req, "goto: targetId must be an integer");
        }
        if args.get("instructionReference").is_some()
            && args
                .get("instructionReference")
                .and_then(|v| v.as_str())
                .is_none()
        {
            return self.send_err(req, "goto: instructionReference must be a string");
        }
        if args.get("threadId").is_some() && args.get("threadId").and_then(|v| v.as_i64()).is_none()
        {
            return self.send_err(req, "goto: threadId must be an integer");
        }
        let addr = if let Some(target_id) = args.get("targetId").and_then(|v| v.as_i64()) {
            if target_id < 0 {
                return self.send_err(req, "goto: targetId must be non-negative");
            }
            let addr = u64::try_from(target_id).map_err(|_| anyhow!("goto: targetId invalid"))?;
            addr as usize
        } else if let Some(reference) = args.get("instructionReference").and_then(|v| v.as_str()) {
            if reference.is_empty() {
                return self.send_err(req, "goto: instructionReference must not be empty");
            }
            super::parse_memory_reference_with_offset(reference, 0)?
        } else {
            return self.send_err(req, "goto: missing arguments.targetId");
        };

        if let Some(thread_id) = args.get("threadId").and_then(|v| v.as_i64()) {
            if thread_id < 0 {
                return self.send_err(req, "goto: threadId must be non-negative");
            }
            let pid_value =
                i32::try_from(thread_id).map_err(|_| anyhow!("goto: threadId out of range"))?;
            let pid = self
                .thread_cache
                .get(&thread_id)
                .copied()
                .unwrap_or_else(|| Pid::from_raw(pid_value));
            let _ = dbg.set_thread_into_focus_by_pid(pid);
        }

        dbg.set_pc(addr as u64).context("goto: set pc")?;
        let _ = dbg.set_frame_into_focus(0);

        self.send_success(req)?;
        self.emit_manual_stop("goto", None)
    }

    fn active_breakpoint_addresses(&self) -> HashSet<debugger::address::Address> {
        let mut out = HashSet::new();
        for records in self.breakpoints_by_source.values() {
            for record in records {
                out.extend(record.addresses.iter().copied());
            }
        }
        for record in &self.function_breakpoints {
            out.extend(record.addresses.iter().copied());
        }
        for record in &self.instruction_breakpoints {
            out.extend(record.addresses.iter().copied());
        }
        out
    }

    pub(super) fn handle_goto_targets(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("gotoTargets: arguments must be object"))?;
        let source_path = args
            .get("source")
            .and_then(|v| v.get("path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("gotoTargets: missing arguments.source.path"))?;
        if source_path.is_empty() {
            return self.send_err(req, "gotoTargets: source.path must not be empty");
        }
        let line = args
            .get("line")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("gotoTargets: missing arguments.line"))?;
        if line < 1 {
            return self.send_err(req, "gotoTargets: line must be >= 1");
        }
        if args.get("column").is_some() && args.get("column").and_then(|v| v.as_i64()).is_none() {
            return self.send_err(req, "gotoTargets: column must be an integer");
        }
        let column = args.get("column").and_then(|v| v.as_i64()).unwrap_or(1);
        if column < 1 {
            return self.send_err(req, "gotoTargets: column must be >= 1");
        }

        let target_path = self.source_map.map_client_to_target(source_path);
        let existing_breakpoints = self.active_breakpoint_addresses();
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("gotoTargets: debugger not initialized"))?;
        let mut views = dbg.set_breakpoint_at_line(&target_path, line as u64);
        if views.is_err()
            && let Some(base) = Path::new(&target_path).file_name().and_then(|s| s.to_str())
        {
            views = dbg.set_breakpoint_at_line(base, line as u64);
        }

        let (targets, remove_addrs) = match views {
            Ok(views) => {
                let mut targets = Vec::new();
                let mut remove_addrs = Vec::new();
                for view in &views {
                    let relocated = match view.addr {
                        debugger::address::Address::Relocated(addr) => addr,
                        debugger::address::Address::Global(addr) => {
                            debugger::address::RelocatedAddress::from(u64::from(addr))
                        }
                    };
                    let address = relocated.as_u64();
                    let target_id = i64::try_from(address)
                        .map_err(|_| anyhow!("gotoTargets: target address out of range"))?;

                    let (line_out, col_out, label) = if let Some(place) = view.place.as_ref() {
                        let mapped = self
                            .source_map
                            .map_target_to_client(place.file.to_string_lossy().as_ref());
                        (
                            place.line_number as i64,
                            place.column_number as i64,
                            format!("{mapped}:{}", place.line_number),
                        )
                    } else {
                        (line, column, format!("0x{address:x}"))
                    };

                    targets.push(json!({
                        "id": target_id,
                        "label": label,
                        "line": line_out,
                        "column": col_out,
                        "instructionPointerReference": format!("0x{address:x}"),
                    }));
                }

                for view in &views {
                    if !existing_breakpoints.contains(&view.addr) {
                        remove_addrs.push(view.addr);
                    }
                }

                (targets, remove_addrs)
            }
            Err(_) => (Vec::new(), Vec::new()),
        };

        for addr in remove_addrs {
            let _ = dbg.remove_breakpoint(addr);
        }

        self.send_success_body(req, json!({ "targets": targets }))
    }

    pub(super) fn handle_step_in_targets(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("stepInTargets: debugger not initialized"))?;
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("stepInTargets: arguments must be object"))?;

        let frame_id = args
            .get("frameId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("stepInTargets: missing arguments.frameId"))?;
        if frame_id < 0 {
            return self.send_err(req, "stepInTargets: frameId must be non-negative");
        }
        let (thread_id, frame_num) = Self::decode_frame_id(frame_id);
        let pid = self
            .thread_cache
            .get(&thread_id)
            .copied()
            .unwrap_or_else(|| Pid::from_raw(thread_id as i32));
        let _ = dbg.set_thread_into_focus_by_pid(pid);
        let _ = dbg.set_frame_into_focus(frame_num);

        let asm = dbg.disasm().context("stepInTargets: disasm")?;
        let mut targets = Vec::new();
        let mut seen = HashSet::new();

        for ins in asm.instructions {
            let mnemonic = ins.mnemonic.as_deref().unwrap_or("");
            if !mnemonic.starts_with("call") {
                continue;
            }
            let operands = ins.operands.as_deref().unwrap_or("");
            let Some(target_addr) = parse_call_target_addr(operands) else {
                continue;
            };
            if !seen.insert(target_addr) {
                continue;
            }

            let mut label = format!("0x{target_addr:x}");
            let mut line = None;
            let mut column = None;

            if let Ok(Some((name, place))) =
                dbg.resolve_function_at_pc(GlobalAddress::from(target_addr))
            {
                label = name;
                if let Some(place) = place {
                    line = Some(place.line_number as i64);
                    column = Some(place.column_number as i64);
                }
            }

            let id = i64::try_from(target_addr).unwrap_or(i64::MAX);
            let mut target = json!({ "id": id, "label": label });
            if let Some(line) = line {
                target["line"] = json!(line);
            }
            if let Some(column) = column {
                target["column"] = json!(column);
            }
            targets.push(target);
        }

        self.send_success_body(req, json!({ "targets": targets }))
    }

    pub(super) fn handle_step_back(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.has_replay_session() {
            return self.handle_replay_step_back(req);
        }
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("stepBack: arguments must be object"))?;
        if args.get("threadId").is_some() && args.get("threadId").and_then(|v| v.as_i64()).is_none()
        {
            return self.send_err(req, "stepBack: threadId must be an integer");
        }
        let thread_id = args
            .get("threadId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("stepBack: missing arguments.threadId"))?;
        if thread_id < 0 {
            return self.send_err(req, "stepBack: threadId must be non-negative");
        }
        let _ = i32::try_from(thread_id).map_err(|_| anyhow!("stepBack: threadId out of range"))?;
        self.handle_live_step_back(req, thread_id)
    }

    pub(super) fn handle_reverse_continue(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.has_replay_session() {
            return self.handle_replay_reverse_continue(req);
        }
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("reverseContinue: arguments must be object"))?;
        if args.get("threadId").is_some() && args.get("threadId").and_then(|v| v.as_i64()).is_none()
        {
            return self.send_err(req, "reverseContinue: threadId must be an integer");
        }
        let thread_id = args
            .get("threadId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("reverseContinue: missing arguments.threadId"))?;
        if thread_id < 0 {
            return self.send_err(req, "reverseContinue: threadId must be non-negative");
        }
        let _ = i32::try_from(thread_id)
            .map_err(|_| anyhow!("reverseContinue: threadId out of range"))?;
        self.send_err(
            req,
            "reverseContinue: reverse execution is not supported by the current engine",
        )
    }

    pub(super) fn handle_terminate_threads(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("terminateThreads: arguments must be object"))?;
        let thread_ids_value = args.get("threadIds");
        let thread_ids = match thread_ids_value {
            Some(value) => value
                .as_array()
                .ok_or_else(|| anyhow!("terminateThreads: threadIds must be array"))?
                .clone(),
            None => Vec::new(),
        };

        if thread_ids.is_empty() {
            self.send_success(req)?;
            self.terminate_debuggee();
            self.drain_events()?;
            return Ok(());
        }

        for thread_id in thread_ids {
            let Some(thread_id) = thread_id.as_i64() else {
                return self.send_err(req, "terminateThreads: threadIds must be integers");
            };
            if thread_id < 0 {
                return self.send_err(req, "terminateThreads: threadIds must be non-negative");
            }
            let pid_raw = i32::try_from(thread_id)
                .map_err(|_| anyhow!("terminateThreads: threadId out of range"))?;
            let pid = Pid::from_raw(pid_raw);
            signal::kill(pid, Signal::SIGTERM)
                .map_err(|err| anyhow!("terminateThreads: failed to signal {pid}: {err}"))?;
        }

        self.send_success(req)?;
        let _ = self.refresh_threads_with_events();
        self.drain_events()
    }

    fn terminate_debuggee(&mut self) {
        // Drop the debugger instance. For internally spawned debuggee this will SIGKILL and detach ptrace in Debugger::drop.
        // For external debuggee it will detach.
        let _ = self.debugger.take();
        self.enqueue_event(InternalEvent::Terminated);
    }

    pub(super) fn handle_terminate(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.send_success(req)?;
        self.terminate_debuggee();
        self.drain_events()?;
        Ok(())
    }

    pub(super) fn handle_disconnect(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let terminate = req
            .arguments
            .get("terminateDebuggee")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.send_success(req)?;
        if terminate {
            self.terminate_debuggee();
            self.drain_events()?;
        } else if let Some(mut dbg) = self.debugger.take() {
            dbg.detach().context("detach debuggee")?;
        }
        Ok(())
    }
}

fn parse_call_target_addr(operands: &str) -> Option<u64> {
    let idx = operands.find("0x")?;
    let hex = operands[idx + 2..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect::<String>();
    if hex.is_empty() {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

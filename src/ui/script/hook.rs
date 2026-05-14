// SPDX-License-Identifier: MIT
//! `EventHook` adapter that publishes structured events to the script
//! transport.
//!
//! The console's `TerminalHook` formats events as ANSI strings; this hook
//! emits them as JSON-RPC notifications down the same `OutputSink` the
//! request loop uses, with the writer mutex serialising both.

use nix::sys::signal::Signal;
use nix::unistd::Pid;

use crate::debugger::address::RelocatedAddress;
use crate::debugger::register::debug::BreakCondition;
use crate::debugger::variable::value::Value;
use crate::debugger::{EventHook, FunctionInfo, InlineFrame, PlaceDescriptor};
use crate::ui::structured::event::{Event, EventFrame, InlineFrameDto};

use super::transport::OutputSink;

pub struct ScriptHook {
    sink: OutputSink,
}

impl ScriptHook {
    pub fn new(sink: OutputSink) -> Self {
        Self { sink }
    }
}

fn frame_for(
    pc: RelocatedAddress,
    place: Option<PlaceDescriptor<'_>>,
    function: Option<&FunctionInfo>,
) -> EventFrame {
    EventFrame {
        function: function.and_then(|f| f.full_name()),
        file: place.as_ref().map(|p| p.file.display().to_string()),
        line: place.as_ref().map(|p| p.line_number),
        address: format!("0x{:x}", pc.as_u64()),
    }
}

fn dtos(chain: &[InlineFrame]) -> Vec<InlineFrameDto> {
    chain.iter().map(InlineFrameDto::from).collect()
}

impl EventHook for ScriptHook {
    fn on_breakpoint(
        &self,
        pc: RelocatedAddress,
        num: u32,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::BreakpointHit {
            breakpoint_id: num,
            thread: thread_num.map(|n| n as i32).unwrap_or(0),
            frame: Some(frame_for(pc, place, function)),
            inline_chain: vec![],
        });
        Ok(())
    }

    fn on_breakpoint_with_chain(
        &self,
        pc: RelocatedAddress,
        num: u32,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
        inline_chain: &[InlineFrame],
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::BreakpointHit {
            breakpoint_id: num,
            thread: thread_num.map(|n| n as i32).unwrap_or(0),
            frame: Some(frame_for(pc, place, function)),
            inline_chain: dtos(inline_chain),
        });
        Ok(())
    }

    fn on_watchpoint(
        &self,
        _pc: RelocatedAddress,
        num: u32,
        _place: Option<PlaceDescriptor<'_>>,
        condition: BreakCondition,
        dqe_string: Option<&str>,
        old_value: Option<&Value>,
        new_value: Option<&Value>,
        end_of_scope: bool,
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::WatchpointHit {
            watchpoint_id: num,
            thread: 0,
            condition: format!("{condition:?}"),
            dqe: dqe_string.map(str::to_owned),
            old_value: old_value.map(|v| format!("{v:?}")),
            new_value: new_value.map(|v| format!("{v:?}")),
            end_of_scope,
        });
        Ok(())
    }

    fn on_step(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::Step {
            thread: thread_num.map(|n| n as i32).unwrap_or(0),
            frame: Some(frame_for(pc, place, function)),
            inline_chain: vec![],
        });
        Ok(())
    }

    fn on_step_with_chain(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
        inline_chain: &[InlineFrame],
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::Step {
            thread: thread_num.map(|n| n as i32).unwrap_or(0),
            frame: Some(frame_for(pc, place, function)),
            inline_chain: dtos(inline_chain),
        });
        Ok(())
    }

    fn on_async_step(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        task_id: u64,
        task_completed: bool,
    ) -> anyhow::Result<()> {
        self.sink.emit_event(Event::AsyncStep {
            task_id,
            task_completed,
            frame: Some(frame_for(pc, place, function)),
        });
        Ok(())
    }

    fn on_signal(&self, signal: Signal) {
        self.sink.emit_event(Event::Signal {
            signal: signal as i32,
            signal_name: format!("{signal:?}"),
        });
    }

    fn on_exit(&self, code: i32) {
        self.sink.emit_event(Event::Exit { code });
    }

    fn on_process_install(&self, pid: Pid, _object: Option<&object::File<'_>>) {
        self.sink.emit_event(Event::ProcessInstalled {
            pid: pid.as_raw(),
        });
    }
}

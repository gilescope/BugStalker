// SPDX-License-Identifier: MIT
//! Replay-backed DAP handlers.

use crate::dap::yadap::protocol::{DapRequest, InternalEvent};
use anyhow::anyhow;
use bs_replay_driver::ReverseDebugger;
use bs_replay_driver::dap::{
    JumpTarget, ReplayCheckpointListRequest, ReplayLoadRequest, ReplayTimelineRequest,
    TimelineWaypoint, load,
};
use nix::unistd::Pid;
use serde_json::{Value, json};

pub(super) const REPLAY_THREAD_ID: i64 = 1;
pub(super) const REPLAY_FRAME_ID: i64 = REPLAY_THREAD_ID << 16;

impl super::DebugSession {
    pub(super) fn has_replay_session(&self) -> bool {
        self.replay_session.is_some()
    }

    pub(super) fn replay_position(&self) -> Option<u64> {
        self.replay_session.as_ref().map(ReverseDebugger::position)
    }

    pub(super) fn replay_current_pc(&self) -> anyhow::Result<Option<u64>> {
        match self.replay_session.as_ref() {
            Some(replay) => Ok(replay.current_pc()?),
            None => Ok(None),
        }
    }

    pub(super) fn handle_replay_load(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.debugger.is_some() {
            return self.send_err(
                req,
                "bs/replayLoad: cannot load a replay trace while a live debuggee is active",
            );
        }

        let trace_path = req
            .arguments
            .get("tracePath")
            .or_else(|| req.arguments.get("trace_path"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("bs/replayLoad: missing arguments.tracePath"))?
            .to_owned();

        let (replayer, response) = load(&ReplayLoadRequest {
            trace_path: trace_path.clone(),
        })?;
        let mut replay = ReverseDebugger::new(replayer);
        replay.seek_to(response.total_events);
        let event_index = replay.position();
        self.replay_session = Some(replay);
        self.thread_cache = std::collections::HashMap::from([(
            REPLAY_THREAD_ID,
            Pid::from_raw(REPLAY_THREAD_ID as i32),
        )]);

        self.send_success_body(
            req,
            json!({
                "tracePath": trace_path,
                "totalEvents": response.total_events,
                "totalSegments": response.total_segments,
                "totalCheckpoints": response.total_checkpoints,
                "recordedAt": response.recorded_at,
                "buildId": response.build_id,
                "eventIndex": event_index,
            }),
        )?;
        self.enqueue_capabilities(json!({
            "supportsStepBack": true,
            "supportsReverseContinue": true,
        }));
        self.enqueue_event(InternalEvent::Thread {
            reason: "started",
            thread_id: REPLAY_THREAD_ID,
        });
        self.drain_events()
    }

    pub(super) fn handle_replay_checkpoint_list(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let replay = match self.replay_session.as_ref() {
            Some(replay) => replay,
            None => return self.send_err(req, "bs/replayCheckpointList: no replay trace loaded"),
        };
        let response = replay
            .replayer()
            .dap_checkpoint_list(&ReplayCheckpointListRequest::default())?;
        let checkpoints: Vec<_> = response
            .checkpoints
            .into_iter()
            .map(|checkpoint| {
                json!({
                    "index": checkpoint.index,
                    "eventIndex": checkpoint.event_index,
                })
            })
            .collect();
        self.send_success_body(req, json!({ "checkpoints": checkpoints }))
    }

    pub(super) fn handle_replay_jump(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let target = replay_jump_target(&req.arguments)
            .ok_or_else(|| anyhow!("bs/replayJump: missing target eventIndex/checkpointIndex"))?;
        let replay = match self.replay_session.as_mut() {
            Some(replay) => replay,
            None => return self.send_err(req, "bs/replayJump: no replay trace loaded"),
        };
        let target_event = match target {
            JumpTarget::EventIndex { event_index } => event_index,
            JumpTarget::Checkpoint { index } => {
                let headers = replay.replayer().reader().checkpoint_headers()?;
                let header = headers
                    .iter()
                    .find(|header| header.index == index)
                    .ok_or_else(|| anyhow!("bs/replayJump: checkpoint {index} not found"))?;
                header.event_index
            }
        };
        replay.seek_to(target_event);
        let restore_from_checkpoint = replay
            .replayer()
            .find_checkpoint_at_or_before(target_event)?
            .map(|header| header.index);

        self.send_success_body(
            req,
            json!({
                "eventIndex": target_event,
                "restoreFromCheckpoint": restore_from_checkpoint,
            }),
        )?;
        self.enqueue_event(InternalEvent::Stopped {
            reason: "goto".to_string(),
            thread_id: Some(REPLAY_THREAD_ID),
            description: Some(format!("Replay at event {target_event}")),
            preserve_focus_hint: false,
        });
        self.drain_events()
    }

    pub(super) fn handle_replay_timeline(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let replay = match self.replay_session.as_ref() {
            Some(replay) => replay,
            None => return self.send_err(req, "bs/replayTimeline: no replay trace loaded"),
        };
        let response = replay
            .replayer()
            .dap_timeline(&ReplayTimelineRequest::default())?;
        let waypoints: Vec<_> = response
            .waypoints
            .into_iter()
            .map(|waypoint| match waypoint {
                TimelineWaypoint::Checkpoint { index, event_index } => json!({
                    "kind": "checkpoint",
                    "index": index,
                    "eventIndex": event_index,
                }),
            })
            .collect();
        self.send_success_body(
            req,
            json!({
                "totalEvents": response.total_events,
                "waypoints": waypoints,
            }),
        )
    }

    pub(super) fn handle_replay_step_back(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if !valid_replay_thread(&req.arguments) {
            return self.send_err(req, "stepBack: threadId must be the replay thread");
        }
        let (event_index, event, pc) = {
            let replay = self
                .replay_session
                .as_mut()
                .ok_or_else(|| anyhow!("stepBack: no replay trace loaded"))?;
            let event = replay.rstep()?;
            let event_index = replay.position();
            let pc = replay.current_pc()?;
            (event_index, event, pc)
        };
        let event_debug = event.as_ref().map(|event| format!("{event:?}"));
        self.send_success_body(
            req,
            json!({
                "eventIndex": event_index,
                "pc": pc,
                "event": event_debug,
            }),
        )?;
        self.begin_stop_epoch();
        self.enqueue_event(InternalEvent::Stopped {
            reason: "step".to_string(),
            thread_id: Some(REPLAY_THREAD_ID),
            description: Some(format!("Replay at event {event_index}")),
            preserve_focus_hint: false,
        });
        self.drain_events()
    }

    pub(super) fn handle_replay_reverse_continue(
        &mut self,
        req: &DapRequest,
    ) -> anyhow::Result<()> {
        if !valid_replay_thread(&req.arguments) {
            return self.send_err(req, "reverseContinue: threadId must be the replay thread");
        }
        let event_index = {
            let replay = self
                .replay_session
                .as_mut()
                .ok_or_else(|| anyhow!("reverseContinue: no replay trace loaded"))?;
            replay.rcontinue()?
        };
        self.send_success_body(req, json!({ "eventIndex": event_index }))?;
        self.begin_stop_epoch();
        self.enqueue_event(InternalEvent::Stopped {
            reason: "step".to_string(),
            thread_id: Some(REPLAY_THREAD_ID),
            description: Some(format!("Replay at event {event_index}")),
            preserve_focus_hint: false,
        });
        self.drain_events()
    }
}

fn valid_replay_thread(arguments: &Value) -> bool {
    arguments
        .get("threadId")
        .and_then(Value::as_i64)
        .is_none_or(|thread_id| thread_id == REPLAY_THREAD_ID)
}

fn replay_jump_target(arguments: &Value) -> Option<JumpTarget> {
    if let Some(event_index) = arguments.get("eventIndex").and_then(Value::as_u64) {
        return Some(JumpTarget::EventIndex { event_index });
    }
    if let Some(index) = arguments
        .get("checkpointIndex")
        .or_else(|| arguments.get("checkpoint"))
        .and_then(Value::as_u64)
    {
        return Some(JumpTarget::Checkpoint { index });
    }
    let target = arguments.get("target")?;
    if let Some(event_index) = target.get("eventIndex").and_then(Value::as_u64) {
        return Some(JumpTarget::EventIndex { event_index });
    }
    if let Some(index) = target
        .get("checkpointIndex")
        .or_else(|| target.get("checkpoint"))
        .and_then(Value::as_u64)
    {
        return Some(JumpTarget::Checkpoint { index });
    }
    None
}

// SPDX-License-Identifier: MIT
//! One-shot Tier 2 → Tier 3 capture helper.
//!
//! Linux-only, since Tier 2 fork-checkpoint capture lives in
//! `bs_replay::linux`. The function composes:
//!
//! 1. `Tier2Capture::capture` — fork+SIGSTOP+SEIZE+memory+regs.
//! 2. `tier2::to_payload` — serialize the captured state.
//! 3. `TraceWriter::create` — fresh on-disk trace dir.
//! 4. `take_checkpoint(payload)` — stash inside the trace.
//! 5. `finish` — close.
//! 6. `Tier2Capture::kill` — SIGKILL the captured fork (we now
//!    have the bytes on disk; no need to keep the live ring entry).
//!
//! The result is a `CaptureReport` summarising the size of what
//! was written. A future DAP `bs/replayCapture` handler is a thin
//! wrapper around this function.

#![cfg(target_os = "linux")]

use std::path::Path;

use bs_replay::linux::fork_self::LinuxForkSelfMechanism;
use bs_replay::linux::tier2::{self, Tier2Capture, Tier2Error};
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::{TraceWriteError, TraceWriter};

/// Summary of one capture call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CaptureReport {
    /// 1-based index of the checkpoint inside the trace.
    pub checkpoint_index: u64,
    /// Size of the encoded Tier 2 state payload, in bytes.
    pub payload_bytes: u64,
}

/// Capture a single Tier 2 checkpoint and write it to a *fresh*
/// trace directory at `trace_path`. The path must not already
/// exist (reuses `TraceWriter::create`'s policy of refusing to
/// overwrite).
///
/// `key` is a caller-defined u64 — typically the event index at
/// the moment of capture. Phase 5 doesn't interpret it; future
/// recorders use it for ordering and replay-seek.
pub fn capture_one_shot(
    trace_path: impl AsRef<Path>,
    manifest: &Manifest,
    key: u64,
) -> Result<CaptureReport, RecordError> {
    let mut mech = LinuxForkSelfMechanism::new();
    let cap = Tier2Capture::capture(&mut mech, key).map_err(RecordError::Tier2)?;
    let payload = tier2::to_payload(&cap.state);
    let payload_bytes = payload.len() as u64;

    let mut writer = TraceWriter::create(&trace_path, manifest)
        .map_err(RecordError::Trace)?;
    let checkpoint_index = 1; // first checkpoint in a fresh trace
    writer
        .take_checkpoint(payload)
        .map_err(RecordError::Trace)?;
    writer.finish().map_err(RecordError::Trace)?;

    cap.kill(&mut mech).map_err(RecordError::Tier2)?;

    Ok(CaptureReport { checkpoint_index, payload_bytes })
}

/// Errors arising from `capture_one_shot`.
#[derive(thiserror::Error, Debug)]
pub enum RecordError {
    /// Tier 2 capture or kill failed.
    #[error("tier2: {0}")]
    Tier2(Tier2Error),
    /// Trace writer failed.
    #[error("trace: {0}")]
    Trace(TraceWriteError),
}

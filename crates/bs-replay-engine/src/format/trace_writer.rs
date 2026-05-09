// SPDX-License-Identifier: MIT
//! `TraceWriter` — append events to a trace directory.
//!
//! Events accumulate in memory; on rotation (size-driven, or on
//! demand by the caller) the buffer is wrapped in a `Segment`
//! `{header, events}`, rkyv-archived, lz4-frame-compressed, and
//! written to `event-NNNNNN.lz4`. Segments are immutable once
//! finalised.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use lz4_flex::frame::FrameEncoder;
use rkyv::rancor::Error as RkyvError;

use super::checkpoint::{Checkpoint, CheckpointHeader, CheckpointIoError, checkpoint_path};
use super::event::Event;
use super::manifest::Manifest;
use super::segment::{MANIFEST_FILENAME, Segment, SegmentHeader, segment_filename};

/// Default segment-size cap before auto-rotation, in *uncompressed*
/// bytes. Tracks the plan's "~16 MB segments" target. Conservative
/// — the running estimate over-counts slightly, so the actual
/// compressed file is comfortably under this.
pub const DEFAULT_SEGMENT_SIZE_BYTES: usize = 16 * 1024 * 1024;

/// Append events to a trace directory.
#[derive(Debug)]
pub struct TraceWriter {
    dir: PathBuf,
    pending: Vec<Event>,
    pending_size_bytes: usize,
    next_segment: u64,
    next_checkpoint: u64,
    /// Total events written across all rotated segments + the
    /// in-flight buffer. Used to stamp `event_index` into a
    /// checkpoint header so replay knows where to resume from.
    events_written: u64,
    segment_size_threshold: usize,
}

impl TraceWriter {
    /// Create a fresh trace directory and write its manifest.
    ///
    /// The directory must not already exist — refusing to overwrite
    /// is deliberate: a trace is the record of a debugging session
    /// and a stale segment from a prior run could silently corrupt
    /// a replay.
    pub fn create(dir: impl AsRef<Path>, manifest: &Manifest) -> Result<Self, TraceWriteError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir(&dir)?;
        let manifest_path = dir.join(MANIFEST_FILENAME);
        fs::write(&manifest_path, manifest.to_text())?;
        Ok(Self {
            dir,
            pending: Vec::new(),
            pending_size_bytes: 0,
            next_segment: 1,
            next_checkpoint: 1,
            events_written: 0,
            segment_size_threshold: DEFAULT_SEGMENT_SIZE_BYTES,
        })
    }

    /// Override the auto-rotate threshold (in uncompressed bytes).
    /// Mostly useful in tests; production callers can leave the
    /// default.
    pub fn with_segment_size(mut self, threshold: usize) -> Self {
        self.segment_size_threshold = threshold.max(1);
        self
    }

    /// Append `event` to the in-memory buffer of the current segment.
    /// Auto-rotates when the running size estimate crosses the
    /// configured threshold.
    pub fn write_event(&mut self, event: Event) -> Result<(), TraceWriteError> {
        let before = self.pending_size_bytes;
        let increment = event.approx_archive_size();
        self.pending_size_bytes = before.saturating_add(increment);
        // Plan §Invariants: "Aggregator counters monotonic-non-
        // decreasing during a session." Same shape applies to the
        // writer's pending-size estimate.
        debug_assert!(
            self.pending_size_bytes >= before,
            "pending_size_bytes saturated backwards: {before} → {}",
            self.pending_size_bytes,
        );
        self.pending.push(event);
        let prev_events_written = self.events_written;
        self.events_written += 1;
        debug_assert!(
            self.events_written > prev_events_written,
            "events_written counter overflowed",
        );
        if self.pending_size_bytes >= self.segment_size_threshold {
            self.rotate()?;
        }
        Ok(())
    }

    /// Take a checkpoint snapshot at the current point in the event
    /// stream. `payload` is opaque — the format layer just stores
    /// it. Tier 3 replay (sub-phase 3C) writes the actual memory +
    /// register snapshot in there.
    ///
    /// Forces a segment rotation first so the checkpoint's
    /// `event_index` is unambiguous: every event up to and
    /// including `events_written` is on disk in a finalised
    /// segment, so replay restoring from this checkpoint resumes
    /// at exactly that offset.
    pub fn take_checkpoint(&mut self, payload: Vec<u8>) -> Result<(), TraceWriteError> {
        self.rotate()?;
        // Plan §Invariants: rotate() flushed everything in-flight,
        // so the buffer must be empty before we stamp event_index.
        debug_assert!(
            self.pending.is_empty(),
            "rotate() left {} pending events; checkpoint event_index would be wrong",
            self.pending.len(),
        );
        debug_assert!(self.next_checkpoint > 0, "checkpoint counter is 1-based");
        let checkpoint = Checkpoint {
            header: CheckpointHeader {
                index: self.next_checkpoint,
                event_index: self.events_written,
            },
            payload,
        };
        let path = checkpoint_path(&self.dir, self.next_checkpoint);
        checkpoint
            .write_to(&path)
            .map_err(TraceWriteError::Checkpoint)?;
        self.next_checkpoint += 1;
        Ok(())
    }

    /// Flush the in-memory buffer to the next segment file. No-op
    /// if the buffer is empty.
    pub fn rotate(&mut self) -> Result<(), TraceWriteError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let events = std::mem::take(&mut self.pending);
        // Plan §Invariants: "segment.events.len() > 0".
        debug_assert!(
            !events.is_empty(),
            "empty-segment guard above should have already returned",
        );
        debug_assert!(self.next_segment > 0, "segment counter is 1-based");
        let segment = Segment {
            header: SegmentHeader {
                index: self.next_segment,
                event_count: events.len() as u64,
            },
            events,
        };
        // Plan §Invariants: "header.event_count == events.len()".
        debug_assert_eq!(
            segment.header.event_count as usize,
            segment.events.len(),
            "SegmentHeader.event_count desynced from events.len()",
        );
        let archived = rkyv::to_bytes::<RkyvError>(&segment).map_err(TraceWriteError::Archive)?;
        let path = self.dir.join(segment_filename(self.next_segment));
        let file = File::create(&path)?;
        let mut encoder = FrameEncoder::new(BufWriter::new(file));
        encoder.write_all(&archived)?;
        encoder
            .finish()
            .map_err(|e| TraceWriteError::Lz4(format!("{e}")))?
            .flush()?;
        self.pending_size_bytes = 0;
        self.next_segment += 1;
        Ok(())
    }

    /// Flush any pending events and close the writer.
    pub fn finish(mut self) -> Result<(), TraceWriteError> {
        self.rotate()
    }

    /// Index that the *next* segment to be written will carry.
    /// Useful for tests that want to assert rotation behaviour.
    pub fn next_segment_index(&self) -> u64 {
        self.next_segment
    }
}

/// Errors arising from trace writes.
#[derive(thiserror::Error, Debug)]
pub enum TraceWriteError {
    /// Filesystem I/O failed.
    #[error("trace write I/O: {0}")]
    Io(#[from] std::io::Error),
    /// rkyv archive of the pending event buffer failed.
    #[error("trace archive: {0}")]
    Archive(RkyvError),
    /// LZ4 frame encoder failed at finish.
    #[error("trace lz4: {0}")]
    Lz4(String),
    /// Checkpoint file write failed.
    #[error("checkpoint: {0}")]
    Checkpoint(CheckpointIoError),
}

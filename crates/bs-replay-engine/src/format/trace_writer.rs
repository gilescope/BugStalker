// SPDX-License-Identifier: MIT
//! `TraceWriter` — append events to a trace directory.
//!
//! One archive per segment: events accumulate in memory; on rotation
//! (or finish) the buffer is rkyv-archived as `Vec<Event>`,
//! lz4-frame-compressed, and written to `event-NNNNNN.lz4`. Segments
//! are immutable once finalised.
//!
//! Sub-phase 3A scaffold. Real size-driven rotation lands later;
//! today the caller decides when to rotate.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use lz4_flex::frame::FrameEncoder;
use rkyv::rancor::Error as RkyvError;

use super::event::Event;
use super::manifest::Manifest;
use super::segment::{segment_filename, MANIFEST_FILENAME};

/// Append events to a trace directory.
#[derive(Debug)]
pub struct TraceWriter {
    dir: PathBuf,
    pending: Vec<Event>,
    next_segment: u64,
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
        Ok(Self { dir, pending: Vec::new(), next_segment: 1 })
    }

    /// Append `event` to the in-memory buffer of the current segment.
    pub fn write_event(&mut self, event: Event) {
        self.pending.push(event);
    }

    /// Flush the in-memory buffer to the next segment file. No-op
    /// if the buffer is empty.
    pub fn rotate(&mut self) -> Result<(), TraceWriteError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let archived = rkyv::to_bytes::<RkyvError>(&self.pending)
            .map_err(TraceWriteError::Archive)?;
        let path = self.dir.join(segment_filename(self.next_segment));
        let file = File::create(&path)?;
        let mut encoder = FrameEncoder::new(BufWriter::new(file));
        encoder.write_all(&archived)?;
        encoder
            .finish()
            .map_err(|e| TraceWriteError::Lz4(format!("{e}")))?
            .flush()?;
        self.pending.clear();
        self.next_segment += 1;
        Ok(())
    }

    /// Flush any pending events and close the writer.
    pub fn finish(mut self) -> Result<(), TraceWriteError> {
        self.rotate()
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
}

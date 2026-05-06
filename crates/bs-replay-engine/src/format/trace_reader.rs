// SPDX-License-Identifier: MIT
//! `TraceReader` — open a trace directory and walk it.
//!
//! Each segment file is lz4-frame-decompressed once into an owned
//! buffer; the rkyv-archived `Segment` and its events are then
//! accessed *zero-copy* as borrowed references into that buffer.
//! The amortised cost of decompression is paid per segment, not
//! per event — and replay re-reads the same segment many times
//! during a debugging session, so the per-event walk cost stays
//! at pointer-arithmetic speed.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use lz4_flex::frame::FrameDecoder;
use rkyv::rancor::Error as RkyvError;
use rkyv::vec::ArchivedVec;

use super::checkpoint::{
    checkpoint_path, parse_checkpoint_filename, Checkpoint, CheckpointIoError,
};
use super::event::{ArchivedEvent, Event};
use super::manifest::{Manifest, ManifestParseError};
use super::segment::{
    parse_segment_filename, ArchivedSegment, ArchivedSegmentHeader, Segment, MANIFEST_FILENAME,
};

/// Read-only handle to a trace directory.
#[derive(Debug)]
pub struct TraceReader {
    dir: PathBuf,
    manifest: Manifest,
    segments: Vec<u64>,
    checkpoints: Vec<u64>,
}

impl TraceReader {
    /// Open the trace at `dir`. Validates the manifest version,
    /// enumerates segment files, and asserts segment indices are
    /// strictly monotonic (Phase 5 invariant: `segment_index >=
    /// prev_segment_index`).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, TraceReadError> {
        let dir = dir.as_ref().to_path_buf();
        let manifest_path = dir.join(MANIFEST_FILENAME);
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|e| TraceReadError::ManifestIo(manifest_path.clone(), e))?;
        let manifest = Manifest::from_text(&manifest_text)?;
        if !manifest.format_version.is_supported() {
            return Err(TraceReadError::UnsupportedVersion(manifest.format_version.0));
        }

        let mut segments: Vec<u64> = Vec::new();
        let mut checkpoints: Vec<u64> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(idx) = parse_segment_filename(name) {
                    segments.push(idx);
                } else if let Some(idx) = parse_checkpoint_filename(name) {
                    checkpoints.push(idx);
                }
            }
        }
        segments.sort_unstable();
        checkpoints.sort_unstable();
        // Strict-monotonic check: filename parsers only accept
        // six-digit names so duplicates would have to come from
        // the same on-disk filename, which the filesystem already
        // excludes — but the assertion costs nothing and makes
        // the invariant explicit.
        for w in segments.windows(2) {
            debug_assert!(w[1] > w[0], "segment indices not strict-monotonic");
        }
        for w in checkpoints.windows(2) {
            debug_assert!(w[1] > w[0], "checkpoint indices not strict-monotonic");
        }

        Ok(Self { dir, manifest, segments, checkpoints })
    }

    /// The trace's manifest. Cheap; pre-parsed at [`Self::open`].
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Sorted segment indices present in the trace.
    pub fn segment_indices(&self) -> &[u64] {
        &self.segments
    }

    /// Sorted checkpoint indices present in the trace.
    pub fn checkpoint_indices(&self) -> &[u64] {
        &self.checkpoints
    }

    /// Read checkpoint `idx` from disk. Validates the header's
    /// stored index matches the filename — same shape of check the
    /// segment reader does.
    pub fn open_checkpoint(&self, idx: u64) -> Result<Checkpoint, TraceReadError> {
        let path = checkpoint_path(&self.dir, idx);
        let cp = Checkpoint::read_from(&path).map_err(TraceReadError::Checkpoint)?;
        if cp.header.index != idx {
            return Err(TraceReadError::CheckpointHeaderMismatch {
                file_index: idx,
                header_index: cp.header.index,
            });
        }
        Ok(cp)
    }

    /// Decompress segment `idx` and return a reader over its events.
    /// Validates the segment header at open: `header.index == idx`
    /// and `header.event_count == events.len()`.
    pub fn open_segment(&self, idx: u64) -> Result<SegmentReader, TraceReadError> {
        let path = self.dir.join(super::segment::segment_filename(idx));
        let file = File::open(&path)
            .map_err(|e| TraceReadError::SegmentIo(path.clone(), e))?;
        let mut decoder = FrameDecoder::new(file);
        let mut decompressed = Vec::with_capacity(64 * 1024);
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| TraceReadError::SegmentIo(path.clone(), e))?;

        let archived =
            rkyv::access::<ArchivedSegment, RkyvError>(&decompressed)
                .map_err(TraceReadError::Archive)?;
        let stored_idx = archived.header.index.to_native();
        let stored_count = archived.header.event_count.to_native();
        let actual_count = archived.events.len() as u64;
        if stored_idx != idx {
            return Err(TraceReadError::HeaderMismatch {
                file_index: idx,
                header_index: stored_idx,
            });
        }
        if stored_count != actual_count {
            return Err(TraceReadError::EventCountMismatch {
                segment_index: idx,
                header_count: stored_count,
                actual_count,
            });
        }

        Ok(SegmentReader { decompressed })
    }
}

/// One decompressed segment held in memory. Events are exposed as
/// borrowed references into the owned buffer — no per-event parse.
#[derive(Debug)]
pub struct SegmentReader {
    decompressed: Vec<u8>,
}

impl SegmentReader {
    /// Borrowed zero-copy view of the segment's archive root.
    /// Header invariants have already been checked at open time.
    pub fn segment(&self) -> Result<&ArchivedSegment, TraceReadError> {
        rkyv::access::<ArchivedSegment, RkyvError>(&self.decompressed)
            .map_err(TraceReadError::Archive)
    }

    /// Borrowed zero-copy view of the header.
    pub fn header(&self) -> Result<&ArchivedSegmentHeader, TraceReadError> {
        Ok(&self.segment()?.header)
    }

    /// Borrowed zero-copy view of all events in this segment.
    pub fn events(&self) -> Result<&ArchivedVec<ArchivedEvent>, TraceReadError> {
        Ok(&self.segment()?.events)
    }

    /// Eagerly deserialize every event in this segment to owned
    /// values. Convenient for tests; production replay should
    /// prefer [`Self::events`] and walk the archived view.
    pub fn events_owned(&self) -> Result<Vec<Event>, TraceReadError> {
        let segment: Segment = rkyv::from_bytes::<Segment, RkyvError>(&self.decompressed)
            .map_err(TraceReadError::Archive)?;
        Ok(segment.events)
    }
}

/// Errors arising from trace reads.
#[derive(thiserror::Error, Debug)]
pub enum TraceReadError {
    /// Manifest file could not be read.
    #[error("trace manifest at {0:?}: {1}")]
    ManifestIo(PathBuf, std::io::Error),
    /// Manifest contents could not be parsed.
    #[error("trace manifest parse: {0}")]
    ManifestParse(#[from] ManifestParseError),
    /// Manifest declared a version this build can't read.
    #[error("trace format v{0} is newer than this build supports")]
    UnsupportedVersion(u32),
    /// Directory listing failed.
    #[error("trace directory: {0}")]
    Io(#[from] std::io::Error),
    /// Segment file I/O failed.
    #[error("trace segment at {0:?}: {1}")]
    SegmentIo(PathBuf, std::io::Error),
    /// rkyv archive validation failed.
    #[error("trace archive: {0}")]
    Archive(RkyvError),
    /// Segment header's `index` did not match its filename.
    #[error(
        "segment {file_index} header reports index {header_index}; file/header disagree"
    )]
    HeaderMismatch {
        /// Index parsed from the filename.
        file_index: u64,
        /// Index stored in the segment header.
        header_index: u64,
    },
    /// Segment header's `event_count` did not match the actual event vec length.
    #[error(
        "segment {segment_index} header reports {header_count} events but {actual_count} are stored"
    )]
    EventCountMismatch {
        /// Index of the offending segment.
        segment_index: u64,
        /// Count claimed by the header.
        header_count: u64,
        /// Count of events actually stored.
        actual_count: u64,
    },
    /// Checkpoint file I/O or archive error.
    #[error("checkpoint: {0}")]
    Checkpoint(CheckpointIoError),
    /// Checkpoint header's `index` did not match its filename.
    #[error(
        "checkpoint {file_index} header reports index {header_index}; file/header disagree"
    )]
    CheckpointHeaderMismatch {
        /// Index parsed from the filename.
        file_index: u64,
        /// Index stored in the checkpoint header.
        header_index: u64,
    },
}

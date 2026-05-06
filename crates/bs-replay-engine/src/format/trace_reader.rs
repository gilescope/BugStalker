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
use std::sync::OnceLock;

use lz4_flex::frame::FrameDecoder;
use rkyv::rancor::Error as RkyvError;
use rkyv::vec::ArchivedVec;

use super::checkpoint::{
    checkpoint_path, parse_checkpoint_filename, Checkpoint, CheckpointHeader, CheckpointIoError,
};
use super::event::{ArchivedEvent, Event};
use super::manifest::{Manifest, ManifestParseError};
use super::segment::{
    parse_segment_filename, ArchivedSegment, ArchivedSegmentHeader, Segment, MANIFEST_FILENAME,
};

/// Range info for one segment: where in the global event-index
/// space its events live.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SegmentRange {
    /// Filename / SegmentHeader.index.
    pub segment_index: u64,
    /// Index of the first event stored in this segment, in the
    /// global event-index space across the whole trace.
    pub first_event_index: u64,
    /// Number of events this segment carries.
    pub event_count: u64,
}

impl SegmentRange {
    /// Inclusive range `[first_event_index, first_event_index + event_count)`.
    pub fn contains(&self, event_index: u64) -> bool {
        event_index >= self.first_event_index
            && event_index < self.first_event_index + self.event_count
    }
}

/// Read-only handle to a trace directory.
#[derive(Debug)]
pub struct TraceReader {
    dir: PathBuf,
    manifest: Manifest,
    segments: Vec<u64>,
    checkpoints: Vec<u64>,
    /// Lazily computed per-segment ranges. First call to
    /// `segment_event_ranges()` decompresses every segment's header
    /// and populates this; subsequent calls are O(1). `OnceLock`
    /// not `OnceCell` so a `&TraceReader` shared across threads
    /// stays `Sync`.
    segment_ranges: OnceLock<Vec<SegmentRange>>,
    /// Lazily-computed mirror of every checkpoint header. Same
    /// pattern as `segment_ranges`.
    checkpoint_headers: OnceLock<Vec<CheckpointHeader>>,
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

        Ok(Self {
            dir,
            manifest,
            segments,
            checkpoints,
            segment_ranges: OnceLock::new(),
            checkpoint_headers: OnceLock::new(),
        })
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

    /// Per-segment event-index ranges across the whole trace.
    ///
    /// Computed once lazily on first call (decompresses every
    /// segment to read its header) and cached. Subsequent calls
    /// are O(1).
    pub fn segment_event_ranges(&self) -> Result<&[SegmentRange], TraceReadError> {
        if let Some(ranges) = self.segment_ranges.get() {
            return Ok(ranges);
        }
        let mut ranges = Vec::with_capacity(self.segments.len());
        let mut next_event_index: u64 = 0;
        for &idx in &self.segments {
            let seg = self.open_segment(idx)?;
            let header = seg.header()?;
            let count = header.event_count.to_native();
            ranges.push(SegmentRange {
                segment_index: idx,
                first_event_index: next_event_index,
                event_count: count,
            });
            next_event_index = next_event_index
                .checked_add(count)
                .expect("event-index space overflow — trace exceeded u64 events");
        }
        // OnceLock::set fails harmlessly if another thread won the
        // race; both populate identical contents.
        let _ = self.segment_ranges.set(ranges);
        Ok(self.segment_ranges.get().expect("just set"))
    }

    /// Locate the segment containing `event_index`. Returns
    /// `(segment_index, offset_within_segment)`. `None` if the
    /// requested event is past the end of the trace.
    pub fn segment_for_event(
        &self,
        event_index: u64,
    ) -> Result<Option<(u64, u64)>, TraceReadError> {
        let ranges = self.segment_event_ranges()?;
        // Binary search for the segment whose range contains
        // `event_index`. Ranges are sorted by `first_event_index`
        // and contiguous (each segment's first index = previous
        // segment's first + count), so partition_point returns
        // the segment-after; we walk one back.
        let pos = ranges.partition_point(|r| r.first_event_index <= event_index);
        if pos == 0 {
            // event_index is below the first range — only possible
            // if the trace is empty.
            return Ok(None);
        }
        let r = &ranges[pos - 1];
        if r.contains(event_index) {
            Ok(Some((r.segment_index, event_index - r.first_event_index)))
        } else {
            // Past end of trace.
            Ok(None)
        }
    }

    /// Headers of every checkpoint. Same lazy/cached shape as
    /// `segment_event_ranges`.
    pub fn checkpoint_headers(&self) -> Result<&[CheckpointHeader], TraceReadError> {
        if let Some(hs) = self.checkpoint_headers.get() {
            return Ok(hs);
        }
        let mut hs = Vec::with_capacity(self.checkpoints.len());
        for &idx in &self.checkpoints {
            let cp = self.open_checkpoint(idx)?;
            hs.push(cp.header.clone());
        }
        let _ = self.checkpoint_headers.set(hs);
        Ok(self.checkpoint_headers.get().expect("just set"))
    }

    /// Find the latest checkpoint whose `event_index <=
    /// target_event_index`. Returns `None` if no checkpoint
    /// covers that target (i.e. all checkpoints are after the
    /// target, so replay would have to walk from event 0).
    pub fn find_checkpoint_at_or_before(
        &self,
        target_event_index: u64,
    ) -> Result<Option<CheckpointHeader>, TraceReadError> {
        let headers = self.checkpoint_headers()?;
        // Binary-search for the right-most header with
        // event_index <= target. partition_point puts the dividing
        // line one *past* the last qualifying entry.
        let pos = headers.partition_point(|h| h.event_index <= target_event_index);
        if pos == 0 {
            Ok(None)
        } else {
            Ok(Some(headers[pos - 1].clone()))
        }
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

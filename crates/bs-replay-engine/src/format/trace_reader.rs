// SPDX-License-Identifier: MIT
//! `TraceReader` — open a trace directory and walk it.
//!
//! Each segment file is lz4-frame-decompressed once into an owned
//! buffer; rkyv-archived events are then accessed *zero-copy* as
//! borrowed references into that buffer. The amortised cost of
//! decompression is paid per segment, not per event — and replay
//! re-reads the same segment many times during a debugging session,
//! so the per-event walk cost stays at pointer-arithmetic speed.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use lz4_flex::frame::FrameDecoder;
use rkyv::rancor::Error as RkyvError;
use rkyv::vec::ArchivedVec;

use super::event::{ArchivedEvent, Event};
use super::manifest::{Manifest, ManifestParseError};
use super::segment::{parse_segment_filename, MANIFEST_FILENAME};

/// Read-only handle to a trace directory.
#[derive(Debug)]
pub struct TraceReader {
    dir: PathBuf,
    manifest: Manifest,
    segments: Vec<u64>,
}

impl TraceReader {
    /// Open the trace at `dir`. Validates the manifest version and
    /// enumerates segment files but does not read them — segments
    /// are decompressed on demand by [`Self::open_segment`].
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
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(idx) = parse_segment_filename(name) {
                    segments.push(idx);
                }
            }
        }
        segments.sort_unstable();

        Ok(Self { dir, manifest, segments })
    }

    /// The trace's manifest. Cheap; pre-parsed at [`Self::open`] time.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Sorted segment indices present in the trace.
    pub fn segment_indices(&self) -> &[u64] {
        &self.segments
    }

    /// Decompress segment `idx` and return a reader over its events.
    /// `idx` must be one of [`Self::segment_indices`].
    pub fn open_segment(&self, idx: u64) -> Result<SegmentReader, TraceReadError> {
        let path = self.dir.join(super::segment::segment_filename(idx));
        let file = File::open(&path)
            .map_err(|e| TraceReadError::SegmentIo(path.clone(), e))?;
        let mut decoder = FrameDecoder::new(file);
        let mut decompressed = Vec::with_capacity(64 * 1024);
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| TraceReadError::SegmentIo(path.clone(), e))?;
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
    /// Borrowed zero-copy view of all events in this segment.
    pub fn events(&self) -> Result<&ArchivedVec<ArchivedEvent>, TraceReadError> {
        rkyv::access::<ArchivedVec<ArchivedEvent>, RkyvError>(&self.decompressed)
            .map_err(TraceReadError::Archive)
    }

    /// Eagerly deserialize every event in this segment to owned
    /// values. Convenient for tests; production replay should
    /// prefer [`Self::events`] and walk the archived view.
    pub fn events_owned(&self) -> Result<Vec<Event>, TraceReadError> {
        let archived = self.events()?;
        rkyv::deserialize::<Vec<Event>, RkyvError>(archived)
            .map_err(TraceReadError::Archive)
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
}

// SPDX-License-Identifier: MIT
//! Trace-internal checkpoint snapshots.
//!
//! These are the snapshots embedded *inside* the trace directory —
//! distinct from Tier 2 fork-checkpoints (which are kernel `fork(2)`
//! children held in memory). A trace-internal checkpoint records
//! enough state to fast-forward replay to that point without
//! scanning from event 0; the recipe is:
//!
//! 1. Replay opens the trace, finds the latest checkpoint at or
//!    before the target time.
//! 2. Restores the recorded process state from the checkpoint
//!    payload (memory regions + registers; format owned by the
//!    replay engine, opaque to this layer).
//! 3. Continues from `header.event_index` in the segment stream.
//!
//! The payload is `Vec<u8>` here — the format crate doesn't pick
//! the contents. Tier 3 replay (sub-phase 3C) will land its own
//! payload encoder.
//!
//! On-disk filename: `checkpoint-NNNNNN.snap`. lz4-frame-compressed
//! per the same policy as event segments.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use rkyv::rancor::Error as RkyvError;
use rkyv::{Archive, Deserialize, Serialize};

/// Header of a `checkpoint-NNNNNN.snap` file.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct CheckpointHeader {
    /// Index in the trace's checkpoint sequence (1-based).
    pub index: u64,
    /// Index of the most recent event written before this snapshot.
    /// Used to align replay-from to the right event stream offset.
    pub event_index: u64,
}

/// Archive root of one checkpoint file: header + opaque payload.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct Checkpoint {
    /// Per-checkpoint metadata.
    pub header: CheckpointHeader,
    /// Opaque body — memory snapshot + registers + whatever else
    /// the replay engine decides to encode. The format crate sees
    /// only bytes.
    pub payload: Vec<u8>,
}

impl Checkpoint {
    /// Write `self` to `path` as an lz4-frame-compressed rkyv
    /// archive.
    pub fn write_to(&self, path: &Path) -> Result<(), CheckpointIoError> {
        let archived = rkyv::to_bytes::<RkyvError>(self)
            .map_err(CheckpointIoError::Archive)?;
        let file = File::create(path)?;
        let mut encoder = FrameEncoder::new(BufWriter::new(file));
        encoder.write_all(&archived)?;
        encoder
            .finish()
            .map_err(|e| CheckpointIoError::Lz4(format!("{e}")))?
            .flush()?;
        Ok(())
    }

    /// Read and deserialize a checkpoint file.
    pub fn read_from(path: &Path) -> Result<Self, CheckpointIoError> {
        let file = File::open(path)?;
        let mut decoder = FrameDecoder::new(file);
        let mut decompressed = Vec::with_capacity(64 * 1024);
        decoder.read_to_end(&mut decompressed)?;
        rkyv::from_bytes::<Self, RkyvError>(&decompressed)
            .map_err(CheckpointIoError::Archive)
    }
}

/// Errors arising from checkpoint file I/O.
#[derive(thiserror::Error, Debug)]
pub enum CheckpointIoError {
    /// Filesystem I/O failed.
    #[error("checkpoint I/O: {0}")]
    Io(#[from] std::io::Error),
    /// rkyv archive failed.
    #[error("checkpoint archive: {0}")]
    Archive(RkyvError),
    /// LZ4 frame encoder failed at finish.
    #[error("checkpoint lz4: {0}")]
    Lz4(String),
}

/// Build the filename for checkpoint `idx`. 6-digit zero-padded so
/// listings sort chronologically.
#[inline]
pub fn checkpoint_filename(idx: u64) -> String {
    format!("checkpoint-{idx:06}.snap")
}

/// Inverse of [`checkpoint_filename`].
#[inline]
pub fn parse_checkpoint_filename(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("checkpoint-")?.strip_suffix(".snap")?;
    if stem.len() != 6 {
        return None;
    }
    stem.parse().ok()
}

/// Convenience: full path inside `dir` for checkpoint `idx`.
#[inline]
pub fn checkpoint_path(dir: &Path, idx: u64) -> PathBuf {
    dir.join(checkpoint_filename(idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rkyv_roundtrip() {
        let c = Checkpoint {
            header: CheckpointHeader { index: 3, event_index: 1024 },
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let bytes = rkyv::to_bytes::<RkyvError>(&c).unwrap();
        let back = rkyv::from_bytes::<Checkpoint, RkyvError>(&bytes).unwrap();
        assert_eq!(back.header.index, 3);
        assert_eq!(back.header.event_index, 1024);
        assert_eq!(back.payload, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn filename_parser_round_trips_and_rejects_garbage() {
        for idx in [1u64, 42, 999_999] {
            let name = checkpoint_filename(idx);
            assert_eq!(parse_checkpoint_filename(&name), Some(idx));
        }
        assert_eq!(parse_checkpoint_filename("event-000001.lz4"), None);
        assert_eq!(parse_checkpoint_filename("checkpoint-1.snap"), None); // not zero-padded
        assert_eq!(parse_checkpoint_filename("checkpoint-000001.lz4"), None);
    }
}

// SPDX-License-Identifier: MIT
//! Numbered, length-prefixed, zstd-compressed event segment.
//!
//! Skeleton only. Segment layout per
//! `doc/plans/phase-5-time-travel.md` § "3A. Trace format and storage":
//! variable-length, length-prefixed records, zstd-compressed via
//! pure-Rust `ruzstd` at `CompressionLevel::Fastest`. Segment size
//! ~16 MB; rotated on size or time.

use rkyv::{Archive, Deserialize, Serialize};

/// Header at the start of every event segment file.
///
/// rkyv-archived: replay mmaps the segment and accesses the header
/// (and event records) through the archived view, no parsing pass.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct SegmentHeader {
    /// Monotonic, non-decreasing index.
    /// Phase 5 invariant: `segment_index >= prev_segment_index`.
    pub index: u64,
    /// Number of events stored in this segment after decompression.
    /// Phase 5 invariant: `segment.events.len() > 0`.
    pub event_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rkyv_roundtrip() {
        let h = SegmentHeader { index: 7, event_count: 4096 };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        let back =
            rkyv::from_bytes::<SegmentHeader, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(h.index, back.index);
        assert_eq!(h.event_count, back.event_count);
    }

    #[test]
    fn rkyv_archived_access_is_zero_copy() {
        let h = SegmentHeader { index: 9, event_count: 1 };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        // access() reads the trailing root pointer and returns a
        // borrowed view into `bytes` — the zero-copy code path that
        // makes rkyv the right choice for replay.
        let archived = rkyv::access::<ArchivedSegmentHeader, rkyv::rancor::Error>(&bytes)
            .unwrap();
        assert_eq!(archived.index.to_native(), 9);
        assert_eq!(archived.event_count.to_native(), 1);
    }
}

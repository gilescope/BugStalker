// SPDX-License-Identifier: MIT
//! Numbered, lz4-frame-compressed event segment.
//!
//! Layout per `doc/plans/phase-5-time-travel.md` § "3A. Trace
//! format and storage": rkyv-archived `Segment { header, events }`
//! root, lz4-frame-compressed at rest. Segment size ~16 MB;
//! rotated on size (or on demand by the writer's caller).

use rkyv::{Archive, Deserialize, Serialize};

use super::event::Event;

/// Filename of the trace manifest at the trace directory root.
pub const MANIFEST_FILENAME: &str = "manifest.txt";

/// Build the filename for segment number `idx`. 6-digit zero-padded
/// so a directory listing sorts in chronological order.
#[inline]
pub fn segment_filename(idx: u64) -> String {
    format!("event-{idx:06}.lz4")
}

/// Inverse of [`segment_filename`] — parse `event-NNNNNN.lz4` →
/// `Some(NNNNNN)`. Returns `None` for any other filename shape so
/// the reader can ignore stray files (manifest, checkpoints, etc.).
#[inline]
pub fn parse_segment_filename(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("event-")?.strip_suffix(".lz4")?;
    if stem.len() != 6 {
        return None;
    }
    stem.parse().ok()
}

/// Header at the start of every event segment.
///
/// rkyv-archived as part of the [`Segment`] root: replay
/// decompresses the segment file once and accesses the header (and
/// event records) through the archived view, no parsing pass.
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

/// Archive root of one segment file: header + the events that were
/// written into it. The header's `event_count` mirrors `events.len()`
/// — the reader checks they agree at open time.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct Segment {
    /// Per-segment metadata.
    pub header: SegmentHeader,
    /// Events recorded into this segment, in record order.
    pub events: Vec<Event>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rkyv_roundtrip() {
        let h = SegmentHeader {
            index: 7,
            event_count: 4096,
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        let back = rkyv::from_bytes::<SegmentHeader, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(h.index, back.index);
        assert_eq!(h.event_count, back.event_count);
    }

    #[test]
    fn rkyv_archived_access_is_zero_copy() {
        let h = SegmentHeader {
            index: 9,
            event_count: 1,
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        // access() reads the trailing root pointer and returns a
        // borrowed view into `bytes` — the zero-copy code path that
        // makes rkyv the right choice for replay.
        let archived = rkyv::access::<ArchivedSegmentHeader, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(archived.index.to_native(), 9);
        assert_eq!(archived.event_count.to_native(), 1);
    }
}

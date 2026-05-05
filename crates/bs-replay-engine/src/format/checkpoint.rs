// SPDX-License-Identifier: MIT
//! Periodic full-process snapshots embedded inside the trace.
//!
//! Skeleton only. Snapshots enable replay seek without scanning from
//! the start. Cadence configurable per § "3A. Trace format and
//! storage" in the plan.

use rkyv::{Archive, Deserialize, Serialize};

/// Header of a `checkpoint-NNNNNN.snap` file.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct CheckpointHeader {
    /// Index in the trace's checkpoint sequence.
    pub index: u64,
    /// Index of the most recent event written before this snapshot.
    /// Used to align replay-from to the right event stream offset.
    pub event_index: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rkyv_roundtrip() {
        let h = CheckpointHeader { index: 3, event_index: 1024 };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        let back =
            rkyv::from_bytes::<CheckpointHeader, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(h.index, back.index);
        assert_eq!(h.event_index, back.event_index);
    }
}

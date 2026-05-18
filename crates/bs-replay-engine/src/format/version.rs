// SPDX-License-Identifier: MIT
//! Trace format magic + version.
//!
//! Phase 5 invariant: `trace.format_version <= MAX_SUPPORTED_FORMAT_VERSION`.

use core::fmt;

use rkyv::{Archive, Deserialize, Serialize};

/// Magic string at the head of every trace manifest.
///
/// Eight bytes, ASCII, fixed for the lifetime of the format. A
/// future-incompatible format change bumps [`FormatVersion`], not the
/// magic.
pub const TRACE_MAGIC: &[u8; 8] = b"BSREPLAY";

/// Highest format version this build can read.
///
/// Increment on every incompatible on-disk change. Replay refuses
/// `trace.format_version > MAX_SUPPORTED_FORMAT_VERSION` with a
/// clear error rather than silently misinterpreting newer data.
pub const MAX_SUPPORTED_FORMAT_VERSION: FormatVersion = FormatVersion(2);

/// Wire-format version newtype. Monotonic, never reused.
///
/// rkyv-archived because it appears in segment + checkpoint headers
/// (binary). The manifest carries the version as a decimal text key
/// for human-readability.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct FormatVersion(pub u32);

impl FormatVersion {
    /// First public version. Anything older is pre-history.
    pub const V1: Self = Self(1);

    /// V2 — adds `Manifest::initial_fds` (sorted list of fd numbers
    /// open in the recorded child at exec time). Older readers
    /// missing this field would silently drop fd-table fidelity;
    /// the version bump makes that detection explicit.
    pub const V2: Self = Self(2);

    /// True iff this build can decode `self`.
    #[inline]
    pub const fn is_supported(self) -> bool {
        self.0 <= MAX_SUPPORTED_FORMAT_VERSION.0
    }
}

impl fmt::Debug for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FormatVersion(v{})", self.0)
    }
}

impl fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_eight_ascii_bytes() {
        assert_eq!(TRACE_MAGIC.len(), 8);
        assert!(TRACE_MAGIC.iter().all(|b| b.is_ascii()));
        assert_eq!(TRACE_MAGIC, b"BSREPLAY");
    }

    #[test]
    fn v1_and_v2_are_supported_v_max_plus_one_is_not() {
        assert!(FormatVersion::V1.is_supported());
        assert!(FormatVersion::V2.is_supported());
        assert!(MAX_SUPPORTED_FORMAT_VERSION.is_supported());
        let unsupported = FormatVersion(MAX_SUPPORTED_FORMAT_VERSION.0 + 1);
        assert!(!unsupported.is_supported());
    }

    #[test]
    fn v2_is_strictly_greater_than_v1() {
        assert!(FormatVersion::V2 > FormatVersion::V1);
    }

    #[test]
    fn ordering_is_monotonic() {
        assert!(FormatVersion(1) < FormatVersion(2));
        assert!(FormatVersion(2) > FormatVersion(1));
    }

    #[test]
    fn rkyv_roundtrip() {
        let v = FormatVersion::V1;
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&v).unwrap();
        let back = rkyv::from_bytes::<FormatVersion, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(v, back);
    }
}

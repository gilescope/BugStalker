// SPDX-License-Identifier: MIT
//! Compose Tier 2 checkpoint payloads.
//!
//! Wraps the layout snapshotter ([`crate::linux::proc_maps`]) and
//! the byte reader ([`crate::linux::proc_mem`]) into a single
//! "snapshot the writable state of a SIGSTOP'd child" call. The
//! resulting [`WritableState`] is encoded into a `Vec<u8>` payload
//! suitable for stuffing into a Tier 3 trace-internal
//! `format::Checkpoint.payload`.
//!
//! Read-only regions (text segments, file-backed `r-xp` mappings,
//! shared libraries) are deliberately skipped: replay can re-map
//! them from the same backing on disk. We only need to carry the
//! bytes of regions that may have diverged — heap, stack, and
//! private writable VMAs.
//!
//! Encoding (little-endian throughout, hand-rolled to avoid an
//! rkyv dep on this crate):
//!
//! ```text
//! u64        region count N
//! repeat N:
//!     u64    start virtual address
//!     u64    region length L
//!     u8 × L bytes
//! ```

use nix::unistd::Pid;

use super::proc_maps::{read_proc_maps, MemoryRegion, ProcMapsError};
use super::proc_mem::{read_region, write_bytes_at, ProcMemError};

/// One captured writable region: its start address + raw bytes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CapturedRegion {
    /// Virtual start address of the region in the captured process.
    pub start: u64,
    /// Bytes — `(end - start)` long.
    pub bytes: Vec<u8>,
}

/// Snapshot of every writable VMA's bytes at capture time.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WritableState {
    /// Captured regions in the order `/proc/<pid>/maps` reported them.
    pub regions: Vec<CapturedRegion>,
}

/// Errors arising from capture or payload codec.
#[derive(thiserror::Error, Debug)]
pub enum CaptureError {
    /// `/proc/<pid>/maps` failed.
    #[error("proc_maps: {0}")]
    Maps(#[from] ProcMapsError),
    /// `/proc/<pid>/mem` failed.
    #[error("proc_mem: {0}")]
    Mem(#[from] ProcMemError),
}

/// Errors arising from `from_payload`.
#[derive(thiserror::Error, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// Buffer ended before all expected bytes had been read.
    #[error("truncated payload: needed {needed} bytes, had {have}")]
    Truncated {
        /// Bytes still required to satisfy the field length.
        needed: usize,
        /// Bytes actually remaining when the read tried.
        have: usize,
    },
    /// A region's claimed length exceeded the remaining payload.
    #[error("region {index}: length {claimed} exceeds remaining {remaining}")]
    RegionLengthMismatch {
        /// 0-based region index.
        index: usize,
        /// Length the header claimed.
        claimed: u64,
        /// Bytes left when the region tried to read.
        remaining: usize,
    },
}

/// Snapshot every writable, *private* region of the target
/// process. The target must already be ptrace-attached (SEIZE'd
/// or ATTACH'd) so `/proc/<pid>/mem` is openable.
///
/// Regions that fail the byte read are silently skipped. Some
/// kernel-special mappings (e.g. `[vsyscall]` on hardened kernels,
/// or `[uprobes]`) are present in `/proc/<pid>/maps` but reject
/// reads via `/proc/<pid>/mem` with EIO; treating those as
/// "skip" rather than "fail" keeps capture robust against the
/// long tail of weird mappings.
pub fn capture_writable_state(pid: Pid) -> Result<WritableState, CaptureError> {
    let maps = read_proc_maps(pid)?;
    let mut regions = Vec::new();
    for r in &maps {
        if !is_capturable(r) {
            continue;
        }
        match read_region(pid, r) {
            Ok(bytes) => regions.push(CapturedRegion { start: r.start, bytes }),
            Err(_) => {
                // Skip — kernel-special VMA or transient
                // race; replay-time restore will not need this
                // region anyway.
                tracing::debug!(
                    target: "bs_replay",
                    "skipping unreadable region 0x{:x}..0x{:x}",
                    r.start, r.end,
                );
            }
        }
    }
    Ok(WritableState { regions })
}

/// Restore the captured writable state into `pid`'s address
/// space. Each region's bytes are pwrite64'd at its captured
/// start address.
///
/// Returns `(written, skipped)` — count of regions written
/// successfully and count that failed individually. A region
/// failure is *not* fatal because some regions in a capture may
/// no longer exist in the target (e.g. heap shrunk between
/// capture and restore) — the caller decides whether to treat
/// the skip count as a hard failure.
///
/// The caller must already have ptrace-attached the target.
pub fn restore_writable_state(
    pid: Pid,
    state: &WritableState,
) -> Result<RestoreReport, ProcMemError> {
    let mut written = 0usize;
    let mut skipped = 0usize;
    for r in &state.regions {
        match write_bytes_at(pid, r.start, &r.bytes) {
            Ok(()) => written += 1,
            Err(_) => {
                tracing::debug!(
                    target: "bs_replay",
                    "restore: skipping region 0x{:x} ({} bytes)",
                    r.start, r.bytes.len(),
                );
                skipped += 1;
            }
        }
    }
    Ok(RestoreReport { written, skipped })
}

/// Result of [`restore_writable_state`]: how many regions were
/// successfully restored vs. silently skipped.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RestoreReport {
    /// Regions whose bytes were written into the target.
    pub written: usize,
    /// Regions that failed to write (e.g. unmapped at restore
    /// time). Caller decides whether this is fatal.
    pub skipped: usize,
}

/// True iff the region should be carried in the checkpoint
/// payload. Filter rules:
///
/// - Must be writable (no point capturing read-only).
/// - Must be private (shared regions are by definition shared
///   with the parent kernel object; the child can re-map from
///   that source at replay).
/// - Skip the kernel pseudo-mappings — `[vvar]`, `[vdso]`,
///   `[vsyscall]` — which are kernel-managed and re-set up by
///   the kernel on replay-process start anyway.
fn is_capturable(r: &MemoryRegion) -> bool {
    if !r.perms.write || !r.perms.private {
        return false;
    }
    if let Some(name) = &r.pathname {
        if matches!(name.as_str(), "[vvar]" | "[vdso]" | "[vsyscall]") {
            return false;
        }
    }
    true
}

/// Encode a [`WritableState`] as the payload format documented at
/// the top of this module.
pub fn to_payload(state: &WritableState) -> Vec<u8> {
    let total_bytes = state
        .regions
        .iter()
        .map(|r| 16 + r.bytes.len()) // 8 start + 8 len + bytes
        .sum::<usize>();
    let mut out = Vec::with_capacity(8 + total_bytes);
    out.extend_from_slice(&(state.regions.len() as u64).to_le_bytes());
    for r in &state.regions {
        out.extend_from_slice(&r.start.to_le_bytes());
        out.extend_from_slice(&(r.bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(&r.bytes);
    }
    out
}

/// Decode a payload produced by [`to_payload`].
pub fn from_payload(bytes: &[u8]) -> Result<WritableState, DecodeError> {
    let mut cur = bytes;
    let count = read_u64(&mut cur)? as usize;
    let mut regions = Vec::with_capacity(count);
    for index in 0..count {
        let start = read_u64(&mut cur)?;
        let len = read_u64(&mut cur)?;
        if (len as usize) > cur.len() {
            return Err(DecodeError::RegionLengthMismatch {
                index,
                claimed: len,
                remaining: cur.len(),
            });
        }
        let (head, tail) = cur.split_at(len as usize);
        regions.push(CapturedRegion {
            start,
            bytes: head.to_vec(),
        });
        cur = tail;
    }
    Ok(WritableState { regions })
}

fn read_u64(cur: &mut &[u8]) -> Result<u64, DecodeError> {
    if cur.len() < 8 {
        return Err(DecodeError::Truncated { needed: 8, have: cur.len() });
    }
    let (head, tail) = cur.split_at(8);
    let v = u64::from_le_bytes(head.try_into().expect("split_at gives 8"));
    *cur = tail;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fork_self::LinuxForkSelfMechanism;
    use crate::ring::CheckpointMechanism;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn payload_roundtrip_preserves_every_byte() {
        let state = WritableState {
            regions: vec![
                CapturedRegion { start: 0x1000, bytes: vec![0xab; 16] },
                CapturedRegion { start: 0x2000, bytes: vec![0xcd, 0xef] },
                CapturedRegion { start: 0x3000, bytes: Vec::new() },
            ],
        };
        let p = to_payload(&state);
        let back = from_payload(&p).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn payload_empty_state_roundtrips() {
        let state = WritableState::default();
        let p = to_payload(&state);
        let back = from_payload(&p).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn from_payload_rejects_truncated_count() {
        // < 8 bytes can't even encode the count.
        for n in 0..8 {
            let buf = vec![0u8; n];
            assert!(matches!(
                from_payload(&buf),
                Err(DecodeError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn from_payload_rejects_oversized_region_length() {
        // count = 1, region claims 999 bytes but only 4 follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes()); // count
        buf.extend_from_slice(&0x1000u64.to_le_bytes()); // start
        buf.extend_from_slice(&999u64.to_le_bytes()); // claimed length
        buf.extend_from_slice(&[0xab; 4]); // only 4 bytes provided
        assert!(matches!(
            from_payload(&buf),
            Err(DecodeError::RegionLengthMismatch { index: 0, .. })
        ));
    }

    #[test]
    fn restore_writes_each_region_back() {
        // Build a synthetic WritableState whose one region targets
        // an actual heap-allocated buffer in the parent. After
        // fork+SEIZE+restore, the child's view of that buffer must
        // contain the pattern from the synthetic state.
        let buf: Vec<u8> = vec![0xff; 64];
        let addr = buf.as_ptr() as u64;
        let pattern: Vec<u8> = (0..32u8).collect(); // 0..32 byte progression

        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                eprintln!("skipping restore test: YAMA blocked seize");
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }

        let state = WritableState {
            regions: vec![CapturedRegion {
                start: addr,
                bytes: pattern.clone(),
            }],
        };
        let report = match restore_writable_state(h.pid, &state) {
            Ok(r) => r,
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EACCES") || s.contains("EPERM") {
                    eprintln!("skipping restore test: {e:?}");
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("restore failed: {e:?}");
            }
        };
        assert_eq!(report.written, 1);
        assert_eq!(report.skipped, 0);

        // Read back from the child to confirm the bytes landed.
        let read_back = super::super::proc_mem::read_bytes_at(h.pid, addr, pattern.len())
            .expect("read failed");
        assert_eq!(read_back, pattern, "restore did not land the bytes");

        // Parent's heap is unchanged thanks to COW.
        assert_eq!(buf, vec![0xff; 64], "parent buffer modified — COW broke");

        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn capture_on_seized_child_returns_non_empty_state() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                eprintln!("skipping capture test: YAMA blocked seize");
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }
        let state = match capture_writable_state(h.pid) {
            Ok(s) => s,
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EACCES") || s.contains("EPERM") {
                    eprintln!("skipping capture test: {e:?}");
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("capture failed: {e:?}");
            }
        };
        assert!(
            !state.regions.is_empty(),
            "expected at least one writable region in the child",
        );
        // Sanity: no captured region is empty.
        for (i, r) in state.regions.iter().enumerate() {
            assert!(
                !r.bytes.is_empty(),
                "captured region {i} at 0x{:x} is empty",
                r.start,
            );
        }
        // Roundtrip the captured payload to prove it serialises cleanly.
        let payload = to_payload(&state);
        let back = from_payload(&payload).expect("roundtrip failed");
        assert_eq!(state, back);

        mech.kill(h).expect("kill failed");
    }
}

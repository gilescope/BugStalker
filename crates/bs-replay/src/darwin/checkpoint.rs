// SPDX-License-Identifier: MIT
//! Mach-based checkpoint primitives for Darwin (Tier 2).
//!
//! Apple does not expose `fork(2)`-with-ptrace cleanly. The
//! plan's fallback is to enumerate writable VM regions via
//! `mach_vm_region_recurse`, snapshot each into a heap buffer
//! via `mach_vm_read_overwrite`, and restore by writing them
//! back via `mach_vm_write`. Heavier than Linux COW forks but
//! fully functional on macOS arm64 and x86-64.
//!
//! `task_for_pid` is the gate. Modern macOS denies it without
//! `com.apple.security.cs.debugger` (codesigned) or
//! root/`taskgated` consent. The existing BugStalker test
//! infrastructure codesigns the test binaries; the same path
//! applies here.
//!
//! ## What this module lands
//!
//! - [`CapturedRegion`] / [`WritableState`] — same shape as
//!   the Linux module so trace consumers can treat the payload
//!   identically.
//! - [`to_payload`] / [`from_payload`] — wire-format codec,
//!   same on-disk shape as the Linux side (the format crate
//!   stamps `kernel_release` in the manifest so a trace knows
//!   which platform produced its checkpoints).
//! - [`enumerate_writable_regions`] — pure-Rust walker over
//!   `mach_vm_region_recurse`. Returns `(addr, size)` pairs
//!   with VM_PROT_WRITE set.
//! - [`task_port_for_self`] — `mach_task_self()`. No
//!   entitlement needed.
//! - [`task_port_for_pid`] — `task_for_pid` against another
//!   process. May fail with `KERN_FAILURE` (-1) or
//!   `KERN_INVALID_ARGUMENT` (4) when the host denies the
//!   request — caller surfaces the error verbatim.
//! - [`capture_writable_state`] — composes the above.
//! - [`restore_writable_state`] — inverse via `mach_vm_write`.

#![cfg(target_os = "macos")]

use std::io;

use mach2::kern_return::{KERN_INVALID_ADDRESS, KERN_SUCCESS};
use mach2::mach_types::task_t;
use mach2::message::mach_msg_type_number_t;
use mach2::traps::{mach_task_self, task_for_pid};
use mach2::vm::{mach_vm_read_overwrite, mach_vm_region_recurse, mach_vm_write};
use mach2::vm_prot::VM_PROT_WRITE;
use mach2::vm_region::{
    SM_PRIVATE, SM_SHARED, SM_TRUESHARED, VM_REGION_SUBMAP_INFO_COUNT, vm_region_submap_info_64,
};
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t, natural_t};

/// One captured writable region: its start address + raw bytes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CapturedRegion {
    /// Virtual start address of the region in the captured task.
    pub start: u64,
    /// Bytes — `(end - start)` long.
    pub bytes: Vec<u8>,
}

/// Snapshot of every writable region's bytes at capture time.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WritableState {
    /// Captured regions in the order Mach reported them.
    pub regions: Vec<CapturedRegion>,
}

// ---------------------------------------------------------------------------
// Wire format (matches Linux side byte-for-byte)
// ---------------------------------------------------------------------------

/// Encode a [`WritableState`] as the same payload format the
/// Linux side uses:
///
/// ```text
/// u64        region count N
/// repeat N:
///     u64    start virtual address
///     u64    region length L
///     u8 × L bytes
/// ```
pub fn to_payload(state: &WritableState) -> Vec<u8> {
    let total_bytes = state
        .regions
        .iter()
        .map(|r| 16 + r.bytes.len())
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

/// Errors arising from [`from_payload`].
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
        return Err(DecodeError::Truncated {
            needed: 8,
            have: cur.len(),
        });
    }
    let (head, tail) = cur.split_at(8);
    let v = u64::from_le_bytes(head.try_into().expect("split_at gives 8"));
    *cur = tail;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Mach helpers
// ---------------------------------------------------------------------------

/// Get a task port for the current task. Always succeeds — no
/// entitlement required.
pub fn task_port_for_self() -> task_t {
    // SAFETY: mach_task_self() returns the calling task's send
    // right; safe to call from any context.
    unsafe { mach_task_self() }
}

/// Get a task port for another process by pid. Requires the
/// `com.apple.security.cs.debugger` entitlement on a codesigned
/// binary, or root.
pub fn task_port_for_pid(pid: i32) -> Result<task_t, MachError> {
    let mut port: task_t = 0;
    // SAFETY: task_for_pid writes the resulting send right
    // through `&mut port`; we own the local.
    let kr = unsafe { task_for_pid(mach_task_self(), pid, &mut port) };
    if kr != KERN_SUCCESS {
        return Err(MachError::TaskForPid {
            kern_return: kr,
            pid,
        });
    }
    Ok(port)
}

/// Errors from the Mach-side Tier 2 path.
#[derive(thiserror::Error, Debug)]
pub enum MachError {
    /// `task_for_pid` returned non-zero. On modern macOS this
    /// usually means the supervisor isn't codesigned with the
    /// debugger entitlement.
    #[error(
        "task_for_pid({pid}) returned kern_return={kern_return}; \
         verify the supervisor is codesigned with \
         com.apple.security.cs.debugger or run as root"
    )]
    TaskForPid {
        /// kern_return code from the Mach call.
        kern_return: i32,
        /// PID we tried to attach to.
        pid: i32,
    },
    /// `mach_vm_region_recurse` returned non-zero.
    #[error("mach_vm_region_recurse: kern_return={kern_return}")]
    Region {
        /// kern_return code.
        kern_return: i32,
    },
    /// `mach_vm_read_overwrite` returned non-zero.
    #[error("mach_vm_read_overwrite at {addr:#x} ({size} B): kern_return={kern_return}")]
    Read {
        /// Address that failed.
        addr: u64,
        /// Size requested.
        size: u64,
        /// kern_return code.
        kern_return: i32,
    },
    /// `mach_vm_write` returned non-zero.
    #[error("mach_vm_write at {addr:#x} ({size} B): kern_return={kern_return}")]
    Write {
        /// Address that failed.
        addr: u64,
        /// Size requested.
        size: u64,
        /// kern_return code.
        kern_return: i32,
    },
    /// Underlying I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One mapping the Mach VM walker reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachRegion {
    /// Virtual start address.
    pub start: u64,
    /// Region size in bytes.
    pub size: u64,
    /// VM_PROT_* mask.
    pub prot: u32,
    /// True if the region is private (not shared with another
    /// task). Captured so the caller skips shared mappings the
    /// way the Linux side skips `[vdso]` etc.
    pub private: bool,
    /// True iff the region is *actively shared* with another
    /// task (`SM_TRUESHARED` or `SM_SHARED_ALIASED`). COW/private
    /// regions return false here even though their pages may be
    /// physically shared until the first write — restore is
    /// always safe in that case.
    pub shared_with_other_tasks: bool,
}

impl MachRegion {
    /// True iff the region should be carried in the checkpoint
    /// payload — writable, not actively shared with another
    /// task. The Linux side filters on `private` strictly; on
    /// Darwin we treat `SM_COW` and `SM_PRIVATE` (and the
    /// aliased variants) as capturable, leaving `SM_TRUESHARED`
    /// / `SM_SHARED_ALIASED` for the explicit shared-memory
    /// case the recorder doesn't yet handle.
    pub fn is_capturable(&self) -> bool {
        (self.prot & (VM_PROT_WRITE as u32)) != 0 && !self.shared_with_other_tasks
    }
}

/// Walk every VM region in `task`. Returns them in ascending
/// address order. Each region's `prot` is the *effective*
/// protection bits Mach reports; the `private` bit derives
/// from the submap info's `share_mode`.
pub fn enumerate_regions(task: task_t) -> Result<Vec<MachRegion>, MachError> {
    let mut out = Vec::new();
    let mut addr: mach_vm_address_t = 1;
    loop {
        let mut size: mach_vm_size_t = 0;
        let mut depth: natural_t = 0;
        let mut info: vm_region_submap_info_64 = unsafe { std::mem::zeroed() };
        let mut info_count: mach_msg_type_number_t = VM_REGION_SUBMAP_INFO_COUNT;
        // SAFETY: passing well-typed pointers; the kernel only
        // writes through them and only reads the depth count.
        let kr = unsafe {
            mach_vm_region_recurse(
                task,
                &mut addr,
                &mut size,
                &mut depth,
                &mut info as *mut _ as *mut i32,
                &mut info_count,
            )
        };
        if kr == KERN_INVALID_ADDRESS {
            // We walked off the end of the address space.
            break;
        }
        if kr != KERN_SUCCESS {
            return Err(MachError::Region { kern_return: kr });
        }
        let prot = info.protection as u32;
        let private = info.share_mode == SM_PRIVATE;
        let shared_with_other_tasks =
            info.share_mode == SM_SHARED || info.share_mode == SM_TRUESHARED;
        out.push(MachRegion {
            start: addr,
            size,
            prot,
            private,
            shared_with_other_tasks,
        });
        addr = addr.saturating_add(size);
        if addr == u64::MAX {
            break;
        }
    }
    Ok(out)
}

/// Filter [`enumerate_regions`] to just the capturable ones.
pub fn enumerate_writable_regions(task: task_t) -> Result<Vec<MachRegion>, MachError> {
    Ok(enumerate_regions(task)?
        .into_iter()
        .filter(MachRegion::is_capturable)
        .collect())
}

/// Read `size` bytes from `task`'s address space starting at
/// `addr`. Wraps `mach_vm_read_overwrite`.
pub fn read_region_bytes(task: task_t, addr: u64, size: u64) -> Result<Vec<u8>, MachError> {
    let mut buf = vec![0u8; size as usize];
    let mut got: mach_vm_size_t = 0;
    // SAFETY: read_overwrite writes through buf.as_mut_ptr()
    // for `size` bytes; the buffer is exactly that size.
    let kr = unsafe {
        mach_vm_read_overwrite(
            task,
            addr,
            size,
            buf.as_mut_ptr() as mach_vm_address_t,
            &mut got,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(MachError::Read {
            addr,
            size,
            kern_return: kr,
        });
    }
    buf.truncate(got as usize);
    Ok(buf)
}

/// Write `bytes` into `task`'s address space at `addr`. Wraps
/// `mach_vm_write`. The region must already exist and be
/// writable; the caller mprotects/vm_protects beforehand if
/// the region was r-x.
pub fn write_region_bytes(task: task_t, addr: u64, bytes: &[u8]) -> Result<(), MachError> {
    // SAFETY: bytes' lifetime exceeds the syscall.
    let kr = unsafe {
        mach_vm_write(
            task,
            addr,
            bytes.as_ptr() as usize,
            bytes.len() as mach_msg_type_number_t,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(MachError::Write {
            addr,
            size: bytes.len() as u64,
            kern_return: kr,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Capture / restore
// ---------------------------------------------------------------------------

/// Snapshot every writable, private region of the target
/// process. Requires `task_for_pid` to succeed (codesigned
/// binary with the debugger entitlement, or root).
pub fn capture_writable_state(pid: i32) -> Result<WritableState, MachError> {
    let task = task_port_for_pid(pid)?;
    capture_for_task(task)
}

/// Inner half of [`capture_writable_state`] — works against
/// an already-acquired task port. Useful in tests where the
/// supervisor calls [`task_port_for_self`].
pub fn capture_for_task(task: task_t) -> Result<WritableState, MachError> {
    let regions = enumerate_writable_regions(task)?;
    let mut captured = Vec::with_capacity(regions.len());
    for r in regions {
        // Skip empty/zero-size regions defensively.
        if r.size == 0 {
            continue;
        }
        match read_region_bytes(task, r.start, r.size) {
            Ok(bytes) => captured.push(CapturedRegion {
                start: r.start,
                bytes,
            }),
            Err(e) => {
                // Some kernel-special regions reject reads;
                // log and skip. Matches the Linux fallback.
                tracing::debug!(
                    target: "bs_replay",
                    "skipping unreadable Mach region {:#x} ({} B): {e}",
                    r.start, r.size,
                );
            }
        }
    }
    Ok(WritableState { regions: captured })
}

/// Restore the captured writable state into `pid`'s address
/// space. Per-region failures are surfaced via
/// [`RestoreReport`]; the function only returns `Err` for
/// task_for_pid-class failures.
pub fn restore_writable_state(pid: i32, state: &WritableState) -> Result<RestoreReport, MachError> {
    let task = task_port_for_pid(pid)?;
    Ok(restore_for_task(task, state))
}

/// Inner half of [`restore_writable_state`] — already-acquired
/// task port.
pub fn restore_for_task(task: task_t, state: &WritableState) -> RestoreReport {
    let mut written = 0usize;
    let mut skipped = 0usize;
    for r in &state.regions {
        match write_region_bytes(task, r.start, &r.bytes) {
            Ok(()) => written += 1,
            Err(_) => {
                tracing::debug!(
                    target: "bs_replay",
                    "restore: skipping Mach region {:#x} ({} bytes)",
                    r.start, r.bytes.len(),
                );
                skipped += 1;
            }
        }
    }
    RestoreReport { written, skipped }
}

/// Result of [`restore_writable_state`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RestoreReport {
    /// Regions whose bytes were written into the target.
    pub written: usize,
    /// Regions that failed to write. Caller decides whether
    /// this is fatal.
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip_preserves_every_byte() {
        let state = WritableState {
            regions: vec![
                CapturedRegion {
                    start: 0x1_0000,
                    bytes: vec![0xab; 16],
                },
                CapturedRegion {
                    start: 0x2_0000,
                    bytes: vec![0xcd, 0xef],
                },
                CapturedRegion {
                    start: 0x3_0000,
                    bytes: Vec::new(),
                },
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
        assert_eq!(p, 0u64.to_le_bytes());
        let back = from_payload(&p).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn from_payload_rejects_truncated_count() {
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
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&0x1000u64.to_le_bytes());
        buf.extend_from_slice(&999u64.to_le_bytes());
        buf.extend_from_slice(&[0xab; 4]);
        assert!(matches!(
            from_payload(&buf),
            Err(DecodeError::RegionLengthMismatch { index: 0, .. })
        ));
    }

    #[test]
    fn enumerate_regions_for_self_yields_the_address_space() {
        let task = task_port_for_self();
        let regions = enumerate_regions(task).expect("enumerate");
        assert!(
            !regions.is_empty(),
            "process must have at least one VM region"
        );
        // Every region's size should be positive; addresses
        // strictly increase by `size`.
        let mut prev_end: u64 = 0;
        for r in &regions {
            assert!(r.size > 0, "zero-size region in enumeration");
            assert!(
                r.start >= prev_end,
                "regions overlap or wrap: prev_end={:#x}, region_start={:#x}",
                prev_end,
                r.start,
            );
            prev_end = r.start + r.size;
        }
    }

    #[test]
    fn enumerate_writable_regions_filters_to_capturable() {
        let task = task_port_for_self();
        let writable = enumerate_writable_regions(task).expect("enumerate writable");
        // Stack + heap + .data are all writable.
        assert!(
            !writable.is_empty(),
            "expected at least one writable+capturable region",
        );
        for r in &writable {
            assert!((r.prot & (VM_PROT_WRITE as u32)) != 0);
            assert!(
                !r.shared_with_other_tasks,
                "writable filter let through actively-shared region {:#x}",
                r.start,
            );
        }
    }

    #[test]
    fn read_region_bytes_round_trips_self_owned_buffer() {
        // Allocate a known buffer in our own heap, then ask
        // Mach to read it back. Round-tripping through the VM
        // layer proves the read API is wired correctly without
        // needing a separate codesigned task.
        let buf: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let addr = buf.as_ptr() as u64;
        let task = task_port_for_self();
        let read = read_region_bytes(task, addr, buf.len() as u64).expect("read");
        assert_eq!(read, buf);
    }

    #[test]
    fn capture_for_task_self_returns_non_empty_state() {
        let task = task_port_for_self();
        let state = capture_for_task(task).expect("capture");
        assert!(
            !state.regions.is_empty(),
            "self-capture should find at least one writable region",
        );
        // Ensure the payload codec round-trips.
        let p = to_payload(&state);
        let back = from_payload(&p).expect("decode");
        assert_eq!(state.regions.len(), back.regions.len());
    }

    #[test]
    fn task_port_for_pid_other_pid_diagnoses_cleanly() {
        // Try task_for_pid against pid 1 (launchd); on a
        // non-codesigned non-root test runner this denies and
        // we get a clear MachError::TaskForPid back.
        let r = task_port_for_pid(1);
        match r {
            Ok(_port) => {
                // Surprising — we may have run with the entitlement.
                eprintln!("task_for_pid(1) succeeded; supervisor is codesigned/root");
            }
            Err(MachError::TaskForPid {
                kern_return,
                pid: 1,
            }) => {
                assert!(
                    kern_return != KERN_SUCCESS,
                    "TaskForPid error must carry a non-zero kern_return",
                );
            }
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn mach_region_capturable_predicate() {
        let r_writable_private = MachRegion {
            start: 0x1000,
            size: 0x1000,
            prot: (VM_PROT_WRITE | mach2::vm_prot::VM_PROT_READ) as u32,
            private: true,
            shared_with_other_tasks: false,
        };
        assert!(r_writable_private.is_capturable());

        let r_readonly = MachRegion {
            prot: mach2::vm_prot::VM_PROT_READ as u32,
            ..r_writable_private
        };
        assert!(!r_readonly.is_capturable());

        let r_shared = MachRegion {
            shared_with_other_tasks: true,
            ..r_writable_private
        };
        assert!(!r_shared.is_capturable());
    }
}

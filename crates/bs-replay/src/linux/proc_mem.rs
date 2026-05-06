// SPDX-License-Identifier: MIT
//! Read bytes from `/proc/<pid>/mem`.
//!
//! Lets a SEIZE'd checkpoint capture the actual contents of
//! writable memory regions. `/proc/<pid>/mem` exposes the target
//! process's virtual address space as a seekable file: a `pread64`
//! at offset N returns the byte at virtual address N. The caller
//! must already have ptrace-attached the target (PTRACE_SEIZE
//! or PTRACE_ATTACH); the kernel rejects opens otherwise.
//!
//! Two-tier API:
//!
//! - [`read_bytes_at`] for spot reads (e.g. inspecting a variable
//!   at a known PC at trap time).
//! - [`read_region`] for full-region capture into a checkpoint
//!   payload.

use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;

use nix::unistd::Pid;

use super::proc_maps::MemoryRegion;

/// Read `len` bytes from `pid`'s virtual address `address`.
pub fn read_bytes_at(
    pid: Pid,
    address: u64,
    len: usize,
) -> Result<Vec<u8>, ProcMemError> {
    let path = format!("/proc/{}/mem", pid.as_raw());
    let f = OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| ProcMemError::Open { pid: pid.as_raw(), source: e })?;
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, address).map_err(|e| ProcMemError::Read {
        pid: pid.as_raw(),
        address,
        len,
        source: e,
    })?;
    Ok(buf)
}

/// Read every byte of the given memory region.
pub fn read_region(
    pid: Pid,
    region: &MemoryRegion,
) -> Result<Vec<u8>, ProcMemError> {
    let len = region.end.saturating_sub(region.start) as usize;
    read_bytes_at(pid, region.start, len)
}

/// Write `bytes` into `pid`'s virtual address space at `address`.
/// Symmetric to [`read_bytes_at`]. The caller must already have
/// ptrace-attached the target so the kernel allows the write.
pub fn write_bytes_at(
    pid: Pid,
    address: u64,
    bytes: &[u8],
) -> Result<(), ProcMemError> {
    let path = format!("/proc/{}/mem", pid.as_raw());
    let f = OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| ProcMemError::Open { pid: pid.as_raw(), source: e })?;
    f.write_all_at(bytes, address)
        .map_err(|e| ProcMemError::Read {
            pid: pid.as_raw(),
            address,
            len: bytes.len(),
            source: e,
        })?;
    Ok(())
}

/// Errors arising from `/proc/<pid>/mem` reads.
#[derive(thiserror::Error, Debug)]
pub enum ProcMemError {
    /// Couldn't open `/proc/<pid>/mem` (typically EACCES if not
    /// ptrace-attached, or ESRCH if the pid is gone).
    #[error("/proc/{pid}/mem open: {source}")]
    Open {
        /// PID we tried to open.
        pid: i32,
        /// Underlying io::Error.
        source: std::io::Error,
    },
    /// Read failed at the given address (typically EIO if the
    /// region isn't mapped or is paged out without backing).
    #[error("/proc/{pid}/mem read at 0x{address:x} ({len} bytes): {source}")]
    Read {
        /// PID we read.
        pid: i32,
        /// Virtual address that failed.
        address: u64,
        /// Number of bytes requested.
        len: usize,
        /// Underlying io::Error.
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fork_self::LinuxForkSelfMechanism;
    use crate::linux::proc_maps::read_proc_maps;
    use crate::ring::CheckpointMechanism;
    use std::thread::sleep;
    use std::time::Duration;

    /// Static byte pattern to read out of the child's memory.
    /// Lives in the parent's `.rodata`; after fork the child's
    /// frozen address space has the same bytes at the same VA.
    static MAGIC: [u8; 16] = [
        0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe,
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
    ];

    fn skip_if_yama(e: &ProcMemError) -> bool {
        let s = format!("{e:?}");
        // EACCES typically means YAMA blocked the trace. A clean
        // trace + open would always succeed.
        s.contains("EACCES") || s.contains("EPERM")
    }

    #[test]
    fn read_bytes_at_recovers_magic_from_seized_child() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                eprintln!("skipping read_bytes_at test: YAMA blocked seize ({e:?})");
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }

        let addr = MAGIC.as_ptr() as u64;
        let bytes = match read_bytes_at(h.pid, addr, MAGIC.len()) {
            Ok(b) => b,
            Err(e) if skip_if_yama(&e) => {
                eprintln!("skipping read_bytes_at test: {e:?}");
                mech.kill(h).expect("kill failed");
                return;
            }
            Err(e) => panic!("read_bytes_at failed: {e:?}"),
        };
        assert_eq!(
            bytes, MAGIC,
            "read at 0x{addr:x} returned {bytes:02x?}; expected {MAGIC:02x?}",
        );
        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn read_region_captures_a_full_vma_including_magic() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                eprintln!("skipping read_region test: YAMA blocked seize");
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }

        let maps = read_proc_maps(h.pid).expect("proc_maps failed");
        let magic_addr = MAGIC.as_ptr() as u64;
        let region = maps
            .iter()
            .find(|r| magic_addr >= r.start && magic_addr < r.end)
            .expect("no region contains MAGIC")
            .clone();
        // Skip if the region is huge — reading multiple MB to find
        // 16 bytes is wasteful. In practice MAGIC's .rodata page
        // is small (often one 4 KB page), but if mapped as part
        // of the binary's whole text segment it could be large.
        if region.end - region.start > 16 * 1024 * 1024 {
            eprintln!(
                "skipping read_region test: region {:#x}..{:#x} is too large ({} bytes)",
                region.start, region.end, region.end - region.start,
            );
            mech.kill(h).expect("kill failed");
            return;
        }
        let bytes = match read_region(h.pid, &region) {
            Ok(b) => b,
            Err(e) if skip_if_yama(&e) => {
                eprintln!("skipping read_region test: {e:?}");
                mech.kill(h).expect("kill failed");
                return;
            }
            Err(e) => panic!("read_region failed: {e:?}"),
        };
        assert_eq!(
            bytes.len(),
            (region.end - region.start) as usize,
            "region byte count mismatch",
        );
        let offset = (magic_addr - region.start) as usize;
        assert_eq!(
            &bytes[offset..offset + MAGIC.len()],
            &MAGIC[..],
            "MAGIC not found at expected offset within region bytes",
        );
        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn write_then_read_roundtrips_in_seized_child() {
        // Heap-allocate a buffer; the address is shared (same VA)
        // with the post-fork child. write_bytes_at into the
        // child's copy and read it back to confirm the kernel
        // actually wrote the bytes there.
        let buf: Vec<u8> = vec![0u8; 64];
        let addr = buf.as_ptr() as u64;

        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                eprintln!("skipping write_then_read test: YAMA blocked seize");
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }

        let pattern = [0xab; 16];
        match write_bytes_at(h.pid, addr, &pattern) {
            Ok(()) => {}
            Err(e) if skip_if_yama(&e) => {
                eprintln!("skipping write test: {e:?}");
                mech.kill(h).expect("kill failed");
                return;
            }
            Err(e) => panic!("write_bytes_at failed: {e:?}"),
        };
        let read_back = read_bytes_at(h.pid, addr, pattern.len()).expect("read failed");
        assert_eq!(
            read_back, pattern,
            "write then read mismatch at 0x{addr:x}",
        );

        // Parent's view is unchanged — COW means the child's
        // write didn't propagate back.
        assert_eq!(
            buf, vec![0u8; 64],
            "parent's heap buffer was modified — fork should have COW'd",
        );

        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn read_bytes_at_unmapped_address_returns_error() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        if let Err(e) = mech.seize(&h) {
            let s = format!("{e:?}");
            if s.contains("EPERM") {
                mech.kill(h).expect("kill failed");
                return;
            }
            panic!("seize failed: {e:?}");
        }

        // 0x0 is conventionally never mapped.
        match read_bytes_at(h.pid, 0, 16) {
            Ok(_) => panic!("expected read at 0x0 to fail"),
            Err(_) => {} // either Open or Read variant — don't care which
        }
        mech.kill(h).expect("kill failed");
    }
}

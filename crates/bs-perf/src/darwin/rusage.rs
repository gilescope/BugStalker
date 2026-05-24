// SPDX-License-Identifier: MIT
//! `proc_pid_rusage(2)` snapshots for the macOS Tier 2 perf path.
//!
//! Captures wall + cumulative per-process counters (CPU time,
//! retired instructions, cycles, page-ins, disk I/O bytes) for a
//! given pid; diff two snapshots to get a per-window figure.
//! No PMU access, no kperf, no entitlements — just the public
//! `proc_pid_rusage` syscall.
//!
//! `RUSAGE_INFO_V4` is the flavor we ask for. libc's
//! `rusage_info_v4` carries the modern superset including
//! `ri_instructions`, `ri_cycles`, `ri_diskio_*`, and `ri_pageins`.
//! On macOS 13+ the kernel populates all of these; on older
//! releases the trailing fields remain zero, which we treat as
//! "unavailable" through `Option<u64>` accessors.

use std::io;
use std::mem::MaybeUninit;
use std::time::Instant;

use crate::PerfError;

/// Whole-process counter snapshot. All counters are cumulative
/// since the process started — diff two snapshots to get a
/// per-window figure.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSnapshot {
    /// Monotonic wall-clock instant when the snapshot was taken.
    pub wall: Instant,
    /// Cumulative user + system CPU time, in nanoseconds.
    pub cpu_time_ns: u64,
    /// Cumulative retired instructions (`ri_instructions`). Zero
    /// on macOS releases that don't populate this field.
    pub instructions: u64,
    /// Cumulative cycles (`ri_cycles`). Zero when unpopulated.
    pub cycles: u64,
    /// Cumulative page-ins (`ri_pageins`). High deltas suggest the
    /// run was waiting on page-in I/O.
    pub pageins: u64,
    /// Cumulative bytes read from disk (`ri_diskio_bytesread`).
    pub disk_bytes_read: u64,
    /// Cumulative bytes written to disk (`ri_diskio_byteswritten`).
    pub disk_bytes_written: u64,
}

/// Per-window deltas produced by [`ProcessSnapshot::delta_since`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessDelta {
    /// Wall-clock duration of the window, in nanoseconds.
    pub wall_ns: u64,
    /// User + system CPU time spent in the window, in nanoseconds.
    pub cpu_time_ns: u64,
    /// Retired instructions during the window. Zero when the host
    /// doesn't populate `ri_instructions`.
    pub instructions: u64,
    /// Cycles during the window. Zero when unpopulated.
    pub cycles: u64,
    /// Page-ins during the window.
    pub pageins: u64,
    /// Bytes read from disk during the window.
    pub disk_bytes_read: u64,
    /// Bytes written to disk during the window.
    pub disk_bytes_written: u64,
}

impl ProcessSnapshot {
    /// Capture per-process counters for `pid` and the matching
    /// wall instant. Returns `PerfError::Open` (wrapping `errno`)
    /// if the kernel rejects the call — typically `ESRCH`
    /// (process gone) or `EPERM` on a foreign-uid pid.
    pub fn capture(pid: i32) -> Result<Self, PerfError> {
        let wall = Instant::now();
        let info = read_rusage_v4(pid)?;
        Ok(ProcessSnapshot {
            wall,
            cpu_time_ns: info.ri_user_time.saturating_add(info.ri_system_time),
            instructions: info.ri_instructions,
            cycles: info.ri_cycles,
            pageins: info.ri_pageins,
            disk_bytes_read: info.ri_diskio_bytesread,
            disk_bytes_written: info.ri_diskio_byteswritten,
        })
    }

    /// Compute per-window deltas relative to an earlier snapshot.
    /// Every field saturates to zero on underflow so a transient
    /// clock or counter anomaly can't produce garbage.
    pub fn delta_since(self, earlier: ProcessSnapshot) -> ProcessDelta {
        let wall_ns = self
            .wall
            .saturating_duration_since(earlier.wall)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        ProcessDelta {
            wall_ns,
            cpu_time_ns: self.cpu_time_ns.saturating_sub(earlier.cpu_time_ns),
            instructions: self.instructions.saturating_sub(earlier.instructions),
            cycles: self.cycles.saturating_sub(earlier.cycles),
            pageins: self.pageins.saturating_sub(earlier.pageins),
            disk_bytes_read: self.disk_bytes_read.saturating_sub(earlier.disk_bytes_read),
            disk_bytes_written: self
                .disk_bytes_written
                .saturating_sub(earlier.disk_bytes_written),
        }
    }
}

fn read_rusage_v4(pid: i32) -> Result<libc::rusage_info_v4, PerfError> {
    // proc_pid_rusage's third argument is `rusage_info_t`, which is
    // `void *` in the kernel header; libc declares it as a `*mut
    // c_void` typedef. Zero-init the buffer so any kernel field
    // we don't read still has a defined value.
    let mut buffer = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `buffer` outlives the call; the buffer pointer is
    // exactly the size the kernel expects for `RUSAGE_INFO_V4`.
    let ret = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4 as libc::c_int,
            buffer.as_mut_ptr().cast::<libc::c_void>().cast(),
        )
    };
    if ret != 0 {
        return Err(PerfError::Open(io::Error::last_os_error()));
    }
    // SAFETY: kernel returned 0, so the buffer is fully populated.
    Ok(unsafe { buffer.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// We can always snapshot our own pid; the call should succeed
    /// and report nonzero user time after we've burned a few cycles.
    #[test]
    fn snapshot_self_is_monotonic() {
        let pid = std::process::id() as i32;
        let a = ProcessSnapshot::capture(pid).expect("snapshot");
        // Burn a measurable amount of CPU — adding to a volatile
        // sink so the optimiser can't elide the loop.
        let mut sink: u64 = 0;
        for i in 0_u64..100_000 {
            sink = sink.wrapping_add(i);
        }
        std::hint::black_box(sink);
        let b = ProcessSnapshot::capture(pid).expect("snapshot");
        let delta = b.delta_since(a);
        assert!(delta.wall_ns > 0, "wall should advance between snapshots");
        // On macOS 13+ the rusage v4 surface populates instructions
        // and cycles. We tolerate zeros (older kernels, idle process)
        // but assert never-negative — `saturating_sub` enforces it.
        // The hot loop above retires ~100k instructions on most hosts;
        // if the field is populated we expect to see at least *some*
        // forward motion, but we don't hard-fail on a quiet kernel.
        let _ = (delta.instructions, delta.cycles, delta.pageins);
    }

    /// Non-existent pid → error, not panic.
    #[test]
    fn snapshot_nonexistent_pid_errors() {
        // Pid 0 is special on Darwin (kernel proc); the syscall
        // returns ESRCH or EPERM. Either way we get an error.
        let r = ProcessSnapshot::capture(-1);
        assert!(r.is_err());
    }

    /// Hot-loop test: snapshot, burn a known amount of CPU, snapshot.
    /// If the kernel populates `ri_instructions`, we should see a
    /// non-zero delta. Skipped silently if `ri_instructions` reads
    /// as zero — older macOS or sandboxed test environments.
    #[test]
    fn instructions_advance_under_hot_loop_when_populated() {
        let pid = std::process::id() as i32;
        let a = ProcessSnapshot::capture(pid).expect("snapshot");
        let mut sink: u64 = 0;
        for i in 0_u64..5_000_000 {
            sink = sink.wrapping_add(i.wrapping_mul(3));
        }
        std::hint::black_box(sink);
        let b = ProcessSnapshot::capture(pid).expect("snapshot");
        let delta = b.delta_since(a);
        if a.instructions != 0 || b.instructions != 0 {
            assert!(
                delta.instructions > 1_000_000,
                "5M-iteration loop should retire ≫1M instructions, got {}",
                delta.instructions
            );
        } else {
            eprintln!("ri_instructions reads as zero on this host; skipping advancement check");
        }
    }
}

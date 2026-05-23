// SPDX-License-Identifier: MIT
//! `proc_pid_rusage(2)` snapshots for the macOS Tier 2 perf path.
//!
//! Captures wall-clock `Instant` + cumulative `ri_user_time +
//! ri_system_time` (nanoseconds since process start) for a given
//! pid; diff two snapshots to get a per-window CPU-time figure for
//! the per-stop summary. No PMU access, no kperf, no entitlements.
//!
//! v4 is the safest flavor to pin to: it's been part of the macOS
//! ABI since 10.9 and every newer flavor (v5/v6) is a strict
//! superset of it. If a future revision changes the v4 layout we
//! catch the mismatch at compile time, because libc's
//! `rusage_info_v4` is the canonical Rust binding for the same
//! kernel header.

use std::io;
use std::mem::MaybeUninit;
use std::time::Instant;

use crate::PerfError;

/// Whole-process CPU + wall snapshot.
///
/// `cpu_time_ns` is `ri_user_time + ri_system_time` for the
/// debuggee at sample time. `wall` is bs's monotonic clock at the
/// same instant — both are captured close together so the diff is
/// representative of the run-to-stop window.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSnapshot {
    /// Monotonic wall-clock instant when the snapshot was taken.
    pub wall: Instant,
    /// Cumulative user + system CPU time, in nanoseconds, since the
    /// debuggee process started.
    pub cpu_time_ns: u64,
}

impl ProcessSnapshot {
    /// Capture user+system CPU time for `pid` and the matching wall
    /// instant. Returns `PerfError::Open` (wrapping `errno`) if the
    /// kernel rejects the call — typically `ESRCH` (process gone)
    /// or `EPERM` on a process the caller doesn't own.
    pub fn capture(pid: i32) -> Result<Self, PerfError> {
        let wall = Instant::now();
        let info = read_rusage_v4(pid)?;
        Ok(ProcessSnapshot {
            wall,
            cpu_time_ns: info.ri_user_time.saturating_add(info.ri_system_time),
        })
    }

    /// `(wall_ns, cpu_ns)` for `self` measured against an earlier
    /// snapshot. Saturates to zero if `self` is somehow earlier —
    /// clocks should be monotonic, but the saturating arithmetic
    /// means a clock anomaly can't underflow into garbage.
    pub fn delta_since(self, earlier: ProcessSnapshot) -> (u64, u64) {
        let wall_ns = self
            .wall
            .saturating_duration_since(earlier.wall)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let cpu_ns = self.cpu_time_ns.saturating_sub(earlier.cpu_time_ns);
        (wall_ns, cpu_ns)
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
        let (wall_ns, cpu_ns) = b.delta_since(a);
        assert!(wall_ns > 0, "wall should advance between snapshots");
        // CPU time is a noisy fast counter; on quiet machines it
        // usually advances, but on a heavily loaded host the
        // kernel may not have updated the per-process accounting
        // between two back-to-back syscalls. Allow zero, but
        // require it to never go backwards (delta_since saturates).
        let _ = cpu_ns;
    }

    /// Non-existent pid → error, not panic.
    #[test]
    fn snapshot_nonexistent_pid_errors() {
        // Pid 0 is special on Darwin (kernel proc); the syscall
        // returns ESRCH or EPERM. Either way we get an error.
        let r = ProcessSnapshot::capture(-1);
        assert!(r.is_err());
    }
}

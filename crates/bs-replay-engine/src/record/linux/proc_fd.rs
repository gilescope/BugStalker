// SPDX-License-Identifier: MIT
//! `/proc/<pid>/fd` walker for fd-table snapshotting at exec time.
//!
//! Sub-phase 3C polish: the recorder captures the set of file
//! descriptors open in the recorded child immediately after
//! exec, and the replay-side uses that set (via
//! `crate::replay::linux::file_actions`) to mask out the
//! supervisor's accidental fd-table contamination.
//!
//! Numbers, not targets — we don't try to reproduce pty paths or
//! pipe inodes. The seccomp listener answers reads/writes from
//! the trace; we just need the fd-table topology to match so the
//! tracee can issue syscalls against the same fd numbers it issued
//! during recording.

#![cfg(target_os = "linux")]

use std::fs;
use std::io;

/// Read the open-fd set of `pid` from `/proc/<pid>/fd`. Returns
/// the fd numbers in ascending order; `dedup`-clean by virtue of
/// the directory's uniqueness contract.
///
/// Suitable for calling on a freshly-forked-and-execed tracee
/// while it sits in its first syscall stop — that's the moment
/// the fd-table reflects the post-exec state (close-on-exec
/// fds have already gone away; the supervisor's leaked fds —
/// if any — remain visible).
pub fn list_open_fds(pid: i32) -> io::Result<Vec<u32>> {
    let path = format!("/proc/{pid}/fd");
    let mut out = Vec::new();
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && let Ok(n) = name.parse::<u32>()
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading `/proc/self/fd` always succeeds on Linux (we have
    /// at least stdin/stdout/stderr). Acts as a smoke test for
    /// the read-and-parse path without needing a forked tracee.
    #[test]
    fn list_open_fds_for_self_is_non_empty_and_sorted() {
        let pid = unsafe { libc::getpid() };
        let fds = list_open_fds(pid).expect("read self fd-table");
        assert!(!fds.is_empty(), "/proc/self/fd had no entries");
        // Three standard fds are always present unless the test
        // harness closed them — extremely unusual, but we don't
        // pin the count, just sortedness + uniqueness.
        for w in fds.windows(2) {
            assert!(w[0] < w[1], "fds not strictly increasing: {fds:?}");
        }
    }

    #[test]
    fn list_open_fds_for_invalid_pid_errors_cleanly() {
        // PID 1 is init — readable but our process likely lacks
        // permission unless run as root. Use a sentinel that
        // can't be a real pid.
        let r = list_open_fds(i32::MAX);
        assert!(r.is_err(), "expected error for non-existent pid");
    }
}

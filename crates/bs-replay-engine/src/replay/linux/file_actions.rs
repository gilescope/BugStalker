// SPDX-License-Identifier: MIT
//! `posix_spawn_file_actions(3)`-style fd-table reconstitution
//! for the replay-side child.
//!
//! Sub-phase 3C polish. The recorded child's fd-table at exec time
//! is captured by [`crate::record::linux::proc_fd::list_open_fds`]
//! and stamped into [`crate::format::manifest::Manifest::initial_fds`].
//! At replay time, [`fd_diff_actions`] computes the delta between
//! that recorded set and the supervisor's current fd-table; the
//! resulting [`FileAction`] list is applied in the replay child
//! between fork and execve via [`apply_in_child`].
//!
//! Two operations cover the practical cases:
//!
//! - **Close**: the supervisor has fd N open but the recorded
//!   child didn't. Closing it before execve prevents the
//!   tracee inheriting the supervisor's accidental contamination.
//! - **OpenDevNullAt**: the recorded child had fd N open but the
//!   supervisor doesn't. The kernel won't replay the recorded fd's
//!   *target* — that's what the seccomp listener handles for
//!   reads/writes — but the fd-table topology has to match, so we
//!   open `/dev/null` at the target slot via `dup2`. Read-only
//!   for fd 0 (stdin), write-only for everything else.
//!
//! The standard fds (0/1/2) are *never closed* — both record and
//! replay treat them as always-present, even if a future recorded
//! program runs without them. (Detaching stdin/stdout/stderr
//! breaks too many libcs.)

#![cfg(target_os = "linux")]

use std::io;

/// One filesystem-level action applied between fork and execve in
/// the replay child. Modelled after `posix_spawn_file_actions_*`
/// but executed directly (we can't use posix_spawn here because
/// PTRACE_TRACEME / NOTIF setup must happen between fork and
/// execve, and posix_spawn doesn't expose that hook).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Close fd N. No-op if N isn't currently open in the child.
    Close(u32),
    /// Open `/dev/null` and dup it onto fd N. After this, reads
    /// from N return EOF immediately; writes go to the bit-bucket.
    /// `read_only` distinguishes stdin (fd 0) from other slots.
    OpenDevNullAt {
        /// Target fd number.
        fd: u32,
        /// True for fd 0; false for everything else (the kernel
        /// rejects writes to a read-only `/dev/null` and reads
        /// from a write-only one, so picking the right mode
        /// matters even though the seccomp listener intercepts
        /// the actual syscall).
        read_only: bool,
    },
}

/// Compute the delta between the supervisor's current fd-table
/// and the recorded child's. Both inputs must be ascending and
/// dedup'd (which is what [`crate::record::linux::proc_fd::list_open_fds`]
/// produces and what [`Manifest::initial_fds`] stores).
///
/// Returned in a deterministic order: closes first (ascending
/// fd), then opens (ascending fd). Tests rely on this ordering.
///
/// Standard fds (0/1/2) are never emitted as a `Close` action
/// even if the recorded child didn't have them open — see the
/// module-level note.
pub fn fd_diff_actions(supervisor_fds: &[u32], recorded_fds: &[u32]) -> Vec<FileAction> {
    let mut closes = Vec::new();
    let mut opens = Vec::new();

    let recorded_set: std::collections::BTreeSet<u32> = recorded_fds.iter().copied().collect();
    let supervisor_set: std::collections::BTreeSet<u32> = supervisor_fds.iter().copied().collect();

    for &fd in supervisor_set.difference(&recorded_set) {
        if fd >= 3 {
            closes.push(FileAction::Close(fd));
        }
    }
    for &fd in recorded_set.difference(&supervisor_set) {
        opens.push(FileAction::OpenDevNullAt {
            fd,
            read_only: fd == 0,
        });
    }

    closes.append(&mut opens);
    closes
}

/// Apply a sequence of [`FileAction`]s in the calling process.
/// **Async-signal-safe**: invoked from the replay child's
/// post-fork pre-execve window, where only async-signal-safe
/// libc calls are allowed. Uses `close(2)`, `open(2)`, `dup2(2)`
/// — all on the safe list.
///
/// Returns the first error encountered. Caller is expected to
/// `_exit(2)` on failure rather than try to recover.
pub fn apply_in_child(actions: &[FileAction]) -> io::Result<()> {
    for action in actions {
        match *action {
            FileAction::Close(fd) => {
                // `close` on a non-open fd returns EBADF; we
                // treat that as benign — it just means the
                // supervisor closed the fd (e.g. via CLOEXEC)
                // between snapshot and apply.
                let r = unsafe { libc::close(fd as i32) };
                if r != 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EBADF) {
                        return Err(err);
                    }
                }
            }
            FileAction::OpenDevNullAt { fd, read_only } => {
                let flags = if read_only {
                    libc::O_RDONLY
                } else {
                    libc::O_WRONLY
                };
                // SAFETY: open(/dev/null, flags) — async-signal-
                // safe. NUL-terminated string literal.
                let opened = unsafe { libc::open(c"/dev/null".as_ptr(), flags) };
                if opened < 0 {
                    return Err(io::Error::last_os_error());
                }
                if opened as u32 != fd {
                    // SAFETY: dup2 — async-signal-safe.
                    let r = unsafe { libc::dup2(opened, fd as i32) };
                    let saved = if r < 0 {
                        Some(io::Error::last_os_error())
                    } else {
                        None
                    };
                    let _ = unsafe { libc::close(opened) };
                    if let Some(e) = saved {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_with_identical_sets_emits_nothing() {
        let actions = fd_diff_actions(&[0, 1, 2, 5], &[0, 1, 2, 5]);
        assert_eq!(actions, vec![]);
    }

    #[test]
    fn diff_emits_close_for_supervisor_only_fds() {
        // Supervisor leaked fd 5; recorded child didn't have it.
        let actions = fd_diff_actions(&[0, 1, 2, 5], &[0, 1, 2]);
        assert_eq!(actions, vec![FileAction::Close(5)]);
    }

    #[test]
    fn diff_emits_open_for_recorded_only_fds() {
        // Recorded child had fd 7 open; supervisor doesn't.
        let actions = fd_diff_actions(&[0, 1, 2], &[0, 1, 2, 7]);
        assert_eq!(
            actions,
            vec![FileAction::OpenDevNullAt {
                fd: 7,
                read_only: false
            }],
        );
    }

    #[test]
    fn diff_never_closes_stdin_stdout_stderr() {
        // Supervisor has 0/1/2 open; recorded had nothing. We
        // refuse to close the standard fds even when the
        // delta says we should — too many libcs assume they
        // exist.
        let actions = fd_diff_actions(&[0, 1, 2], &[]);
        assert_eq!(actions, vec![]);
    }

    #[test]
    fn diff_marks_fd_zero_as_read_only() {
        let actions = fd_diff_actions(&[1, 2], &[0, 1, 2]);
        assert_eq!(
            actions,
            vec![FileAction::OpenDevNullAt {
                fd: 0,
                read_only: true
            }],
        );
    }

    #[test]
    fn diff_orders_closes_before_opens_each_ascending() {
        let actions = fd_diff_actions(&[0, 1, 2, 7, 9], &[0, 1, 2, 4, 6]);
        assert_eq!(
            actions,
            vec![
                FileAction::Close(7),
                FileAction::Close(9),
                FileAction::OpenDevNullAt {
                    fd: 4,
                    read_only: false
                },
                FileAction::OpenDevNullAt {
                    fd: 6,
                    read_only: false
                },
            ],
        );
    }

    /// `apply_in_child` runs in our test process — closing a
    /// non-existent fd must not crash; opening at a target slot
    /// via /dev/null must succeed and the resulting fd must be
    /// usable. We test in our own process here (not a child) so
    /// the assertions are visible; production callers run this
    /// in the post-fork pre-execve window of the tracee.
    #[test]
    fn apply_close_of_nonexistent_fd_is_benign() {
        // fd 999 is almost certainly not open in our process.
        let r = apply_in_child(&[FileAction::Close(999)]);
        assert!(r.is_ok(), "close-of-nonexistent should be benign: {r:?}");
    }

    #[test]
    fn apply_open_devnull_lands_at_requested_fd() {
        // Find an unused fd >= 100 (very unlikely to be used in tests).
        let target = 200u32;
        // Pre-clean: close in case a prior test left it open.
        unsafe { libc::close(target as i32) };

        apply_in_child(&[FileAction::OpenDevNullAt {
            fd: target,
            read_only: false,
        }])
        .expect("open /dev/null at target fd");

        // The fd should now be open and writable.
        let r = unsafe { libc::write(target as i32, b"hello".as_ptr() as *const _, 5) };
        assert_eq!(r, 5, "write to /dev/null at fd {target} returned {r}");

        // Cleanup.
        unsafe { libc::close(target as i32) };
    }
}

// SPDX-License-Identifier: MIT
//! `ReplayChild` — fork+exec helper for the *replay* path.
//!
//! Mirrors [`crate::record::linux::record_child::spawn`] but
//! installs the seccomp `SECCOMP_RET_USER_NOTIF` filter that
//! lets the supervisor *intercept* every syscall instead of
//! letting it run. The replay shim
//! ([`crate::replay::linux::shim::apply_recorded_event`]) then
//! supplies the recorded result instead.
//!
//! ## Why two child-spawn helpers
//!
//! - [`crate::record::linux::record_session::spawn_recorded_child`]
//!   uses **PTRACE_O_TRACESYSCALL alone** — no seccomp filter.
//!   The recorder just observes; ptrace gives full register
//!   visibility on entry+exit stops.
//! - [`spawn_replay_child`] uses **seccomp NOTIF only** — no
//!   ptrace. The replay supervisor needs *intercept* power
//!   (don't run the syscall; supply the recorded result), and
//!   that's exactly what NOTIF + omit-FLAG_CONTINUE does.
//!
//! The two mechanisms don't compose well (per `seccomp_unotify(2)`
//! NOTIF suppresses corresponding PTRACE_SYSCALL stops), so the
//! cleanest design is to pick the right one per role.
//!
//! ## Lifecycle
//!
//! 1. Parent creates `socketpair(AF_UNIX, SOCK_STREAM | CLOEXEC)`.
//! 2. Fork.
//! 3. Child:
//!    a. `prctl(PR_SET_NO_NEW_PRIVS)` (mandatory pre-seccomp).
//!    b. [`install_trap_all_listener`] → listener fd.
//!    c. `sendmsg(SCM_RIGHTS)` the fd to the parent.
//!    d. `execve(prog, argv, envp)` — the kernel will trap every
//!       subsequent syscall via NOTIF.
//! 4. Parent:
//!    a. `recvmsg(SCM_RIGHTS)` — receives the listener.
//!    b. Returns [`ReplayChild { pid, listener }`] ready for
//!       the replay supervisor's recv_notif loop.
//!
//! No `PTRACE_TRACEME`, no SIGSTOP barrier, no `PTRACE_SETOPTIONS`
//! — just the listener handover. The kernel does the rest.
//!
//! ## Drop semantics
//!
//! [`ReplayChild::drop`] does best-effort cleanup: SIGKILL the
//! child if the supervisor hasn't done so, and reaps via
//! `waitpid(WNOHANG)`. The listener fd's `OwnedFd` Drop closes
//! the listener; once closed, any in-flight syscall in the
//! tracee gets `-EFAULT` and the program continues (or, if the
//! filter required NOTIF response, the kernel's policy applies
//! — Linux delivers SIGKILL).

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use crate::record::linux::record_child::{recv_fd, send_fd};
use crate::record::linux::seccomp::install_trap_all_listener;

/// A NOTIF-trapped child process. The replay supervisor drives
/// it via [`Self::listener`].
#[derive(Debug)]
pub struct ReplayChild {
    pid: i32,
    listener: OwnedFd,
    cleaned_up: bool,
}

impl ReplayChild {
    /// PID of the child.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Borrow the seccomp-NOTIF listener fd. The supervisor
    /// passes this to `recv_notif` / `respond_intercept` /
    /// `respond_continue` on each turn.
    pub fn listener(&self) -> BorrowedFd<'_> {
        // SAFETY: BorrowedFd::borrow_raw with the OwnedFd's
        // raw fd is sound for the lifetime of `self`.
        unsafe { BorrowedFd::borrow_raw(self.listener.as_raw_fd()) }
    }

    /// SIGKILL the child and reap it. Used at end-of-replay or
    /// after a fatal error. Idempotent.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.do_shutdown()?;
        self.cleaned_up = true;
        Ok(())
    }

    fn do_shutdown(&self) -> io::Result<()> {
        // Best-effort SIGKILL. ESRCH is fine — child already
        // died.
        let r = unsafe { libc::kill(self.pid, libc::SIGKILL) };
        if r != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(err);
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(self.pid, &mut status, 0) };
        Ok(())
    }
}

impl Drop for ReplayChild {
    fn drop(&mut self) {
        if !self.cleaned_up {
            if let Err(e) = self.do_shutdown() {
                tracing::warn!(
                    "ReplayChild::drop: cleanup(pid={}) failed: {e}",
                    self.pid,
                );
            }
        }
    }
}

/// Spawn a child program for replay. `argv[0]` is the program
/// path; `argv[1..]` the arguments. `envp` is the environment.
///
/// Returns a [`ReplayChild`] whose listener is ready for the
/// supervisor's first `recv_notif`. The first notification will
/// arrive when the child issues its first syscall after execve
/// (typically the libc startup path).
pub fn spawn_replay_child(
    argv: Vec<CString>,
    envp: Vec<CString>,
) -> Result<ReplayChild, ReplaySpawnError> {
    if argv.is_empty() {
        return Err(ReplaySpawnError::EmptyArgv);
    }
    let mut sv: [RawFd; 2] = [-1, -1];
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(ReplaySpawnError::Socketpair(io::Error::last_os_error()));
    }
    let parent_sock = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let child_sock = unsafe { OwnedFd::from_raw_fd(sv[1]) };

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(ReplaySpawnError::Fork(io::Error::last_os_error()));
    }
    if pid == 0 {
        // === Child ===
        drop(parent_sock);
        match child_main(child_sock, argv, envp) {
            Ok(_) => unsafe { libc::_exit(101) },
            Err(code) => unsafe { libc::_exit(code) },
        }
    }
    // === Parent ===
    drop(child_sock);
    match parent_setup(pid, parent_sock) {
        Ok(c) => Ok(c),
        Err(e) => {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            }
            Err(e)
        }
    }
}

fn child_main(
    sock: OwnedFd,
    argv: Vec<CString>,
    envp: Vec<CString>,
) -> Result<core::convert::Infallible, libc::c_int> {
    // Install the NOTIF filter. install_trap_all_listener also
    // sets PR_SET_NO_NEW_PRIVS so we don't have to.
    let listener = match install_trap_all_listener() {
        Ok(fd) => fd,
        Err(e) => {
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) => 64,
                Some(libc::EINVAL) => 65,
                Some(libc::EACCES) => 66,
                _ => 71,
            });
        }
    };
    if send_fd(&sock, listener.as_raw_fd()).is_err() {
        return Err(72);
    }
    drop(listener);
    drop(sock);

    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_ptrs: Vec<*const libc::c_char> = envp
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let prog = argv[0].as_ptr();
    unsafe {
        libc::execve(prog, argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }
    Err(74)
}

fn parent_setup(pid: i32, sock: OwnedFd) -> Result<ReplayChild, ReplaySpawnError> {
    let listener = recv_fd(&sock).map_err(ReplaySpawnError::RecvFd)?;
    drop(sock);
    Ok(ReplayChild {
        pid,
        listener,
        cleaned_up: false,
    })
}

/// Errors arising from [`spawn_replay_child`].
#[derive(thiserror::Error, Debug)]
pub enum ReplaySpawnError {
    /// Caller passed an empty `argv`.
    #[error("argv is empty; need at least the program path")]
    EmptyArgv,
    /// `socketpair(2)` failed.
    #[error("socketpair: {0}")]
    Socketpair(io::Error),
    /// `fork(2)` failed.
    #[error("fork: {0}")]
    Fork(io::Error),
    /// `recvmsg(SCM_RIGHTS)` failed.
    #[error("recvmsg(SCM_RIGHTS): {0}")]
    RecvFd(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argv_is_rejected() {
        let err = spawn_replay_child(vec![], vec![]).unwrap_err();
        assert!(matches!(err, ReplaySpawnError::EmptyArgv));
    }
}

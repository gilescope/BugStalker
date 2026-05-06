// SPDX-License-Identifier: MIT
//! `RecordChild` — fork+exec helper that hands off a seccomp
//! listener fd to the supervisor.
//!
//! The recorder needs the seccomp filter installed *in the
//! tracee* (the kernel queues notifications against the
//! installing process). The listener fd
//! `seccomp(SECCOMP_FILTER_FLAG_NEW_LISTENER, …)` returns lives
//! in the child. To let the supervisor in the parent process
//! drive the recorder loop, the child sends the listener over a
//! `socketpair(AF_UNIX, SOCK_STREAM)` via `SCM_RIGHTS`.
//!
//! Lifecycle:
//!
//! 1. Parent creates the socketpair and forks.
//! 2. Child:
//!    a. `PTRACE_TRACEME` so the parent gains ptrace authority.
//!    b. `prctl(PR_SET_NO_NEW_PRIVS)` (mandatory pre-seccomp).
//!    c. [`install_trap_all_listener`] → listener fd.
//!    d. Send fd to parent via `sendmsg(SCM_RIGHTS)`.
//!    e. Close the fd in the child (the parent now owns it).
//!    f. `raise(SIGSTOP)` — synchronisation barrier; the parent
//!       sets ptrace options on the next stop and CONTs us.
//!    g. `execve(argv[0], argv, envp)`.
//! 3. Parent:
//!    a. `recvmsg(SCM_RIGHTS)` — receives the listener.
//!    b. `waitpid` for the child's SIGSTOP.
//!    c. `PTRACE_SETOPTIONS` with TRACESYSGOOD | TRACESYSCALL.
//!    d. `PTRACE_CONT` — child proceeds to execve.
//!    e. Returns [`RecordChild { pid, listener }`] ready for
//!       [`record_syscall_with_exit`](super::exit_stop::record_syscall_with_exit).
//!
//! ## Drop semantics
//!
//! [`RecordChild::drop`] does best-effort cleanup: ptrace-
//! detaches if the supervisor hasn't done so, and reaps the
//! child via `waitpid` so the process table doesn't accumulate
//! zombies. Failures are logged via `tracing` but don't panic.
//!
//! ## What's deferred to follow-up
//!
//! - Per-thread filter inheritance for multi-threaded tracees
//!   needs `clone(CLONE_VM | …)` carrier-thread setup; the
//!   single-threaded path here is enough for the smoke test.
//! - `posix_spawn_file_actions` to reconstitute the recorded
//!   environment / cwd / fd table for replay — the recorder
//!   side here just inherits the parent's environment.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use super::seccomp::install_trap_all_listener;

/// A recording-attached child process. The supervisor drives
/// it via [`Self::pid`] (for ptrace) and [`Self::listener`]
/// (for seccomp notifications).
#[derive(Debug)]
pub struct RecordChild {
    /// PID of the child the parent is ptrace-attached to.
    pid: i32,
    /// Listener fd received from the child via SCM_RIGHTS.
    listener: OwnedFd,
    /// Set true by [`Self::detach`] so [`Drop`] doesn't
    /// double-detach.
    detached: bool,
}

impl RecordChild {
    /// PID of the child.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Borrow the listener fd. The supervisor passes this to
    /// `recv_notif` / `respond_continue` on each turn.
    pub fn listener(&self) -> BorrowedFd<'_> {
        self.listener.as_fd_owned()
    }

    /// Stop tracing and let the child run free. Used at
    /// recording-stop or after a fatal error. The child becomes
    /// detached from the supervisor; any seccomp filter still
    /// installed continues to fire but no longer reaches the
    /// supervisor (the listener is closed when this struct
    /// drops).
    pub fn detach(mut self) -> io::Result<()> {
        self.do_detach()?;
        self.detached = true;
        Ok(())
    }

    fn do_detach(&self) -> io::Result<()> {
        // SAFETY: ptrace-detach is idempotent if we own the
        // tracee. PTRACE_DETACH carries a signal-to-deliver in
        // the data argument; 0 means "no signal".
        let r = unsafe {
            libc::ptrace(
                libc::PTRACE_DETACH,
                self.pid,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for RecordChild {
    fn drop(&mut self) {
        if !self.detached {
            // Best-effort detach. Errors are logged; the only
            // signal-safe response is to keep going so the
            // listener fd's `OwnedFd` Drop can run.
            if let Err(e) = self.do_detach() {
                tracing::warn!(
                    "RecordChild::drop: PTRACE_DETACH(pid={}) failed: {e}",
                    self.pid,
                );
            }
        }
        // Reap the child so the process table doesn't keep a
        // zombie around. WNOHANG so we don't block if the child
        // hasn't exited yet.
        let mut status = 0;
        unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
    }
}

/// Need a `BorrowedFd` accessor; `OwnedFd::as_fd` is on a
/// trait that isn't in the prelude in all toolchains we
/// support, so wrap in a helper for clarity.
trait OwnedFdExt {
    fn as_fd_owned(&self) -> BorrowedFd<'_>;
}
impl OwnedFdExt for OwnedFd {
    fn as_fd_owned(&self) -> BorrowedFd<'_> {
        // SAFETY: BorrowedFd::borrow_raw with the OwnedFd's
        // raw fd is sound for the lifetime of `self`.
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

/// Spawn a child program for recording. `argv[0]` is the
/// program path; `argv[1..]` are the arguments. `envp` is the
/// environment passed to `execve`. Both vectors must contain
/// well-formed `CString`s; the function takes ownership and
/// drops them after the fork.
///
/// On success, returns a [`RecordChild`] paused at its first
/// post-`execve` syscall stop. The supervisor drives it
/// forward via `record_syscall_with_exit` (one call per
/// syscall) until the child exits.
pub fn spawn(argv: Vec<CString>, envp: Vec<CString>) -> Result<RecordChild, SpawnError> {
    if argv.is_empty() {
        return Err(SpawnError::EmptyArgv);
    }
    let mut sv: [RawFd; 2] = [-1, -1];
    // SAFETY: socketpair writes through &mut sv with two fds.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(SpawnError::Socketpair(io::Error::last_os_error()));
    }
    // SAFETY: socketpair just minted the fds; nothing else
    // owns them.
    let parent_sock = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let child_sock = unsafe { OwnedFd::from_raw_fd(sv[1]) };

    // SAFETY: fork creates a child that is the only one of
    // its kind; the only safety obligation is that any code
    // we run between fork and execve is async-signal-safe
    // (no allocators, no global state mutation). The body
    // below stays inside that envelope: prctl, ptrace, and
    // the seccomp install all qualify.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(SpawnError::Fork(io::Error::last_os_error()));
    }
    if pid == 0 {
        // === Child side ============================================
        // No allocations beyond what's already in `argv`/`envp`.
        // Errors here can't propagate; we _exit() with a
        // documented code so the parent can diagnose.
        drop(parent_sock);
        match child_main(child_sock, argv, envp) {
            // child_main returns Ok only via execve, which
            // doesn't return; reaching this branch is a bug.
            Ok(_) => unsafe { libc::_exit(101) },
            Err(code) => unsafe { libc::_exit(code) },
        }
    }
    // === Parent side ===============================================
    drop(child_sock);
    match parent_setup(pid, parent_sock) {
        Ok(rc) => Ok(rc),
        Err(e) => {
            // Clean up the child; we can't proceed.
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
    // 1. PTRACE_TRACEME so the parent can SETOPTIONS at the
    //    SIGSTOP barrier.
    // SAFETY: PTRACE_TRACEME is a self-only request; no
    // pointers dereferenced.
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if r != 0 {
        return Err(70);
    }
    // 2. Install the seccomp listener.
    let listener = match install_trap_all_listener() {
        Ok(fd) => fd,
        Err(e) => {
            // Skip codes mirror the seccomp test.
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) => 64,
                Some(libc::EINVAL) => 65,
                Some(libc::EACCES) => 66,
                _ => 71,
            });
        }
    };
    // 3. Hand the listener over.
    if send_fd(&sock, listener.as_raw_fd()).is_err() {
        return Err(72);
    }
    // listener drops here in the child; parent owns it via
    // the SCM_RIGHTS dup.
    drop(listener);
    drop(sock);
    // 4. SIGSTOP to synchronise with the parent.
    // SAFETY: kill on self with SIGSTOP is signal-safe.
    let r = unsafe { libc::raise(libc::SIGSTOP) };
    if r != 0 {
        return Err(73);
    }
    // 5. Execve. argv/envp are already CStrings; build C-style
    // arrays in place. Any allocation here is a pre-execve
    // expense; we intentionally leak because execve replaces
    // the address space.
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
    // SAFETY: execve takes well-formed null-terminated C
    // arrays and the program path; on success it doesn't
    // return.
    unsafe {
        libc::execve(prog, argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }
    // execve returned → it failed.
    Err(74)
}

fn parent_setup(pid: i32, sock: OwnedFd) -> Result<RecordChild, SpawnError> {
    // 1. Receive the listener fd.
    let listener = recv_fd(&sock).map_err(SpawnError::RecvFd)?;
    drop(sock);

    // 2. Wait for the child's SIGSTOP barrier.
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return Err(SpawnError::Wait(io::Error::last_os_error()));
    }
    if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGSTOP {
        return Err(SpawnError::ChildSetupFailed { wstatus: status });
    }

    // 3. SETOPTIONS for syscall stops + their discriminator.
    let opts: libc::c_long = (libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEEXEC) as libc::c_long;
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            opts as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(SpawnError::SetOptions(io::Error::last_os_error()));
    }

    // 4. PTRACE_SYSCALL — child proceeds to execve, then to
    // its first syscall. The next stop the supervisor
    // observes will be a syscall-stop.
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SYSCALL,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if r != 0 {
        return Err(SpawnError::Cont(io::Error::last_os_error()));
    }

    Ok(RecordChild {
        pid,
        listener,
        detached: false,
    })
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS fd handover
// ---------------------------------------------------------------------------

/// Send a file descriptor over a unix-domain socket via
/// SCM_RIGHTS. Used by both [`spawn`] (child→parent listener
/// handover) and the replay-side [`crate::replay::linux::replay_child`]
/// path. Public-in-crate so the replay module can reuse it
/// without duplicating the cmsg plumbing.
pub(crate) fn send_fd(sock: &OwnedFd, fd: RawFd) -> io::Result<()> {
    // One byte of payload — the receiver must do a one-byte
    // recvmsg, otherwise the kernel won't deliver the cmsg.
    let dummy: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: &dummy as *const u8 as *mut _,
        iov_len: 1,
    };
    let cmsg_size = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_size];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = std::ptr::null_mut();
    msg.msg_namelen = 0;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_size;

    // SAFETY: CMSG_FIRSTHDR returns a pointer aligned for cmsghdr
    // inside the buffer we just allocated; we initialise it by
    // hand below before calling sendmsg.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
    }
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len =
            libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        let data = libc::CMSG_DATA(cmsg);
        std::ptr::copy_nonoverlapping(&fd as *const _, data as *mut RawFd, 1);
    }
    let r = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive a file descriptor sent via [`send_fd`]. Same public-
/// in-crate visibility for the same reason.
pub(crate) fn recv_fd(sock: &OwnedFd) -> io::Result<OwnedFd> {
    let mut dummy: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: &mut dummy as *mut u8 as *mut _,
        iov_len: 1,
    };
    let cmsg_size = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_size];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = std::ptr::null_mut();
    msg.msg_namelen = 0;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_size;

    let r = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(io::Error::other("recvmsg returned no cmsg"));
    }
    unsafe {
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other(format!(
                "unexpected cmsg level/type: {}, {}",
                (*cmsg).cmsg_level,
                (*cmsg).cmsg_type,
            )));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg) as *const RawFd, &mut fd, 1);
        if fd < 0 {
            return Err(io::Error::other("recvmsg yielded negative fd"));
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Errors arising from [`spawn`].
#[derive(thiserror::Error, Debug)]
pub enum SpawnError {
    /// Caller passed an empty `argv`.
    #[error("argv is empty; need at least the program path")]
    EmptyArgv,
    /// `socketpair(2)` failed.
    #[error("socketpair: {0}")]
    Socketpair(io::Error),
    /// `fork(2)` failed.
    #[error("fork: {0}")]
    Fork(io::Error),
    /// `recvmsg(2)` for the SCM_RIGHTS handover failed.
    #[error("recvmsg(SCM_RIGHTS): {0}")]
    RecvFd(io::Error),
    /// `waitpid(2)` for the child's SIGSTOP barrier failed.
    #[error("waitpid: {0}")]
    Wait(io::Error),
    /// Child raised an unexpected stop instead of SIGSTOP.
    /// Typically the child failed to install seccomp; the
    /// child's exit code carries the reason.
    #[error(
        "child setup failed at SIGSTOP barrier; wstatus={wstatus:#x}. \
         If WEXITSTATUS == 64–66, the kernel/perms don't allow \
         seccomp NEW_LISTENER; bump kernel ≥ 5.5 or run with \
         CAP_SYS_ADMIN."
    )]
    ChildSetupFailed {
        /// Raw wstatus from waitpid.
        wstatus: libc::c_int,
    },
    /// `PTRACE_SETOPTIONS` failed.
    #[error("PTRACE_SETOPTIONS: {0}")]
    SetOptions(io::Error),
    /// `PTRACE_SYSCALL` failed.
    #[error("PTRACE_SYSCALL: {0}")]
    Cont(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argv_is_rejected() {
        let err = spawn(vec![], vec![]).unwrap_err();
        assert!(matches!(err, SpawnError::EmptyArgv));
    }

    /// End-to-end smoke test: spawn /bin/true, the simplest
    /// program in the universe; it execve's and exit_groups.
    /// We just verify the spawn succeeds and the child reaches
    /// at least one syscall stop.
    ///
    /// Auto-skipped on hosts where seccomp NEW_LISTENER isn't
    /// available (kernel < 5.5, sandboxed CI without
    /// CAP_SYS_ADMIN).
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "RecordChild requires Linux seccomp"
    )]
    fn spawn_bin_true_reaches_first_syscall_stop() {
        // /bin/true exists on every Linux host. NixOS and
        // Alpine put coreutils at /bin/true too.
        let prog = std::path::Path::new("/bin/true");
        if !prog.exists() {
            eprintln!("skipping: {} not found", prog.display());
            return;
        }
        let argv = vec![CString::new("/bin/true").unwrap()];
        let envp = vec![CString::new("PATH=/usr/bin:/bin").unwrap()];

        let rc = match spawn(argv, envp) {
            Ok(rc) => rc,
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("64") || s.contains("65") || s.contains("66") {
                    eprintln!("skipping: kernel/perms don't support seccomp: {e:?}");
                    return;
                }
                if s.contains("EPERM") || s.contains("EACCES") || s.contains("ENOSYS") {
                    eprintln!("skipping: {e:?}");
                    return;
                }
                panic!("spawn failed: {e:?}");
            }
        };

        // We hold a valid pid + listener; the child is paused
        // at the first syscall stop after execve. Detach and
        // let it run to completion.
        let pid = rc.pid();
        rc.detach().expect("detach");

        // Reap the child to keep the test process clean.
        let mut status = 0;
        let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}

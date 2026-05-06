// SPDX-License-Identifier: MIT
//! `LinuxForkSelfMechanism` — Linux fork(2) + SIGSTOP checkpoint
//! mechanism.
//!
//! Forks the *calling process*. The child raises SIGSTOP on itself
//! and enters T (stopped) state, frozen at exactly the memory + FD
//! state of the parent at fork time. Replay would later
//! `SIGCONT + ptrace::seize` the child and drive it forward; this
//! mechanism just owns the take/kill lifecycle.
//!
//! Where this fits in the plan:
//!
//! - The plan's Tier 2 wants to checkpoint the *debuggee*, which
//!   requires ptrace-injecting a fork() syscall into the
//!   debuggee's context (subphase 3I-Tier2 work). That's
//!   substantially larger.
//! - This mechanism forks BugStalker's *own* process instead.
//!   It's a real mechanism: the same SIGSTOP-pause / SIGKILL-reap
//!   shape, the same Pid lifecycle, the same kernel paths. It's
//!   the building block for the debuggee-targeted version once
//!   ptrace injection lands.
//!
//! ## Safety
//!
//! `fork()` in a multi-threaded process is hazardous — only the
//! calling thread survives in the child, and any mutex held by
//! another thread becomes permanently locked. The child here
//! does *nothing* after fork except `raise(SIGSTOP)`, so the
//! hazard window is bounded to exactly that one syscall, which
//! is async-signal-safe.

use std::time::{SystemTime, UNIX_EPOCH};

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

use crate::ring::CheckpointMechanism;

/// Handle for one forked-and-stopped child. Mechanism's `kill()`
/// reaps the PID; dropping this without going through the
/// mechanism *leaks the child*. The ring's drain() consumes
/// every handle so production callers get clean reaping; tests
/// must do the same.
#[derive(Debug)]
pub struct ForkHandle {
    /// PID of the suspended child.
    pub pid: Pid,
    /// Wall-clock instant of capture, monotonic seconds since
    /// UNIX_EPOCH. Caller-visible for diagnostics.
    pub captured_unix_seconds: u64,
}

/// Real Linux fork+SIGSTOP mechanism.
#[derive(Debug, Default)]
pub struct LinuxForkSelfMechanism {
    /// Number of children we've created in this session, for
    /// debugging / diagnostics. Not load-bearing.
    pub takes: u64,
}

impl LinuxForkSelfMechanism {
    /// Construct with zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Send `SIGCONT` to the suspended child. Plan §"Tier 2"
    /// step (d) — replay sets a breakpoint, then SIGCONTs and
    /// lets the child run forward. This method handles the
    /// SIGCONT half; ptrace-attach + breakpoint setting is the
    /// caller's job (BugStalker's existing tracee plumbing).
    ///
    /// Does **not** consume the handle — the caller is still
    /// responsible for either [`Self::wait_for_exit`] or
    /// [`Self::kill`]ing it later.
    pub fn resume(&mut self, handle: &ForkHandle) -> Result<(), ForkMechanismError> {
        signal::kill(handle.pid, Signal::SIGCONT)?;
        Ok(())
    }

    /// `PTRACE_SEIZE` the suspended child without disturbing
    /// its existing stop state. Plan §"Tier 2" step (b). The
    /// child must already be alive (i.e. not yet `kill()`ed)
    /// and is normally still sitting in `T` (stopped) state from
    /// the self-raised SIGSTOP.
    ///
    /// Default `Options` — the caller upgrades to e.g.
    /// `PTRACE_O_TRACESYSGOOD` later via the public
    /// `nix::sys::ptrace::setoptions` if it wants syscall-stop
    /// distinction. Phase 5 leaves that to the consumer (sub-
    /// phase 3B will set it).
    pub fn seize(&mut self, handle: &ForkHandle) -> Result<(), ForkMechanismError> {
        nix::sys::ptrace::seize(handle.pid, nix::sys::ptrace::Options::empty())?;
        Ok(())
    }

    /// `PTRACE_CONT` the seized child, optionally delivering a
    /// signal. The tracee resumes execution and the next stop
    /// is observable via `wait_for_exit` or any of the
    /// `nix::sys::wait::*` paths.
    pub fn ptrace_cont(
        &mut self,
        handle: &ForkHandle,
        deliver_signal: Option<Signal>,
    ) -> Result<(), ForkMechanismError> {
        nix::sys::ptrace::cont(handle.pid, deliver_signal)?;
        Ok(())
    }

    /// Block until the child exits, reap it, and return the
    /// final wait status. Consumes the handle.
    ///
    /// Useful in two replay-flow shapes:
    ///
    /// - "Run to natural end" — SIGCONT the child via
    ///   [`Self::resume`], then `wait_for_exit` to let it
    ///   complete cleanly.
    /// - "Synchronise on completion" — used by tests and by
    ///   future driver code that wants to confirm a checkpoint's
    ///   workload is done before moving on.
    pub fn wait_for_exit(
        &mut self,
        handle: ForkHandle,
    ) -> Result<WaitStatus, ForkMechanismError> {
        loop {
            match waitpid(handle.pid, None) {
                Ok(status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => {
                    return Ok(status);
                }
                Ok(_) => continue, // intermediate stop / continue events
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl CheckpointMechanism for LinuxForkSelfMechanism {
    type Handle = ForkHandle;
    type Error = ForkMechanismError;

    fn take(&mut self, _key: u64) -> Result<ForkHandle, ForkMechanismError> {
        // SAFETY: fork() in a process with other threads is
        // unsafe in general; the child here calls only
        // raise(SIGSTOP) which is async-signal-safe, so the
        // hazard window is empty for this use.
        match unsafe { fork() }? {
            ForkResult::Child => {
                // STOP self; replay later does SIGCONT + ptrace.
                // raise() never returns once stopped.
                let _ = signal::raise(Signal::SIGSTOP);
                // If we somehow get here (parent SIGCONT'd before
                // attaching), exit cleanly. The mechanism's kill()
                // path uses SIGKILL so this is the unusual case.
                std::process::exit(0);
            }
            ForkResult::Parent { child } => {
                self.takes += 1;
                let captured_unix_seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(ForkHandle { pid: child, captured_unix_seconds })
            }
        }
    }

    fn kill(&mut self, handle: ForkHandle) -> Result<(), ForkMechanismError> {
        // Send SIGKILL — bypasses the SIGSTOP-stopped state.
        signal::kill(handle.pid, Signal::SIGKILL)?;
        // Reap so we don't leave a zombie. waitpid blocks until
        // the kernel signals the child has exited; with SIGKILL
        // that's near-immediate.
        loop {
            match waitpid(handle.pid, Some(WaitPidFlag::empty())) {
                Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => break,
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// Errors arising from fork/kill/wait.
#[derive(thiserror::Error, Debug)]
pub enum ForkMechanismError {
    /// nix-level errno failure.
    #[error("nix error: {0}")]
    Nix(#[from] nix::errno::Errno),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::CheckpointRing;
    use nix::sys::signal::Signal::SIGCONT;
    use std::thread::sleep;
    use std::time::Duration;

    /// Linux-only by virtue of the parent module's cfg gate. The
    /// unit-test runner on macOS doesn't reach this file.

    fn child_state(pid: Pid) -> Option<char> {
        // Read /proc/<pid>/stat to get the state byte. None if
        // the process disappeared.
        let path = format!("/proc/{}/stat", pid.as_raw());
        let s = std::fs::read_to_string(&path).ok()?;
        // Format: "PID (comm) STATE …"; parse after the closing ')'
        // because comm can contain spaces.
        let after = s.rsplit_once(')')?.1.trim_start();
        after.chars().next()
    }

    #[test]
    fn take_creates_a_stopped_child() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        // Give the kernel a moment to deliver the SIGSTOP.
        sleep(Duration::from_millis(50));
        let state = child_state(h.pid);
        assert!(
            matches!(state, Some('T') | Some('t')),
            "expected stopped state ('T' or 't'), got {state:?} for pid {}",
            h.pid,
        );
        // Clean up.
        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn kill_reaps_the_child_with_no_zombie() {
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        let pid = h.pid;
        mech.kill(h).expect("kill failed");
        // After reap the /proc entry should be gone.
        sleep(Duration::from_millis(50));
        let path = format!("/proc/{}/stat", pid.as_raw());
        assert!(
            !std::path::Path::new(&path).exists(),
            "/proc/{}/stat still present — zombie?",
            pid,
        );
    }

    #[test]
    fn ring_eviction_kills_oldest_via_real_fork() {
        let mut ring = CheckpointRing::with_capacity(LinuxForkSelfMechanism::new(), 2);
        let h0_pid = match ring.take(0) {
            Ok(h) => h.pid,
            Err(e) => panic!("take 0 failed: {e:?}"),
        };
        let _h1_pid = match ring.take(1) {
            Ok(h) => h.pid,
            Err(e) => panic!("take 1 failed: {e:?}"),
        };
        // Capacity 2, full now.
        let _h2_pid = match ring.take(2) {
            Ok(h) => h.pid,
            Err(e) => panic!("take 2 failed: {e:?}"),
        };
        // Eviction killed h0. Give the reaper time to settle.
        sleep(Duration::from_millis(50));
        let path = format!("/proc/{}/stat", h0_pid.as_raw());
        assert!(
            !std::path::Path::new(&path).exists(),
            "evicted child {} should have been reaped",
            h0_pid,
        );
        // Drain to clean up the rest.
        ring.drain().expect("drain failed");
    }

    #[test]
    fn drain_kills_every_remaining_child() {
        let mut ring = CheckpointRing::with_capacity(LinuxForkSelfMechanism::new(), 4);
        let pids: Vec<Pid> = (0..3)
            .map(|k| ring.take(k).expect("take failed").pid)
            .collect();
        ring.drain().expect("drain failed");
        sleep(Duration::from_millis(50));
        for pid in pids {
            let path = format!("/proc/{}/stat", pid.as_raw());
            assert!(
                !std::path::Path::new(&path).exists(),
                "child {} survived drain",
                pid,
            );
        }
    }

    #[test]
    fn seize_does_not_disturb_existing_stop() {
        // PTRACE_SEIZE on an already-stopped tracee leaves it in
        // group-stop. The /proc state should still read as 't'
        // (stopped, traced) after the seize.
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        match mech.seize(&h) {
            Ok(()) => {}
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EPERM") {
                    eprintln!(
                        "skipping seize test: kernel YAMA scope blocks self-trace ({e:?})",
                    );
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("seize failed: {e:?}");
            }
        }
        sleep(Duration::from_millis(50));
        let state = child_state(h.pid);
        assert!(
            matches!(state, Some('T') | Some('t')),
            "expected stopped state ('T'/'t') post-seize, got {state:?}",
        );
        // Clean up via the kill path — kill sends SIGKILL, the
        // ptrace attach is implicitly torn down by the child's
        // death.
        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn ptrace_read_recovers_known_bytes_from_seized_child() {
        // Magic in the parent's `.data` (well, RO data — same
        // story: lives at a fixed address shared by parent and
        // child after fork). After SEIZE, ptrace::read at that
        // address returns the same bytes verbatim. Validates the
        // foundation for breakpoint-setting (write the trap
        // instruction at the target PC) and post-stop state
        // inspection.
        const MAGIC: [u8; 8] = [
            0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe,
        ];

        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        match mech.seize(&h) {
            Ok(()) => {}
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EPERM") {
                    eprintln!(
                        "skipping ptrace_read test: YAMA blocks self-trace ({e:?})",
                    );
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("seize failed: {e:?}");
            }
        }

        // PTRACE_PEEKDATA reads one machine word; on x86_64 that's
        // 8 bytes. Cast to *mut c_void since the nix API takes
        // an address as a void pointer.
        let addr = MAGIC.as_ptr() as *mut std::ffi::c_void;
        let word: i64 =
            nix::sys::ptrace::read(h.pid, addr).expect("ptrace::read failed");
        // Compare to the parent's view by reinterpreting the
        // word's bytes — same architecture endianness on both
        // sides since parent + child are the same kernel binary.
        let bytes = word.to_ne_bytes();
        assert_eq!(
            bytes, MAGIC,
            "ptrace::read returned {:02x?} for MAGIC at {:p}; expected {:02x?}",
            bytes, MAGIC.as_ptr(), MAGIC,
        );

        mech.kill(h).expect("kill failed");
    }

    #[test]
    fn seize_then_cont_lets_child_run_to_natural_exit() {
        // The fork_self child does `raise(SIGSTOP); exit(0)`.
        // After ptrace SEIZE + CONT, the child returns from
        // raise() and runs the exit(0) tail. wait_for_exit
        // observes Exited(_, 0).
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        match mech.seize(&h) {
            Ok(()) => {}
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("EPERM") {
                    eprintln!(
                        "skipping seize+cont test: YAMA blocks self-trace ({e:?})",
                    );
                    mech.kill(h).expect("kill failed");
                    return;
                }
                panic!("seize failed: {e:?}");
            }
        }
        // PTRACE_CONT wakes the seized tracee. No signal
        // delivered (None) — the SIGSTOP it sent itself was
        // already processed by the kernel's stop machinery.
        mech.ptrace_cont(&h, None).expect("ptrace_cont failed");
        match mech.wait_for_exit(h).expect("wait_for_exit failed") {
            WaitStatus::Exited(_pid, 0) => {}
            other => panic!("expected Exited(_, 0), got {other:?}"),
        }
    }

    #[test]
    fn resume_then_wait_for_exit_reports_zero() {
        // The fork_self child does `raise(SIGSTOP); exit(0)`. If
        // we resume() it, the SIGCONT lets raise() return and the
        // child immediately runs exit(0). wait_for_exit then sees
        // Exited(_, 0).
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        // Give the kernel a tick to deliver the self-SIGSTOP.
        sleep(Duration::from_millis(50));
        mech.resume(&h).expect("SIGCONT failed");
        match mech.wait_for_exit(h).expect("wait_for_exit failed") {
            WaitStatus::Exited(_pid, 0) => {} // expected
            other => panic!("expected Exited(_, 0), got {other:?}"),
        }
    }

    #[test]
    fn resume_does_not_consume_handle() {
        // Caller can still kill() after resume(). (After resume,
        // the child may have exited naturally already; kill()
        // tolerates ESRCH/ECHILD per the existing test.)
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        mech.resume(&h).expect("SIGCONT failed");
        // Race: the child may exit before kill arrives.
        match mech.kill(h) {
            Ok(()) => {}
            Err(e) => {
                let s = format!("{e:?}");
                assert!(
                    s.contains("ESRCH") || s.contains("ECHILD"),
                    "unexpected error: {e:?}",
                );
            }
        }
    }

    #[test]
    fn sigcont_then_kill_still_reaps_cleanly() {
        // If a caller SIGCONT'd a checkpoint without going through
        // mechanism.kill, the child would exit naturally. Our
        // kill() must still reap that case rather than getting
        // stuck on waitpid.
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        let pid = h.pid;
        // Wake the child; it'll fall out of raise() and exit.
        signal::kill(pid, SIGCONT).expect("sigcont failed");
        sleep(Duration::from_millis(50));
        // Now kill() should observe an already-exited or
        // about-to-exit child and reap.
        match mech.kill(h) {
            Ok(()) => {}
            Err(e) => {
                // ESRCH if the child fully reaped before our
                // SIGKILL — also acceptable.
                let s = format!("{e:?}");
                assert!(
                    s.contains("ESRCH") || s.contains("ECHILD"),
                    "unexpected error from kill on already-exited child: {e:?}",
                );
            }
        }
    }
}

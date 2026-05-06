// SPDX-License-Identifier: MIT
//! Sub-phase 3F — single-CPU thread serialisation.
//!
//! The recorder pins the entire tracee to one CPU. With only
//! one core in play, thread context switches happen *only* at
//! syscall boundaries (which the recorder already traps via
//! seccomp) and timer interrupts (which the recorder can log
//! as Event::Signal SIGALRM); there's no concurrent execution
//! to lose. On replay the supervisor schedules each thread for
//! exactly the recorded instruction count before yielding to
//! the next, reproducing the same interleaving.
//!
//! ## Cost
//!
//! Plan-stated: ~5× slowdown on CPU-bound workloads. PT-assisted
//! recording (3H) drops that to ~2×.
//!
//! ## What this commit lands
//!
//! - [`pin_to_single_cpu_for_self`] — `sched_setaffinity` to a
//!   one-element CPU set on the current process.
//! - [`current_affinity_for_self`] — diagnostic; reads the
//!   live mask via `sched_getaffinity` so the tracee can
//!   confirm the pin actually took effect.
//! - [`SingleCpuPin`] — typed wrapper that holds the chosen CPU
//!   and exposes the affinity-restoration path (the recorder
//!   undoes the pin once recording stops).
//!
//! ## Out of scope (deferred)
//!
//! - PMU-based instruction-retired counter for inter-switch
//!   counts. Lands when 3H wires PT into the engine.
//! - The ptrace-driven thread scheduler at replay time.

#![cfg(target_os = "linux")]

use std::io;
use std::mem;

/// Affinity-mask wrapper that owns one `cpu_set_t`. Public
/// because callers may want to peek at the chosen CPU for
/// diagnostics.
#[derive(Debug, Clone)]
pub struct SingleCpuPin {
    /// CPU id the recorder pinned to. Always `> 0` after
    /// successful [`pin_to_single_cpu_for_self`].
    pub cpu: u32,
    /// Pre-pin affinity, kept so [`Self::restore`] can undo
    /// the pin cleanly when recording stops.
    pub previous: AffinityMask,
}

/// CPU mask snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffinityMask {
    /// CPU ids that were enabled. Sorted ascending, deduped.
    pub cpus: Vec<u32>,
}

impl AffinityMask {
    /// Number of CPUs in the mask.
    pub fn len(&self) -> usize {
        self.cpus.len()
    }

    /// True if no CPUs are enabled. The kernel never returns
    /// this from `sched_getaffinity`; provided so callers can
    /// pattern-match against `Default`.
    pub fn is_empty(&self) -> bool {
        self.cpus.is_empty()
    }
}

impl Default for AffinityMask {
    fn default() -> Self {
        Self { cpus: Vec::new() }
    }
}

/// Read the current affinity mask of the calling process.
pub fn current_affinity_for_self() -> io::Result<AffinityMask> {
    // SAFETY: zero-init is the documented sentinel; the kernel
    // fills the mask via the pointer + size we pass.
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    let r = unsafe {
        libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cpus = Vec::new();
    // `CPU_SETSIZE` is 1024 on glibc; the loop's bounded.
    for cpu in 0..libc::CPU_SETSIZE as i32 {
        // SAFETY: CPU_ISSET is read-only on the mask, well-
        // defined for any non-negative cpu id.
        if unsafe { libc::CPU_ISSET(cpu as usize, &set) } {
            cpus.push(cpu as u32);
        }
    }
    Ok(AffinityMask { cpus })
}

/// Pin the current process to one CPU. Returns a
/// [`SingleCpuPin`] that remembers the pre-pin affinity so the
/// caller can [`SingleCpuPin::restore`] it once recording stops.
///
/// `cpu` must already be enabled in the current affinity mask
/// — pinning to a CPU the process can't otherwise reach is a
/// configuration error and the kernel's `sched_setaffinity`
/// call would return `EINVAL`. The check is performed before
/// the syscall so the diagnostic is more helpful than `EINVAL`.
pub fn pin_to_single_cpu_for_self(cpu: u32) -> Result<SingleCpuPin, AffinityError> {
    let previous = current_affinity_for_self().map_err(AffinityError::ReadCurrent)?;
    if !previous.cpus.iter().any(|c| *c == cpu) {
        return Err(AffinityError::CpuNotInCurrentMask {
            requested: cpu,
            available: previous.cpus.clone(),
        });
    }
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    // SAFETY: CPU_SET writes the bit at `cpu` in the mask. The
    // local was zeroed above so the resulting mask has exactly
    // one bit set.
    unsafe { libc::CPU_SET(cpu as usize, &mut set) };
    let r = unsafe {
        libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set)
    };
    if r != 0 {
        return Err(AffinityError::SetFailed(io::Error::last_os_error()));
    }
    Ok(SingleCpuPin { cpu, previous })
}

impl SingleCpuPin {
    /// Restore the pre-pin affinity. The recorder calls this
    /// when recording stops so the tracee isn't left pinned.
    pub fn restore(self) -> io::Result<()> {
        // Plan §Invariants: "Single-CPU mode pre-record" — the
        // restore path is symmetric.
        debug_assert!(!self.previous.cpus.is_empty());
        let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
        for c in &self.previous.cpus {
            unsafe { libc::CPU_SET(*c as usize, &mut set) };
        }
        let r = unsafe {
            libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Errors arising from [`pin_to_single_cpu_for_self`] /
/// [`SingleCpuPin::restore`].
#[derive(thiserror::Error, Debug)]
pub enum AffinityError {
    /// Couldn't read the current affinity mask.
    #[error("sched_getaffinity for current process: {0}")]
    ReadCurrent(io::Error),
    /// `cpu` wasn't enabled in the current mask. Pinning to it
    /// would return `EINVAL`; flagged proactively so the
    /// diagnostic actually helps.
    #[error(
        "requested CPU {requested} isn't in the current affinity mask \
         (available: {available:?}); pin would have failed with EINVAL"
    )]
    CpuNotInCurrentMask {
        /// What the caller asked for.
        requested: u32,
        /// What's actually available.
        available: Vec<u32>,
    },
    /// `sched_setaffinity` itself failed.
    #[error("sched_setaffinity: {0}")]
    SetFailed(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_affinity_is_non_empty() {
        // `sched_getaffinity` returns at least one bit on every
        // running process — if the test got this far, *some*
        // CPU is allowed.
        let mask = current_affinity_for_self().expect("getaffinity");
        assert!(
            !mask.is_empty(),
            "current affinity unexpectedly empty (pid {})",
            std::process::id(),
        );
    }

    #[test]
    fn pin_to_already_allowed_cpu_round_trips() {
        let mask = current_affinity_for_self().expect("getaffinity");
        // Pick the lowest-numbered allowed CPU.
        let cpu = match mask.cpus.first() {
            Some(c) => *c,
            None => return, // no CPUs to pin to; impossible but skip
        };
        let pin = match pin_to_single_cpu_for_self(cpu) {
            Ok(p) => p,
            Err(e) => {
                // Some sandboxed test environments deny
                // sched_setaffinity (EPERM); skip rather than fail.
                eprintln!("skipping pin test: {e:?}");
                return;
            }
        };
        assert_eq!(pin.cpu, cpu);
        let after = current_affinity_for_self().expect("getaffinity post");
        assert_eq!(after.cpus, vec![cpu], "single-CPU pin didn't take");
        pin.restore().expect("restore");
        let restored = current_affinity_for_self().expect("getaffinity restore");
        assert_eq!(
            restored, mask,
            "restore didn't bring the original mask back"
        );
    }

    #[test]
    fn pin_to_disallowed_cpu_is_diagnosed_proactively() {
        // CPU 4096 is well outside any plausible mask.
        let err = pin_to_single_cpu_for_self(4096).unwrap_err();
        match err {
            AffinityError::CpuNotInCurrentMask { requested: 4096, .. } => {}
            other => panic!("expected CpuNotInCurrentMask, got {other:?}"),
        }
    }

    #[test]
    fn affinity_mask_default_is_empty() {
        let m = AffinityMask::default();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }
}

// SPDX-License-Identifier: MIT
//! Cross-platform bridge for Tier-2 writable-state capture and restore.
//!
//! Both `bs_replay::darwin::checkpoint` (Mach `mach_vm_*`) and
//! `bs_replay::linux::checkpoint_capture` (`/proc/<pid>/{maps,mem}`)
//! expose the same conceptual API — snapshot every writable VMA's
//! bytes, restore them back into a process by pid — but with
//! slightly different signatures (the Linux side takes `Pid`,
//! Darwin takes `i32`; the error enums differ; the `RestoreReport`
//! types come from different modules).
//!
//! This module wraps both behind a single `WritableState` alias and
//! `capture` / `restore` functions so consumers (live-reverse
//! step-back, EnC restart via Tier-2 fn-entry snapshots) don't have
//! to learn the platform difference.
//!
//! The on-disk payload is byte-for-byte identical on both platforms
//! (each upstream module's docs spell that out), so a checkpoint
//! captured by either side decodes via either side's `from_payload`.
//! Useful nowhere yet, but keeps cross-platform debugging stories
//! honest if we ever want to ship a recording across hosts.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::Context as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd::Pid;

#[cfg(target_os = "macos")]
pub use bs_replay::darwin::checkpoint::WritableState;
#[cfg(target_os = "linux")]
pub use bs_replay::linux::checkpoint_capture::WritableState;

/// Restore-side outcome flattened to a (`written`, `skipped`) pair
/// so the platform error types don't leak through this API.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct RestoreReport {
    /// Regions whose bytes were successfully written into the
    /// target process's address space.
    pub written: usize,
    /// Regions that failed to write — most commonly because the
    /// VMA no longer exists at restore time (heap shrank, file-
    /// backed mapping was unloaded), or because the kernel
    /// rejected the write on a special region. Per-region
    /// failures are *not* fatal — the caller decides whether the
    /// skip count makes restoration unusable.
    pub skipped: usize,
}

/// Snapshot every writable, private VMA of the target process.
///
/// On macOS the inferior must be reachable via `task_for_pid`
/// (codesigned supervisor with `com.apple.security.cs.debugger`
/// or root). On Linux the inferior must already be ptrace-attached
/// so `/proc/<pid>/mem` is openable.
///
/// Some kernel-special mappings (`[vsyscall]` on hardened kernels,
/// `[uprobes]`, dyld shared cache regions) are silently skipped —
/// they wouldn't deserialise cleanly anyway, and replay re-maps
/// them from their original source.
#[cfg(target_os = "macos")]
pub fn capture(pid: Pid) -> anyhow::Result<WritableState> {
    bs_replay::darwin::checkpoint::capture_writable_state(pid.as_raw())
        .with_context(|| format!("capture writable state for {pid}"))
}

#[cfg(target_os = "linux")]
pub fn capture(pid: Pid) -> anyhow::Result<WritableState> {
    bs_replay::linux::checkpoint_capture::capture_writable_state(pid)
        .with_context(|| format!("capture writable state for {pid}"))
}

/// Write every region of `state` back into the target process's
/// address space at its captured start address. Pairs with
/// [`capture`]; the caller is responsible for ensuring the target
/// is ptrace-attached (or Mach task_for_pid'd) and stopped.
#[cfg(target_os = "macos")]
pub fn restore(pid: Pid, state: &WritableState) -> anyhow::Result<RestoreReport> {
    let report = bs_replay::darwin::checkpoint::restore_writable_state(pid.as_raw(), state)
        .with_context(|| format!("restore writable state for {pid}"))?;
    Ok(RestoreReport {
        written: report.written,
        skipped: report.skipped,
    })
}

#[cfg(target_os = "linux")]
pub fn restore(pid: Pid, state: &WritableState) -> anyhow::Result<RestoreReport> {
    let report = bs_replay::linux::checkpoint_capture::restore_writable_state(pid, state)
        .with_context(|| format!("restore writable state for {pid}"))?;
    Ok(RestoreReport {
        written: report.written,
        skipped: report.skipped,
    })
}

// SPDX-License-Identifier: MIT
//! Tier-2 fn-entry snapshots for EnC restart.
//!
//! Problem: [`crate::debugger::Debugger::restart_top_frame`]'s DWARF-only
//! path restores named-arg registers, callee-saved registers, and RSP
//! from the unwound frame-1 view. That covers the state DWARF
//! describes. It does **not** cover caller-saved registers the body
//! reads before writing, unnamed stack slots holding iterator state
//! (`Iter::ptr/end`), drop flags, trait-object spills, or float args
//! passed in XMM registers. Functions whose body makes outbound
//! `CALL`s (think `compute` calling `Iterator::sum`) fail at restart
//! with plausible-looking garbage because the callees inherit state
//! we couldn't reconstruct.
//!
//! Fix: snapshot the inferior's *entire* writable state plus the
//! complete register file at function entry, restore at restart.
//! That covers everything DWARF doesn't.
//!
//! ## How a snapshot gets taken
//!
//! When the user sets a breakpoint inside function F,
//! [`crate::debugger::Debugger`] installs a **transparent breakpoint**
//! at F's entry. Transparent breakpoints don't surface to the UI —
//! they fire a callback then continue silently. The callback here
//! captures registers via [`crate::debugger::register::RegisterMap::current`]
//! and writable state via [`crate::debugger::platform_checkpoint::capture`],
//! storing the pair in this module's [`EncCheckpointStore`] keyed by
//! the function's start address. On every subsequent call to F, the
//! snapshot is refreshed (last-write-wins).
//!
//! ## How a snapshot gets used
//!
//! When the user runs EnC `patch.apply` with restart and the function
//! body contains outbound calls,
//! [`crate::debugger::Debugger::restart_top_frame`] looks for a snapshot
//! matching the current function's start address. If found, it calls
//! [`crate::debugger::platform_checkpoint::restore`] to write the
//! writable state back, copies the snapshot's registers into the
//! inferior via [`crate::debugger::register::RegisterMap::persist`],
//! and resumes — the patched function body re-executes from entry
//! against the exact memory and register state it had on this call.
//! The DWARF-only path stays untouched for functions without inner
//! calls (it's strictly faster and doesn't need a snapshot).
//!
//! ## What V1 doesn't do yet
//!
//! - **Recursion**: only the most recent snapshot per function is
//!   kept. Recursive calls collapse into one entry — the latest call
//!   wins. For the canonical EnC demo (single call into a function
//!   the user is debugging) this is fine; for restarting an inner
//!   recursive frame, a future revision will key by `(fn_start, RBP)`.
//! - **Eviction**: snapshots accumulate as long as user breakpoints
//!   remain set. For the demo (a handful of bps in a single edit
//!   loop) this is negligible — each snapshot is ~hundreds of KB to
//!   a few MB. A bounded LRU is a follow-up.
//! - **Cross-thread**: only the focused thread's registers are
//!   captured. A function that spawns threads mid-body would not
//!   replay them; this matches the existing Tier-2 live-reverse
//!   behaviour and is consistent with the platform memory snapshot
//!   (single-process write-back via `pwrite64` / `mach_vm_write`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;

use nix::unistd::Pid;

use super::platform_checkpoint::{self, WritableState};
use super::register::RegisterMap;

/// One captured fn-entry snapshot.
#[derive(Clone)]
pub struct EncCheckpoint {
    /// Writable memory state at the moment the snap-bp at fn entry
    /// fired (i.e. before the prologue ran).
    pub writable: WritableState,
    /// Focused-thread registers at fn entry. The PC field will be
    /// at the snap-bp address (== `fn_start`); restorers should
    /// either preserve it or set it back to `fn_start` explicitly.
    pub registers: RegisterMap,
    /// Function start address — also the snap-bp address. Stored
    /// alongside the snapshot so a key-mismatch (caller asks for
    /// the wrong slot) can be flagged loudly instead of silently
    /// restoring into the wrong function. Read by the future
    /// recursion-aware lookup path (Phase 3 V2); kept on the
    /// struct so the on-the-wire layout doesn't need a follow-up
    /// migration.
    #[allow(dead_code)]
    pub fn_start: u64,
}

/// Single-snapshot-per-function store. Keyed by relocated function
/// entry IP. Last write wins; recursion just keeps the deepest
/// call's snapshot, which is the one a `restart_top_frame` for that
/// frame would want anyway.
///
/// Also tracks which functions have already had their snap-bp
/// installed so the breakpoint-add hook doesn't double-arm.
#[derive(Default)]
pub struct EncCheckpointStore {
    snapshots: HashMap<u64, EncCheckpoint>,
    armed: HashMap<u64, ()>,
}

impl EncCheckpointStore {
    /// Returns true if a snap-bp at this function entry has already
    /// been installed. Used to skip double-arming when the user sets
    /// multiple breakpoints inside the same function.
    pub fn is_armed(&self, fn_start: u64) -> bool {
        self.armed.contains_key(&fn_start)
    }

    /// Mark a snap-bp as installed for this function entry. Pair
    /// with `is_armed` to gate the actual breakpoint installation
    /// on the breakpoint-add hook side.
    pub fn mark_armed(&mut self, fn_start: u64) {
        self.armed.insert(fn_start, ());
    }

    /// Try to capture a snapshot for `fn_start` from `pid`. Stores
    /// the snapshot keyed by `fn_start` on success and returns the
    /// number of writable regions captured (for diagnostics).
    /// Failures (capture errored, register read failed) are logged
    /// and the store is left unchanged — the user-visible failure
    /// mode is "restart can't use a snapshot for this function",
    /// which falls back to the inner-call refusal in
    /// `restart_top_frame`.
    pub fn capture_at(&mut self, fn_start: u64, pid: Pid) -> usize {
        let registers = match RegisterMap::current(pid) {
            Ok(regs) => regs,
            Err(err) => {
                log::warn!(
                    target: "enc_checkpoint",
                    "skip snap capture at fn_start=0x{fn_start:x}: register read failed: {err}",
                );
                return 0;
            }
        };
        let writable = match platform_checkpoint::capture(pid) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    target: "enc_checkpoint",
                    "skip snap capture at fn_start=0x{fn_start:x}: writable-state capture failed: {err:#}",
                );
                return 0;
            }
        };
        let region_count = writable.regions.len();
        self.snapshots.insert(
            fn_start,
            EncCheckpoint {
                writable,
                registers,
                fn_start,
            },
        );
        log::debug!(
            target: "enc_checkpoint",
            "captured fn-entry snapshot for fn_start=0x{fn_start:x} ({region_count} writable regions)",
        );
        region_count
    }

    /// Borrow a snapshot for `fn_start`, if one exists. Restorers
    /// should clone the registers (the underlying ptrace API
    /// `persist(self, pid)` consumes by value) before writing them
    /// back.
    pub fn peek(&self, fn_start: u64) -> Option<&EncCheckpoint> {
        self.snapshots.get(&fn_start)
    }

    /// Test/diagnostic accessor — number of stored snapshots.
    #[allow(dead_code)]
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_has_no_snapshots() {
        let store = EncCheckpointStore::default();
        assert_eq!(store.snapshot_count(), 0);
        assert!(store.peek(0x1000).is_none());
        assert!(!store.is_armed(0x1000));
    }

    #[test]
    fn arming_is_idempotent_per_fn() {
        let mut store = EncCheckpointStore::default();
        assert!(!store.is_armed(0x1000));
        store.mark_armed(0x1000);
        assert!(store.is_armed(0x1000));
        store.mark_armed(0x1000); // no-op duplicate
        assert!(store.is_armed(0x1000));
        // Different fn doesn't share the armed state.
        assert!(!store.is_armed(0x2000));
    }
}

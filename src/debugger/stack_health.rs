// SPDX-License-Identifier: MIT
//
// Variables-view §5.5 — stack health hints surfaced on the
// variables pane / threads pane header pill. All signals derive
// from data the debugger already has:
//
//   * **Thread stack budget** — the segment-writability index
//     (built from `proc_maps` in §5.2) gives us the mapping the
//     current SP sits in. That mapping IS the thread stack
//     (`[stack]` for the main thread, anon-rw for spawned
//     threads). `total = mapping.size`, `used = mapping.end - sp`.
//   * **Recursion depth** — walk the existing `Backtrace` and
//     count `fn_name` repeats. Trivial.
//   * **Frame count** — `Backtrace::len()`.
//
// Per-frame size from CFI is mentioned in the design doc but
// deferred for v0 (variables-view.md §7) — it would extend
// `FrameSpan` with a `cfa` field and a follow-up
// `frame_size = CFA(this) − CFA(parent)` computation.

use std::collections::HashMap;

use crate::debugger::Debugger;
use crate::debugger::address::RelocatedAddress;
use crate::debugger::debugee::dwarf::unwind::Backtrace;
use nix::unistd::Pid;

/// Aggregate stack-health snapshot for one thread. See module doc.
#[derive(Debug, Clone, Default)]
pub struct StackHealth {
    /// Total stack bytes available to this thread (from proc_maps).
    /// `None` when the thread's SP isn't in any mapped region —
    /// shouldn't happen for a running thread, but defensively
    /// reported as None rather than panicking.
    pub thread_stack_size: Option<u64>,
    /// Bytes used of the thread stack at the current PC. `None`
    /// when `thread_stack_size` is `None`.
    pub thread_stack_used: Option<u64>,
    /// Total number of frames in the unwound backtrace.
    pub frame_count: u32,
    /// Functions appearing ≥ 2 times in the backtrace, mapped to
    /// their occurrence count. Drives the `[rec N]` frame tag and
    /// the amber / red threshold on the threads-pane bar.
    pub recursion: HashMap<String, u32>,
    /// Maximum recursion depth across all functions. `0` when no
    /// function repeats.
    pub max_recursion: u32,
}

impl StackHealth {
    /// Stack usage as a percentage (0–100). Returns `None` when
    /// either total or used couldn't be determined.
    pub fn used_pct(&self) -> Option<u8> {
        let total = self.thread_stack_size?;
        let used = self.thread_stack_used?;
        if total == 0 {
            return None;
        }
        // Saturate at 100 — used can briefly exceed total if the
        // thread is mid-guard-page-overflow (which is exactly the
        // case we want to flag, so report 100% rather than wrap).
        let pct = (used.saturating_mul(100) / total).min(100);
        Some(pct as u8)
    }
}

/// Compute a [`StackHealth`] snapshot for the given thread. Pure
/// observation — no debuggee state is modified.
pub fn compute(dbg: &Debugger, pid: Pid, backtrace: &Backtrace) -> StackHealth {
    let (thread_stack_size, thread_stack_used) = thread_stack_budget(dbg, pid);
    let (recursion, max_recursion) = recursion_counts(backtrace);
    StackHealth {
        thread_stack_size,
        thread_stack_used,
        frame_count: backtrace.len() as u32,
        recursion,
        max_recursion,
    }
}

/// Find the mapping containing the thread's current SP and
/// compute `(total_size, used)`. The mapping is the thread's
/// stack — `[stack]` for the main thread, an anon-rw mapping for
/// spawned threads. Both are in the segment index.
fn thread_stack_budget(dbg: &Debugger, pid: Pid) -> (Option<u64>, Option<u64>) {
    use crate::debugger::register::{Register, RegisterMap};
    let regs = match RegisterMap::current(pid) {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let sp = regs.value(Register::SP);
    let sp_addr = RelocatedAddress::from(sp as usize);
    let reg = dbg.dwarf_registry();
    let Some(range) = reg.containing_range(sp_addr) else {
        return (None, None);
    };
    let total = u64::from(range.to).saturating_sub(u64::from(range.from));
    let used = u64::from(range.to).saturating_sub(sp);
    (Some(total), Some(used))
}

/// Count fn_name occurrences. Returns (per-name counts, max).
fn recursion_counts(backtrace: &Backtrace) -> (HashMap<String, u32>, u32) {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for span in backtrace {
        if let Some(name) = &span.func_name {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
    }
    counts.retain(|_, n| *n >= 2);
    let max = counts.values().copied().max().unwrap_or(0);
    (counts, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_pct_returns_none_when_size_unknown() {
        let h = StackHealth {
            thread_stack_size: None,
            thread_stack_used: Some(1024),
            ..Default::default()
        };
        assert_eq!(h.used_pct(), None);
    }

    #[test]
    fn used_pct_returns_none_when_size_is_zero() {
        let h = StackHealth {
            thread_stack_size: Some(0),
            thread_stack_used: Some(0),
            ..Default::default()
        };
        assert_eq!(h.used_pct(), None);
    }

    #[test]
    fn used_pct_basic_arithmetic() {
        let h = StackHealth {
            thread_stack_size: Some(1024),
            thread_stack_used: Some(256),
            ..Default::default()
        };
        assert_eq!(h.used_pct(), Some(25));
    }

    #[test]
    fn used_pct_saturates_at_100() {
        // Mid-overflow case: used briefly exceeds total.
        let h = StackHealth {
            thread_stack_size: Some(1024),
            thread_stack_used: Some(2048),
            ..Default::default()
        };
        assert_eq!(h.used_pct(), Some(100));
    }
}

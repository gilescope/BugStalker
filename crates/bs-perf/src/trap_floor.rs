// SPDX-License-Identifier: MIT
//! Per-trap overhead floor estimator for the macOS rusage perf path.
//!
//! `proc_pid_rusage`'s `ri_instructions`/`ri_cycles` charge each debugger trap
//! (Mach exception round-trip → ptrace stop → resume) to the debuggee — about
//! 35k instructions per trap on Apple Silicon, which dwarfs a stepped line's
//! handful of real instructions (signal-to-noise ≈ 8 : 35 000; see
//! `debug-step-costs.md`). We can't make a trap cheaper — it's kernel code —
//! and the user-mode-only PMU counter that would dodge it is entitlement-gated.
//! But every trap pays the *same* floor, so we cancel it by subtraction:
//! `corrected = raw − traps × floor`.
//!
//! The floor is learned passively from the user's own steps — no extra traps,
//! no debuggee perturbation. Each step contributes its per-trap cost
//! (`raw / traps`); the floor is the *minimum* over a recent window, because a
//! step's per-trap cost is `floor + user_work/traps ≥ floor`, so the cheapest
//! recent steps (trivial lines — common) reveal the floor. Trivial lines then
//! correct toward ~0 (honest: 8 instructions are unrecoverable under 35k of
//! noise); lines with real work keep their work; the floor is exact in the
//! millions-of-instructions regime.
//!
//! Known limit (the ±20% band): a `step_over` is 1 single-step trap + 1
//! breakpoint-hit trap, whose costs differ slightly, so a single blended floor
//! is off by up to ~one trap when a line's own work happens to be ~one floor.
//! Per-trap-*type* floors would tighten this; deferred until the kperf/PMU path
//! (which makes the floor moot) is in reach.

/// Recent per-trap samples kept; floor = min over the window. Small enough to
/// track machine-state drift (thermal/scheduler), large enough that a run of
/// work-heavy steps doesn't immediately lose a recent trivial-step floor.
const WINDOW: usize = 64;

/// Rolling-minimum estimate of one trap's fixed `ri_instructions` /
/// `ri_cycles` overhead, learned from observed steps. See module docs.
///
/// ```
/// use bs_perf::TrapFloor;
/// let mut floor = TrapFloor::new();
/// // A trivial 2-trap step costs ~36k/trap of pure overhead:
/// floor.observe(72_000, 72_000, 2);
/// // A later trivial step corrects to ~nothing…
/// assert!(floor.corrected_instructions(72_400, 2) < 1_000);
/// // …while a step that did real work keeps (almost) all of it:
/// assert_eq!(floor.corrected_instructions(19_072_000, 2), 19_072_000 - 2 * 36_000);
/// ```
#[derive(Debug, Clone, Default)]
pub struct TrapFloor {
    instr: MinRing,
    cycles: MinRing,
}

impl TrapFloor {
    /// Empty floor — the `corrected_*` methods pass values through unchanged
    /// until the first [`observe`](Self::observe).
    pub fn new() -> Self {
        Self::default()
    }

    /// Current per-trap instruction floor (min over the recent window), or 0
    /// when nothing has been observed yet.
    pub fn instr_floor(&self) -> u64 {
        self.instr.min().unwrap_or(0)
    }

    /// Current per-trap cycle floor, or 0 when nothing observed yet.
    pub fn cycles_floor(&self) -> u64 {
        self.cycles.min().unwrap_or(0)
    }

    /// Subtract the learned floor from a window's raw instruction count.
    /// `traps` is how many traps the step incurred (the debugger counts them).
    /// Saturates at 0 — a trivial line lands at "negligible", never negative.
    pub fn corrected_instructions(&self, raw: u64, traps: u64) -> u64 {
        raw.saturating_sub(traps.saturating_mul(self.instr_floor()))
    }

    /// As [`corrected_instructions`](Self::corrected_instructions), for cycles.
    pub fn corrected_cycles(&self, raw: u64, traps: u64) -> u64 {
        raw.saturating_sub(traps.saturating_mul(self.cycles_floor()))
    }

    /// Fold one completed step into the floor estimate. A `traps == 0` window
    /// carries no per-trap signal and is ignored.
    pub fn observe(&mut self, raw_instructions: u64, raw_cycles: u64, traps: u64) {
        if traps == 0 {
            return;
        }
        self.instr.push(raw_instructions / traps);
        self.cycles.push(raw_cycles / traps);
    }
}

/// Fixed-capacity ring of the last [`WINDOW`] samples with an O(n) `min`.
/// n ≤ 64 and `min` runs once per stop, so a linear scan beats maintaining a
/// monotonic deque.
#[derive(Debug, Clone)]
struct MinRing {
    buf: [u64; WINDOW],
    len: usize,
    next: usize,
}

impl Default for MinRing {
    fn default() -> Self {
        Self {
            buf: [0; WINDOW],
            len: 0,
            next: 0,
        }
    }
}

impl MinRing {
    fn push(&mut self, v: u64) {
        self.buf[self.next] = v;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
    }

    fn min(&self) -> Option<u64> {
        self.buf[..self.len].iter().copied().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_floor_passes_through() {
        let f = TrapFloor::new();
        assert_eq!(f.instr_floor(), 0);
        assert_eq!(f.corrected_instructions(72_000, 2), 72_000);
        assert_eq!(f.corrected_cycles(72_000, 2), 72_000);
    }

    #[test]
    fn trivial_line_corrects_to_near_zero() {
        let mut f = TrapFloor::new();
        for &raw in &[72_000u64, 71_500, 72_400] {
            f.observe(raw, raw, 2);
        }
        // floor ≈ 35_750/trap → a trivial 2-trap step nets ~0
        assert!(f.corrected_instructions(72_000, 2) < 2_000);
    }

    #[test]
    fn heavy_line_keeps_its_work() {
        let mut f = TrapFloor::new();
        f.observe(72_000, 72_000, 2); // 36k/trap floor
        assert_eq!(
            f.corrected_instructions(19_072_000, 2),
            19_072_000 - 2 * 36_000
        );
    }

    #[test]
    fn floor_is_min_not_latest() {
        let mut f = TrapFloor::new();
        f.observe(70_000, 70_000, 2); // 35k/trap
        f.observe(200_000, 200_000, 2); // 100k/trap — a step with real work
        assert_eq!(
            f.instr_floor(),
            35_000,
            "a work-heavy step must not raise the floor"
        );
    }

    #[test]
    fn observe_ignores_zero_traps() {
        let mut f = TrapFloor::new();
        f.observe(1_000, 1_000, 0);
        assert_eq!(f.instr_floor(), 0);
    }

    #[test]
    fn window_forgets_old_samples() {
        let mut f = TrapFloor::new();
        f.observe(2_000, 2_000, 1); // cheap outlier, then push it out of the window
        for _ in 0..WINDOW {
            f.observe(40_000, 40_000, 1);
        }
        assert_eq!(
            f.instr_floor(),
            40_000,
            "old min should have aged out of the ring"
        );
    }
}

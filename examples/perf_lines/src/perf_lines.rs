// SPDX-License-Identifier: MIT
//! Deterministic, single-threaded debuggee for perf-cost ballpark tests.
//!
//! Each labelled arithmetic line is a known-tiny amount of work. In a debug
//! build `i = i * 2` lowers to a checked multiply (~8 aarch64 instructions),
//! `i = i + 1` to a checked add (~5). Stepping over one of these lines should
//! cost a *handful* of debuggee instructions plus bounded stepping overhead —
//! not the tens of thousands the perf overlay currently reports (see
//! `debug-step-costs.md`, open question #3). The tests in
//! `tests/debugger/perf_cost.rs` break on these lines by number, so keep the
//! line layout below stable — `perf_cost.rs` documents the expected numbers.
use std::hint::black_box;

#[inline(never)]
fn arithmetic() {
    let mut i: i64 = black_box(3); // L16: seed (kept opaque so nothing folds)
    i = i * 2; // L17: MULTIPLY — checked mul, ~8 instructions
    i = i + 1; // L18: ADD — checked add, ~5 instructions
    black_box(i); // L19: sink
}

/// A hot loop of `iters` cheap iterations. Confirms the flip side of #3:
/// once user work dwarfs the ~35k fixed per-trap overhead, rusage instruction
/// counts ARE accurate (a `continue` across this measures ~iters×, not +35k).
/// `wrapping_add` / manual counter avoid overflow-check + iterator machinery.
#[inline(never)]
fn busy_loop(iters: u64) -> u64 {
    let mut acc = 0u64; // L28: pre-loop (break here)
    let mut k = 0u64; // L29
    while k < iters {
        // L30
        acc = acc.wrapping_add(k); // L32
        k += 1; // L33
    } // L34
    acc // L35: post-loop (break here)
}

fn main() {
    arithmetic();
    let heavy = busy_loop(black_box(1_000_000)); // L40: ~19M instr step target
    let medium = busy_loop(black_box(2_000)); // L41: ~38k instr step target
    black_box((heavy, medium)); // L42: sink
}

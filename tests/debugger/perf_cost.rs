// SPDX-License-Identifier: MIT
//! Ballpark guards on the per-line perf cost the overlay shows the user.
//!
//! Background — open question #3 (`debug-step-costs.md`): `proc_pid_rusage`'s
//! `ri_instructions`/`ri_cycles` charge each debugger trap (~35k instr) to the
//! debuggee, so a trivial stepped line reads ~10⁴× too high. The fix is
//! `bs_perf::TrapFloor`: count the traps a step incurs (the debugger tracks
//! them) and subtract a passively-learned per-trap floor — `corrected = raw −
//! traps × floor`. These tests exercise the *real* path (real stepping → real
//! trap counts → real rusage → `TrapFloor`) and assert on the **corrected**
//! number, i.e. what the Step Costs pane shows.
//!
//! We measure *line* cost, not *step* cost: a single source line can carry
//! several `.debug_line` rows (open question #1 — `i = i + 1` stops twice), so
//! a line is "stepped over" by repeating `step_over` until the source line
//! changes (open question #2). Trap counting makes this robust: however many
//! traps the line took (it varies with whether the line sits on a breakpoint),
//! `corrected` subtracts exactly that many floors.
//!
//! `perf_lines.rs` line map (keep in sync): L16 seed, L17 `i = i * 2`,
//! L18 `i = i + 1`, L19 sink; L40/L41 heavy/medium loop call targets.
use crate::PERF_LINES_APP;
use crate::common::{TestHooks, TestInfo};
use crate::prepare_debugee_process;
use bs_perf::TrapFloor;
use bs_perf::darwin::ProcessSnapshot;
use bugstalker::debugger::{Debugger, DebuggerBuilder, LineInstrCount};
use serial_test::serial;

/// A trivial debug-build arithmetic line is a handful of instructions
/// (checked multiply ≈ 8, checked add ≈ 5) — but the rusage path can't resolve
/// that finely. After floor subtraction what's left is the per-trap estimation
/// residual: the floor is the *minimum* sampled per-trap cost, while the
/// measured step's own traps cost a little more (the cold/warm spread is
/// ~6k/trap), so a noisy ~`traps × few-k` survives (observed up to ~5k). This
/// ceiling encodes that achievable precision (down from the ~73k–149k raw),
/// with headroom so it isn't flaky; per-trap-*type* floors or a user-mode PMU
/// counter would tighten it toward the true ~8.
const MAX_LINE_INSTRUCTIONS: u64 = 10_000;
/// A single source line should never need this many `step_over`s to clear —
/// the loop guard catches a stepping regression rather than spinning forever.
const MAX_SUBSTEPS: u32 = 16;

/// Build a fresh debugger stopped at `line` in perf_lines.
fn stopped_at(line: u64) -> (Debugger, TestInfo, i32) {
    let process = prepare_debugee_process(PERF_LINES_APP, &[]);
    let raw_pid = process.pid().as_raw();
    let info = TestInfo::default();
    let mut dbg = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", line).unwrap();
    dbg.start_debugee().unwrap();
    assert_eq!(info.line.get(), Some(line));
    (dbg, info, raw_pid)
}

/// Step over the *whole* source line currently at `cur` (looping `step_over`
/// until the reported line changes — robust to #1's multi-row lines),
/// measuring the summed rusage delta and the trap count for the line.
/// Returns `(raw_instructions, raw_cycles, traps, landed_line)`.
fn step_one_line(dbg: &mut Debugger, info: &TestInfo, raw_pid: i32, cur: u64) -> (u64, u64, u64, u64) {
    dbg.reset_trap_count();
    let before = ProcessSnapshot::capture(raw_pid).expect("rusage before");
    let mut substeps = 0u32;
    loop {
        dbg.step_over().unwrap();
        substeps += 1;
        assert!(
            substeps < MAX_SUBSTEPS,
            "still on line {cur} after {substeps} step_overs — stepping regression",
        );
        if info.line.get() != Some(cur) {
            break;
        }
    }
    let delta = ProcessSnapshot::capture(raw_pid)
        .expect("rusage after")
        .delta_since(before);
    (
        delta.instructions,
        delta.cycles,
        dbg.trap_count(),
        info.line.get().unwrap_or(0),
    )
}

/// Walk trivial lines from L16, priming a [`TrapFloor`] from each, and return
/// the floor-corrected `(instructions, cycles, raw_instructions)` for stepping
/// over `target_line`. The lines walked before the target (≥ L16) are the
/// priming steps — they establish the per-trap floor exactly as a real session
/// warms up over the user's first few steps.
fn corrected_line_cost(target_line: u64) -> (u64, u64, u64) {
    let (mut dbg, info, pid) = stopped_at(16);
    let mut floor = TrapFloor::new();
    let mut cur = 16u64;
    let result = loop {
        let (raw_i, raw_c, traps, landed) = step_one_line(&mut dbg, &info, pid, cur);
        // Correct with the floor from *prior* steps, then fold this step in.
        let corrected = (
            floor.corrected_instructions(raw_i, traps),
            floor.corrected_cycles(raw_c, traps),
            raw_i,
        );
        floor.observe(raw_i, raw_c, traps);
        if cur == target_line {
            break corrected;
        }
        assert!(landed != cur, "did not advance past line {cur}");
        cur = landed;
    };
    dbg.continue_debugee().unwrap();
    result
}

/// `i = i * 2` (L17) — corrected cost should be ≈0 (real work is ~8 instr,
/// unrecoverable under the trap floor, so "negligible" is the honest answer).
#[test]
#[serial]
fn multiply_line_corrected_instruction_ballpark() {
    let (instructions, _, _) = corrected_line_cost(17);
    assert!(
        instructions <= MAX_LINE_INSTRUCTIONS,
        "corrected `i = i * 2` cost {instructions} instructions; expected ≤ \
         {MAX_LINE_INSTRUCTIONS}. Trap-floor subtraction (#3) failed to fire.",
    );
}

/// `i = i + 1` (L18) — carries two `.debug_line` rows (#1); the line's two
/// sub-step traps are both subtracted, so the corrected cost is still ≈0.
#[test]
#[serial]
fn add_line_corrected_instruction_ballpark() {
    let (instructions, _, _) = corrected_line_cost(18);
    assert!(
        instructions <= MAX_LINE_INSTRUCTIONS,
        "corrected `i = i + 1` cost {instructions} instructions; expected ≤ \
         {MAX_LINE_INSTRUCTIONS}. Trap-floor subtraction (#3) failed to fire.",
    );
}

/// The correction must actually remove the bulk of the trap overhead, not just
/// trim it: the raw count carries tens of thousands of instructions of pure
/// trap cost, and the corrected value should be a tiny fraction of it.
#[test]
#[serial]
fn correction_removes_trap_overhead() {
    let (instructions, _, raw) = corrected_line_cost(17);
    assert!(
        raw > 10_000,
        "sanity: raw `i = i * 2` should carry the ~35k+/trap overhead, got {raw}",
    );
    // ≥4× (75%) removal. The 1-trap multiply line's raw is only ~38k, so the
    // ±5k residual caps the demonstrable reduction at ~8×; 4× is the
    // non-flaky floor that still proves most of the overhead is gone.
    assert!(
        instructions.saturating_mul(4) < raw,
        "correction should remove most of the {raw} raw instructions, but left \
         {instructions} (< 4× reduction)",
    );
}

/// The headline of the whole #3 arc: the EXACT instruction count for a source
/// line via step-counting — each ptrace single-step is exactly one retired
/// instruction, so the trap/kernel overhead is excluded by construction. A
/// debug-build `i = i * 2` (checked multiply) is a single-digit handful, NOT the
/// ~73k the rusage counters report.
#[test]
#[serial]
fn exact_instruction_count_multiply_line() {
    let (mut dbg, _info, _pid) = stopped_at(17);
    let count = dbg.count_line_instructions(1_000).unwrap();
    dbg.continue_debugee().unwrap();
    match count {
        // Observed exactly 9 (mov/mov/smull/asr/mul/stur/subs/b.ne/b — branch
        // not taken). Banded for rustc-codegen drift; the point is it's a
        // handful, decisively NOT the rusage ~73k.
        LineInstrCount::Exact(n) => assert!(
            (4..=20).contains(&n),
            "exact `i = i * 2` count {n} outside the expected handful (~9) — \
             step-counting should give the true count, not the rusage ~73k",
        ),
        LineInstrCount::Capped(n) => panic!("expected an exact count, got Capped({n})"),
    }
}

/// `step_over_or_count` is the overlay's exact-count step: exact on a no-call
/// line (single-step ≡ step-over → true count), clean fall-back on a call line —
/// landing on the next line either way (behaviour-preserving).
#[test]
#[serial]
fn step_over_or_count_exact_then_fallback() {
    // No-call arithmetic line (L17 `i = i * 2`) → exact (~9), lands on L18.
    let (mut dbg, info, _pid) = stopped_at(17);
    match dbg.step_over_or_count(4_096).unwrap() {
        LineInstrCount::Exact(n) => assert!(
            (4..=20).contains(&n),
            "no-call line should be exact (~9), got {n}",
        ),
        LineInstrCount::Capped(n) => panic!("no-call line should be Exact, got Capped({n})"),
    }
    assert_eq!(info.line.get(), Some(18), "exact step must land on the next line");
    dbg.continue_debugee().unwrap();

    // Call line (L40 `busy_loop(..)`) → falls back to step-over, lands on L41.
    let (mut dbg, info, _pid) = stopped_at(40);
    match dbg.step_over_or_count(4_096).unwrap() {
        LineInstrCount::Capped(_) => {}
        LineInstrCount::Exact(n) => panic!("call line should fall back, got Exact({n})"),
    }
    assert_eq!(
        info.line.get(),
        Some(41),
        "call-line step-over must land on the next line, not descend",
    );
    dbg.continue_debugee().unwrap();
}

/// `step_into_or_count` is the overlay's exact-count step-**in**: exact on a
/// no-call line (true count, lands on the next line) and — unlike
/// `step_over_or_count` — it DESCENDS into a call instead of stepping over it.
/// Regression for "step into `i = i + 1` shows ~45k": the step-in path had no
/// counting and fell back to the trap-floor rusage delta.
#[test]
#[serial]
fn step_into_or_count_exact_and_descends() {
    // No-call line L18 `i = i + 1` (checked add ~5) → exact, lands on L19.
    // This is the user-reported case: a step-in here is a handful, not ~45k.
    let (mut dbg, info, _pid) = stopped_at(18);
    match dbg.step_into_or_count(4_096).unwrap() {
        LineInstrCount::Exact(n) => assert!(
            (3..=20).contains(&n),
            "no-call `i = i + 1` step-in should be exact (~5), got {n} — \
             step-in must count instructions, not report the rusage ~45k",
        ),
        LineInstrCount::Capped(n) => panic!("no-call line should be Exact, got Capped({n})"),
    }
    assert_eq!(info.line.get(), Some(19), "exact step-in must land on the next line");
    dbg.continue_debugee().unwrap();

    // Call line L40 `busy_loop(black_box(..))` → step-in DESCENDS into the first
    // callee (here black_box, then busy_loop) rather than stepping over to L41.
    // That descent is what distinguishes step_into_or_count from
    // step_over_or_count, which would land on L41.
    let (mut dbg, info, _pid) = stopped_at(40);
    let _ = dbg.step_into_or_count(4_096).unwrap();
    let landed = info.line.get();
    assert_ne!(landed, Some(41), "step-in must not step OVER the call to L41");
    assert_ne!(landed, Some(40), "step-in must leave the call line");
    dbg.continue_debugee().unwrap();
}

/// The debug-build detector behind the "perf numbers are debug-inflated" banner.
#[test]
#[serial]
fn detects_debug_build() {
    let (mut dbg, _info, _pid) = stopped_at(17);
    assert!(
        dbg.is_likely_debug_build(),
        "perf_lines is built under target/debug — should be flagged as a debug build",
    );
    dbg.continue_debugee().unwrap();
}

/// Investigation harness for #3 (run: `cargo test --features perf --test debugger
/// investigate_step_cost -- --ignored --nocapture`). Decomposes the over-count
/// into per-trap components so we can see *where* the ~74k comes from:
///   • one `stepi`            → 1 trap, ~1 user instruction
///   • N consecutive `stepi`  → N traps (linearity check)
///   • one `continue`         → 1 trap, full-speed run of a near-empty line
/// If all land near the same per-trap figure, the cost is fixed kernel
/// exception-round-trip charged into `ri_instructions`, not debuggee user code.
#[test]
#[serial]
#[ignore = "investigation: prints a cost decomposition, no assertions"]
fn investigate_step_cost_decomposition() {
    // --- one stepi (single-instruction step at L17) ---
    let process = prepare_debugee_process(PERF_LINES_APP, &[]);
    let raw_pid = process.pid().as_raw();
    let info = TestInfo::default();
    let mut dbg = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", 17).unwrap();
    dbg.start_debugee().unwrap();

    println!("\n=== #3 step-cost decomposition (arm64, debug) ===");
    println!("{:<28} {:>12} {:>12}", "operation", "instr", "cycles");

    let b = ProcessSnapshot::capture(raw_pid).unwrap();
    dbg.stepi().unwrap();
    let d = ProcessSnapshot::capture(raw_pid).unwrap().delta_since(b);
    println!("{:<28} {:>12} {:>12}", "1x stepi @L17", d.instructions, d.cycles);

    // --- 5 consecutive stepi (linearity: does cost scale with traps?) ---
    for n in 1..=5 {
        let b = ProcessSnapshot::capture(raw_pid).unwrap();
        dbg.stepi().unwrap();
        let d = ProcessSnapshot::capture(raw_pid).unwrap().delta_since(b);
        println!("{:<28} {:>12} {:>12}", format!("  stepi #{n}"), d.instructions, d.cycles);
    }
    dbg.continue_debugee().unwrap();

    // --- one continue over a near-empty line (L16 black_box(3) -> L17) ---
    let process = prepare_debugee_process(PERF_LINES_APP, &[]);
    let raw_pid = process.pid().as_raw();
    let info = TestInfo::default();
    let mut dbg = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", 16).unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", 17).unwrap();
    dbg.start_debugee().unwrap();
    assert_eq!(info.line.get(), Some(16));
    let b = ProcessSnapshot::capture(raw_pid).unwrap();
    dbg.continue_debugee().unwrap(); // 1 trap, full-speed
    let d = ProcessSnapshot::capture(raw_pid).unwrap().delta_since(b);
    println!("{:<28} {:>12} {:>12}", "1x continue L16->L17", d.instructions, d.cycles);
    dbg.continue_debugee().unwrap();

    // --- one continue across a 1,000,000-iteration hot loop (L28 -> L35) ---
    // User work (~19M instr) should dwarf the ~70k per-trap floor, so the count
    // is accurate here: this is the regime where rusage is the right tool.
    let process = prepare_debugee_process(PERF_LINES_APP, &[]);
    let raw_pid = process.pid().as_raw();
    let info = TestInfo::default();
    let mut dbg = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()))
        .build(process)
        .unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", 28).unwrap();
    dbg.set_breakpoint_at_line("perf_lines.rs", 35).unwrap();
    dbg.start_debugee().unwrap();
    assert_eq!(info.line.get(), Some(28));
    let b = ProcessSnapshot::capture(raw_pid).unwrap();
    dbg.continue_debugee().unwrap();
    let d = ProcessSnapshot::capture(raw_pid).unwrap().delta_since(b);
    assert_eq!(info.line.get(), Some(35));
    println!("{:<28} {:>12} {:>12}", "1x continue 1e6-loop", d.instructions, d.cycles);
    println!("(line steps: ~5-8 user instr buried under ~35k/trap; loop: user work dominates)\n");
    dbg.continue_debugee().unwrap();
}

/// Validates the "double-trap" idea: a `step_over` of a no-call line is 2 traps,
/// so subtract a *trap-matched* reference (2× `stepi`, ~0 user work) measured
/// adjacent to the real step. `corrected = raw − floor` should collapse the
/// trivial line toward ~0 (honest: real work is negligible) while recovering
/// the medium/heavy lines accurately.
/// Run: `cargo test --features perf --test debugger investigate_floor_sub -- --ignored --nocapture`
#[test]
#[serial]
#[ignore = "investigation: validates floor subtraction, no assertions"]
fn investigate_floor_subtraction() {
    // True "double-trap": measure the 2-trap reference floor ADJACENT to the
    // real step, in the SAME process (warm, state-matched) — `step_over` then
    // 2× `stepi` from where we landed (≈0 user work). Returns (raw, floor).
    let measure = |line: u64| -> (u64, u64) {
        let (mut dbg, _i, pid) = stopped_at(line);
        let b = ProcessSnapshot::capture(pid).unwrap();
        dbg.step_over().unwrap();
        let raw = ProcessSnapshot::capture(pid).unwrap().delta_since(b).instructions;
        // adjacent floor — 2 traps' worth, same warm process
        let b2 = ProcessSnapshot::capture(pid).unwrap();
        dbg.stepi().unwrap();
        dbg.stepi().unwrap();
        let floor = ProcessSnapshot::capture(pid).unwrap().delta_since(b2).instructions;
        dbg.continue_debugee().unwrap();
        (raw, floor)
    };

    println!("\n=== double-trap (adjacent, same-process floor) validation ===");
    println!("{:<22} {:>12} {:>12} {:>12}", "line", "raw", "floor(2trap)", "corrected");
    for (label, line, real) in [
        ("i = i * 2 (L17)", 17u64, "~8"),
        ("busy_loop 2e3 (L41)", 41, "~38k"),
        ("busy_loop 1e6 (L40)", 40, "~19M"),
    ] {
        let (raw, floor) = measure(line);
        let corrected = raw.saturating_sub(floor);
        println!("{label:<22} {raw:>12} {floor:>12} {corrected:>12}  (real {real})");
    }
    println!();
}

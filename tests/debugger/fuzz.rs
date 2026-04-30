// SPDX-License-Identifier: MIT
//! Random-walk smoke fuzzer over the curated example debuggees.
//!
//! Each test seeds a deterministic xorshift RNG (override with
//! `BS_FUZZ_SEED=<u64>`), starts a debugger against one example,
//! sets a `main` breakpoint, runs to it, then drives a fixed
//! number of random debugger commands picked from
//! `step_into` / `step_over` / `step_out` /
//! `read_local_variables` / `read_arguments` /
//! `frame_information`. The oracle is weak by design — we only
//! check that *nothing panicked, deadlocked, or left an inferior
//! behind*. Errors from individual commands (e.g. step past the
//! end of `main`) are tolerated; we just walk on.
//!
//! Goal: surface the kind of crash a hand-written test will miss
//! — unusual command sequences, edge-case PCs (mid-prologue, in
//! a return slot, in a TLS thunk), interactions between
//! `read_variable` and the next step. If a fuzz iteration
//! reproduces, copy the seed in the panic message into a
//! regression test under one of the existing modules.
//!
//! Per-test wallclock budget is enforced by `serial_test` plus
//! the underlying `Drop` glue (debugger drop tears the inferior
//! down). On Darwin we run all of these `#[serial]` because the
//! parallel `task_for_pid` story is still a known-flake; the
//! suite-level codesign `flock` covers process-spawn races
//! independently.

use crate::common::{TestHooks, TestInfo};
use crate::{
    CALC_APP, CALLS_APP, FIZZBUZZ_APP, HW_APP, MT_APP, RECURSION_APP, VARS_APP,
    prepare_debugee_process,
};
use bugstalker::debugger::DebuggerBuilder;
use bugstalker::ui::command::parser::expression;
use chumsky::Parser;
use serial_test::serial;

/// Default iteration budget per fuzz test. ~20 random commands
/// is enough to cover all four step variants and a couple of
/// reads on every example without blowing out the suite's
/// runtime. Tune via `BS_FUZZ_STEPS` if hunting a specific
/// flake.
const DEFAULT_STEPS: u32 = 20;

/// xorshift64 — one-line PRNG, fully deterministic given the
/// seed. Adequate for picking among a handful of commands; not
/// suitable for cryptography (we don't need it).
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // xorshift64 misbehaves on a zero state; remap deterministically.
        Self(if seed == 0 { 0xDEAD_BEEF_CAFE_BABE } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() as usize) % items.len()]
    }
}

#[derive(Debug, Clone, Copy)]
enum Cmd {
    StepInto,
    StepOver,
    StepOut,
    ReadLocals,
    ReadArguments,
    FrameInfo,
}

/// Seed override. Defaults to `0xBADC0FFEE0DDF00D` — keep
/// reproducible across machines unless the env var overrides.
fn seed() -> u64 {
    std::env::var("BS_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xBADC_0FFE_E0DD_F00D)
}

fn step_budget() -> u32 {
    std::env::var("BS_FUZZ_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_STEPS)
}

/// Run a random walk against `app`. The walk is deliberately
/// lenient: any debugger-API `Err` (step past end of program,
/// read-variable on a non-existent name, etc.) terminates the
/// walk and the test still passes — the only failure modes are
/// `panic!`, deadlock (the test framework's wallclock will catch
/// it), or `unwrap`/`expect` blowing up inside our crate.
fn fuzz_walk(app: &str, args: &[&'static str], local_seed: u64) {
    const COMMANDS: &[Cmd] = &[
        Cmd::StepInto,
        Cmd::StepOver,
        Cmd::StepOut,
        Cmd::ReadLocals,
        Cmd::ReadArguments,
        Cmd::FrameInfo,
    ];

    let process = prepare_debugee_process(app, args);
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    if debugger.set_breakpoint_at_fn("main").is_err() {
        // Some examples (libcalc_lib, etc.) might not expose a
        // `main` symbol; nothing to fuzz, exit cleanly.
        return;
    }
    if debugger.start_debugee().is_err() {
        return;
    }

    let mut rng = XorShift::new(local_seed);
    let mut consecutive_errors = 0u32;
    for step in 0..step_budget() {
        let cmd = *rng.pick(COMMANDS);
        let result: Result<(), String> = match cmd {
            Cmd::StepInto => debugger.step_into().map(|_| ()).map_err(|e| format!("{e}")),
            Cmd::StepOver => debugger.step_over().map(|_| ()).map_err(|e| format!("{e}")),
            Cmd::StepOut => debugger.step_out().map(|_| ()).map_err(|e| format!("{e}")),
            Cmd::ReadLocals => debugger
                .read_local_variables()
                .map(|_| ())
                .map_err(|e| format!("{e}")),
            Cmd::ReadArguments => {
                let parser = expression::parser();
                // Probe a handful of common argument names; we
                // don't care if any individual lookup misses.
                for name in ["a", "b", "x", "y", "i", "n", "self"] {
                    if let Some(expr) = parser.parse(name).into_result().ok() {
                        let _ = debugger.read_argument(expr);
                    }
                }
                Ok(())
            }
            Cmd::FrameInfo => debugger
                .frame_info()
                .map(|_| ())
                .map_err(|e| format!("{e}")),
        };
        if let Err(e) = result {
            consecutive_errors += 1;
            // Three errors in a row almost always means the
            // inferior has exited or got stuck somewhere that's
            // not interesting to fuzz further. Break and let
            // the drop glue tear it down.
            if consecutive_errors >= 3 {
                eprintln!(
                    "[fuzz {app}] step {step} ({cmd:?}) and 2 prior errors; \
                     stopping walk. last err: {e}"
                );
                break;
            }
        } else {
            consecutive_errors = 0;
        }
    }
    // `debugger` drops here → inferior gets killed via the Drop
    // impl. The test passes if we got here without a panic.
}

#[test]
#[serial]
fn fuzz_hello_world() {
    fuzz_walk(HW_APP, &[], seed());
}

#[test]
#[serial]
fn fuzz_calc() {
    fuzz_walk(
        CALC_APP,
        &["1", "2", "3", "--description", "fuzz-result"],
        seed(),
    );
}

#[test]
#[serial]
fn fuzz_vars() {
    fuzz_walk(VARS_APP, &[], seed());
}

#[test]
#[serial]
fn fuzz_recursion() {
    fuzz_walk(RECURSION_APP, &[], seed());
}

#[test]
#[serial]
fn fuzz_fizzbuzz() {
    fuzz_walk(FIZZBUZZ_APP, &[], seed());
}

#[test]
#[serial]
fn fuzz_mt() {
    fuzz_walk(MT_APP, &[], seed());
}

#[test]
#[serial]
fn fuzz_calls() {
    fuzz_walk(CALLS_APP, &[], seed());
}

/// Cross-seed regression net — runs the small `hello_world`
/// example with three different seeds in one test so the suite
/// occasionally flushes out a flake that seeded #1 happens to
/// avoid. Cheap (HW exits in milliseconds).
#[test]
#[serial]
fn fuzz_hello_world_multi_seed() {
    for s in [
        0x1234_5678_9ABC_DEF0,
        0xFEDC_BA98_7654_3210,
        0x0F0F_0F0F_F0F0_F0F0,
    ] {
        fuzz_walk(HW_APP, &[], s);
    }
}

/// Random-example sweep. Each round picks (example, seed) from
/// the seeded RNG, prints the choice up-front, and runs the
/// walk. Default 5 rounds; tune via `BS_FUZZ_ROUNDS`. The
/// printed `(example, seed)` lines make any panic
/// reproducible — set `BS_FUZZ_SEED=<u64>` (and
/// `BS_FUZZ_ROUNDS=N`) to replay exactly the same sequence.
///
/// Pool entries are `(label, path, args)`. Examples that need
/// args (e.g. `calc`) provide them; the rest take `&[]`.
#[test]
#[serial]
fn fuzz_random_examples() {
    let pool: &[(&str, &str, &[&'static str])] = &[
        ("hello_world", HW_APP, &[]),
        (
            "calc",
            CALC_APP,
            &["1", "2", "3", "--description", "fuzz-result"],
        ),
        ("vars", VARS_APP, &[]),
        ("recursion", RECURSION_APP, &[]),
        ("fizzbuzz", FIZZBUZZ_APP, &[]),
        ("mt", MT_APP, &[]),
        ("calls", CALLS_APP, &[]),
    ];

    let rounds = std::env::var("BS_FUZZ_ROUNDS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(5);

    let mut rng = XorShift::new(seed());
    for round in 0..rounds {
        let (label, path, args) = rng.pick(pool);
        let walk_seed = rng.next_u64();
        eprintln!(
            "[fuzz round {round}/{rounds}] example={label} seed={walk_seed:#018x} \
             (replay: BS_FUZZ_SEED={:#018x} BS_FUZZ_ROUNDS={rounds})",
            seed(),
        );
        fuzz_walk(path, args, walk_seed);
    }
}

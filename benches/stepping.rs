// SPDX-License-Identifier: MIT
//! Stepping-latency bench. Times a single `step_over` from a known
//! breakpoint, re-spawning the debuggee per iteration (the spawn is
//! batched setup, not timed).
//!
//! What this measures: the cost of advancing the inferior one source
//! line — the debugger's step machinery — never a debuggee workload
//! (design-principles.md §1). It deliberately does *not* read statics
//! or build any variables view: per design-principles.md §2 the step
//! path must stay clear of expensive file-scope enumeration. The number
//! here is the floor a step should cost; if it ever drifts up toward
//! `statics_enumerate`'s `all_crates` figure, an eager statics read has
//! leaked back into the per-stop path and stepping has regressed for
//! anyone debugging a static-heavy binary.
//!
//! Debuggee: `vars`. Compare via `cargo bench --bench stepping`.

use bs_test_harness::spawn_at_breakpoint;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

const VARS_LINE: u64 = 749;

fn vars_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/target/debug/vars")
}

fn bench_step_over(c: &mut Criterion) {
    let prog = vars_path();
    if !prog.exists() {
        panic!(
            "vars debuggee not found at {} — run `cargo build -p vars` under examples/ first",
            prog.display()
        );
    }
    let mut group = c.benchmark_group("stepping");
    // Each iteration spawns + attaches a fresh session in (untimed)
    // setup, so keep the count low — the attach dominates wall-clock.
    group.sample_size(10);
    group.bench_function("step_over_vars", |b| {
        b.iter_batched(
            || spawn_at_breakpoint(&prog, "vars.rs", VARS_LINE),
            |mut dbg| {
                // One source-line step. That's the whole timed body —
                // no scope/variable reads (see module docs).
                let _ = dbg.step_over();
                black_box(())
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_step_over);
criterion_main!(benches);

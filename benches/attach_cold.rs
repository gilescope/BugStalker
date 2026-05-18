// SPDX-License-Identifier: MIT
//! Cold-attach bench. Each iteration spawns the `examples/hello_world`
//! debuggee, installs a `bugstalker::Debugger`, sets a breakpoint at
//! line 5 (where `let world = "world";` is in scope), runs the
//! debuggee until the breakpoint hits, and drops the debugger. The
//! whole sequence is timed.
//!
//! What this measures: the cold path the user sees when they invoke
//! `bs <binary>` for the first time — process spawn, ptrace/Mach
//! attach, DWARF parse, register read, breakpoint install, run, hit
//! the trap, decode the place. Memory caches reset per iteration
//! (process-scoped); on-disk caches are warm after the first run.
//!
//! Why hello_world: smallest debug-info-bearing fixture in
//! `examples/`, so the bench captures attach overhead rather than
//! DWARF-parse-of-large-binary cost. A larger-binary variant lands
//! in Phase 8 alongside the proper soak / regression suite.
//!
//! Compare via `cargo bench --bench attach_cold`. The `+smoke`
//! Earthly target runs `--quick` which still produces a usable
//! median for change-detection across PRs.

use bs_test_harness::spawn_at_breakpoint;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

fn hello_world_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/target/debug/hello_world")
}

fn bench_attach_cold(c: &mut Criterion) {
    let prog = hello_world_path();
    if !prog.exists() {
        panic!(
            "hello_world debuggee not found at {} — run `cargo build -p hello_world` under examples/ first",
            prog.display()
        );
    }
    // Long iterations — attach + DWARF parse + first BP is hundreds of
    // ms even on the smallest fixture. Reduce sample size so the bench
    // completes in a reasonable wall-clock budget; criterion still
    // produces a usable median.
    let mut group = c.benchmark_group("attach_cold");
    group.sample_size(20);
    group.bench_function("hello_world", |b| {
        b.iter(|| {
            let debugger = spawn_at_breakpoint(black_box(&prog), "hello_world.rs", 5);
            // Drop releases the inferior; that's part of the cold-path
            // cost the user pays per session.
            drop(debugger);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_attach_cold);
criterion_main!(benches);

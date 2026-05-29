// SPDX-License-Identifier: MIT
//! Statics-enumeration bench. Times `Debugger::read_static_variables`
//! — the file-scope walk that reads every matching static's value out
//! of the inferior — across the three `FileScopeFilter` breadths.
//!
//! What this measures: the *debugger's* own cost of materialising the
//! "Statics" view, never a debuggee workload (design-principles.md §1).
//! `all_crates` is the worst case the user hits when the current crate
//! can't be resolved (e.g. a test binary), where the pane falls back to
//! every crate's statics — std plus every dependency. That is precisely
//! the cost that must NOT be paid on every step (design-principles.md
//! §2); the `stepping` bench guards the step path against it, and this
//! one quantifies what we're deferring.
//!
//! Debuggee: `statics_heavy` — 4000 statics across 40 nested modules
//! (`build.rs` generates them), so the numbers scale the way they do on
//! a real dependency-rich binary, not the handful a tiny fixture like
//! `vars` carries.
//!
//! Compare via `cargo bench --bench statics_enumerate`.

use bs_test_harness::spawn_at_breakpoint;
use bugstalker::debugger::variable::execute::FileScopeFilter;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

/// Breakpoint line in `statics_heavy/src/main.rs` (the `println!`) —
/// every generated module static is in scope and the frame resolves to
/// the `statics_heavy` crate so `current_crate` matches all 4000.
const BP_LINE: u64 = 13;

fn debuggee_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/target/debug/statics_heavy")
}

fn bench_statics_enumerate(c: &mut Criterion) {
    let prog = debuggee_path();
    if !prog.exists() {
        panic!(
            "statics_heavy debuggee not found at {} — run `cargo build -p statics_heavy` under examples/ first",
            prog.display()
        );
    }
    // Setup-once: a single stopped session, reused read-only across all
    // timed iterations (the read is non-mutating).
    let debugger = spawn_at_breakpoint(&prog, "main.rs", BP_LINE);

    let mut group = c.benchmark_group("statics_enumerate");
    // Each iteration reads inferior memory for every matching static;
    // `all_crates` is heavy, so keep the sample count modest.
    group.sample_size(20);
    for (label, filter) in [
        ("current_crate", FileScopeFilter::CurrentCrate),
        ("current_unit", FileScopeFilter::CurrentUnit),
        ("all_crates", FileScopeFilter::All),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| {
                let v = debugger
                    .read_static_variables(black_box(filter))
                    .unwrap_or_default();
                black_box(v.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_statics_enumerate);
criterion_main!(benches);

// SPDX-License-Identifier: MIT
//! Phase 1 hot-path bench. Drives the `examples/vars` debuggee
//! through `bs-test-harness`, captures every local variable at the
//! `phase1_specs_b()` breakpoint, drops the debugger, then times
//! `render_value()` over the captured `Value` set per iteration.
//!
//! What this measures: the cost of rendering a varied bag of stdlib
//! types (Pin, Range×4, Duration×4, CString×3, OsString, PathBuf,
//! MaybeUninit, Mutex, RwLock, MutexGuard, RwLockReadGuard, &CStr,
//! &OsStr, &Path, plus assorted scalars and ZSTs that share the
//! breakpoint scope). It does NOT measure debugger setup or DWARF
//! parsing — that's `attach_cold`.
//!
//! How to compare: `cargo bench --bench render_value` saves a
//! baseline in `target/criterion/`. Subsequent runs report change
//! vs the previous baseline; `Performance has regressed` is the
//! signal `+smoke` (and Phase 8's nightly CI) will gate on.

use bs_test_harness::{capture_locals, spawn_at_breakpoint};
use bugstalker::ui::generic::variable::render_value;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

/// Source line in `examples/vars/src/vars.rs` where every Phase 1
/// fixture is in scope. Same anchor `bs-smoke` uses.
const VARS_LINE: u64 = 749;

fn vars_path() -> PathBuf {
    // CARGO_MANIFEST_DIR points at the workspace root for benches
    // declared in the root Cargo.toml.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/target/debug/vars")
}

fn bench_render_phase1(c: &mut Criterion) {
    // Setup-once: spawn the debugger, capture every local, drop the
    // debugger before timed iterations so we measure only the
    // renderer. `Value` carries `*const ()` raw pointers (not `Sync`)
    // so we can't park the snapshot in a `OnceLock`; closure capture
    // is fine — criterion calls the outer closure once.
    let prog = vars_path();
    if !prog.exists() {
        panic!(
            "vars debuggee not found at {} — run `cargo build -p vars` under examples/ first",
            prog.display()
        );
    }
    let debugger = spawn_at_breakpoint(&prog, "vars.rs", VARS_LINE);
    let values = capture_locals(&debugger);
    drop(debugger);

    c.bench_function("render_phase1_locals", |b| {
        b.iter(|| {
            let mut total_len: usize = 0;
            for (_, v) in &values {
                total_len += render_value(black_box(v)).len();
            }
            black_box(total_len)
        });
    });
}

criterion_group!(benches, bench_render_phase1);
criterion_main!(benches);

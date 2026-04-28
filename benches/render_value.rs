// SPDX-License-Identifier: MIT
//! Phase 0 placeholder bench. Phase 1+ replaces the body with a real
//! "render N varied stdlib values" pass against a fixture debuggee.
//! For now this exists so `cargo bench` produces a Criterion report
//! and the per-bench baseline file gets seeded.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn render_value_placeholder(c: &mut Criterion) {
    c.bench_function("render_value_placeholder", |b| {
        b.iter(|| {
            // Stand-in workload: format a couple of values. Replace
            // with the real renderer in Phase 1 once the workspace
            // crate split lands.
            let n: u64 = black_box(1234567890);
            let s = format!("{n:#x} {n}");
            black_box(s.len())
        });
    });
}

criterion_group!(benches, render_value_placeholder);
criterion_main!(benches);

// SPDX-License-Identifier: MIT
//! Phase 0 placeholder bench. Phase 1+ replaces the body with a real
//! "attach to a 100 MB binary, time to first usable variable" pass.
//! For now this exists so `cargo bench` produces a Criterion report
//! and the per-bench baseline file gets seeded.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn attach_cold_placeholder(c: &mut Criterion) {
    c.bench_function("attach_cold_placeholder", |b| {
        b.iter(|| {
            // Stand-in workload: a tiny CPU-bound loop. Replaced in
            // Phase 1 with `bs::attach()` on a fixture binary.
            let mut acc: u64 = 0;
            for i in 0u64..1024 {
                acc = acc.wrapping_add(black_box(i).wrapping_mul(0x9E3779B97F4A7C15));
            }
            black_box(acc)
        });
    });
}

criterion_group!(benches, attach_cold_placeholder);
criterion_main!(benches);

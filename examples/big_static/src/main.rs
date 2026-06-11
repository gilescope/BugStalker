// Fixture for the Statics size-gate: a giant static (800 KB) standing in
// for a precomputed crypto table. The Statics pane must show it lazily
// (read on expand), not materialise all 100 000 elements on open. Plus a
// small static that must still render inline. Asserted by
// `tests/dap/dap_integration.rs::test_statics_giant_static_is_lazy`.

#[used]
static BIG_TABLE: [u64; 100_000] = [0; 100_000];

#[used]
static SMALL: u64 = 42;

fn main() {
    // Touch both so neither is stripped, and give a line to break on.
    println!("{} {}", BIG_TABLE[0], SMALL); // breakpoint line 15
}

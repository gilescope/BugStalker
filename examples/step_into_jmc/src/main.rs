// Fixture for Phase 12 — Step-Into "just my code".
//
// `main` has two interesting lines:
//   (a) an entirely-library call (`str::to_uppercase` → alloc/core);
//   (b) a library call that invokes a *user* closure
//       (`iter().map(user_fn).sum()`).
//
// Line numbers are asserted by the DAP integration test
// `tests/dap/dap_integration.rs`; keep `user_fn`, `LINE A`, and `LINE B`
// where they are (or update the constants there if you move them).

// Invoked as a closure callback by the iterator adaptor on line (b).
// `#[inline(never)]` so it owns a real frame the debugger can stop in.
#[inline(never)]
fn user_fn(x: &u64) -> u64 {
    x.wrapping_mul(2)
}

fn main() {
    // (a) entirely library: SkipLibraries must step OVER this and stop
    //     on the next user line, never inside alloc/core.
    let shout = "hi".to_uppercase(); // LINE A

    // (b) library call invoking a user closure: the full SkipLibraries
    //     engine must stop in `user_fn`; the MVP steps over it.
    let total: u64 = [1u64, 2, 3].iter().map(user_fn).sum(); // LINE B

    println!("{shout} {total}");
}

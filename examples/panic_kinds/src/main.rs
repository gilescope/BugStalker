// Fixture for break-on-panic location recovery. Each panic flavor lives
// in its own `#[inline(never)]` fn so its panic-site line is stable, and
// routes through a different panic entry point (`panic_fmt`,
// `panic_bounds_check`, …) so the `&Location` register-scan is exercised
// across all the traps. The exact `panic!`/call lines are asserted by
// `tests/debugger/panic_location.rs`; keep them where the constants say.
use std::hint::black_box;

fn main() {
    let kind = std::env::args().nth(1).unwrap_or_default();
    match kind.as_str() {
        "str" => panic_str(),
        "fmt" => panic_fmt(),
        "unwrap" => panic_unwrap(),
        "expect" => panic_expect(),
        "index" => panic_index(),
        "assert" => panic_assert(),
        _ => println!("no panic"),
    }
}

#[inline(never)]
fn panic_str() {
    panic!("boom"); // PANIC_STR_LINE
}

#[inline(never)]
fn panic_fmt() {
    let n = black_box(7);
    panic!("value was {n}"); // PANIC_FMT_LINE
}

#[inline(never)]
fn panic_unwrap() {
    let o: Option<i32> = black_box(None);
    o.unwrap(); // PANIC_UNWRAP_LINE
}

#[inline(never)]
fn panic_expect() {
    let o: Option<i32> = black_box(None);
    o.expect("expected a value"); // PANIC_EXPECT_LINE
}

#[inline(never)]
fn panic_index() {
    let v = [1, 2, 3];
    let i = black_box(9);
    println!("{}", v[i]); // PANIC_INDEX_LINE
}

#[inline(never)]
fn panic_assert() {
    let a = black_box(1);
    assert_eq!(a, 2); // PANIC_ASSERT_LINE
}

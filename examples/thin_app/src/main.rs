//! Thin binary with no statics of its own, depending on a crate that
//! has one. Exercises the all-crates Statics default: the binary's own
//! scope is empty, yet `dep_with_static::DEP_STATIC` must appear.
fn main() {
    println!("{}", dep_with_static::DEP_STATIC); // breakpoint line 5
}

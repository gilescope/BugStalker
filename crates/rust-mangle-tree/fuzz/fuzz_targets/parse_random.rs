// SPDX-License-Identifier: MIT OR Apache-2.0
#![no_main]
//! libFuzzer target: feed arbitrary bytes to `rust_mangle_tree::parse`
//! and treat any panic as a test failure. The parser's no-panic
//! guarantee is the headline invariant; this target enforces it
//! across millions of iterations on nightly.
//!
//! Run via: `cargo +nightly fuzz run parse_random`. Phase 8's
//! `ci-fuzz.yml` runs a short session on every PR; the long
//! soak runs nightly.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = core::str::from_utf8(data) {
        let _ = rust_mangle_tree::parse(s);
    }
});

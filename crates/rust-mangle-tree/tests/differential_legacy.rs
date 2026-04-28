// SPDX-License-Identifier: MIT OR Apache-2.0
//! Differential test: every legacy symbol in our small corpus must
//! demangle to exactly the same `Display` string `rustc-demangle`
//! produces. Catches escape-table drift, hash-detection drift, and
//! any future divergence in the segment join.
//!
//! Phase 2 batch B scope. Batch C+ will add a corresponding v0
//! differential.

use rust_mangle_tree::{Symbol, parse};

/// Each row: a real `_ZN…E` symbol that an actual rustc-built binary
/// emits for the indicated source path. Pulled from `nm` over a
/// `cargo build` artifact and a couple of synthetic ones for the
/// escape table.
const CORPUS: &[&str] = &[
    // Bare two-segment.
    "_ZN3foo3barE",
    // With the standard 17-byte trailing hash.
    "_ZN3foo3bar17h0123456789abcdefE",
    // The double-underscore prefix some toolchains use.
    "__ZN3foo3barE",
    // An identifier with the source-`<…>` escape pair.
    "_ZN15foo$LT$ibar$GT$E",
    // An identifier carrying the source `::` mangled as `..`.
    "_ZN4a..bE",
    // Unicode escape (space).
    "_ZN7a$u20$bE",
    // Three-segment with hash — typical rustc output.
    "_ZN4core3fmt5write17h0123456789abcdefE",
    // Crate name with a digit.
    "_ZN8nom_v7_a3lexE",
];

#[test]
fn matches_rustc_demangle_byte_for_byte() {
    let mut mismatches: Vec<String> = Vec::new();
    for sym in CORPUS {
        let theirs = format!("{:#}", rustc_demangle::demangle(sym));
        let ours = match parse(sym) {
            Ok(Symbol::Legacy(p)) => format!("{p}"),
            Ok(other) => panic!("{sym}: expected Legacy, got {other:?}"),
            Err(e) => panic!("{sym}: parse failed: {e}"),
        };
        if theirs != ours {
            mismatches.push(format!("input={sym}\n  ours    = {ours:?}\n  theirs  = {theirs:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn no_panic_on_garbage() {
    // Strings that look-but-aren't a legacy symbol must not panic.
    // They either parse with surprising-but-valid output, or return
    // `Err`. Either is acceptable — panic isn't.
    for input in [
        "_ZN", "_ZN0E", "_ZNXE", "_ZN999999999E", "_ZN3", "_ZNzzzE", "_ZN3foo",
    ] {
        let _ = parse(input); // must not panic
    }
}

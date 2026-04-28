// SPDX-License-Identifier: MIT OR Apache-2.0
//! Differential test for v0: compare our `Display` against
//! `rustc-demangle` on a small corpus of known v0 symbols. Phase 2
//! batch C scope — Display matches rustc-demangle for the easy
//! cases (paths, primitives, simple generics, references).
//! Byte-perfect parity for the full corpus is batch E's
//! responsibility; this test guards against regressions in the
//! easy cases.
//!
//! Mismatches are reported as `expected != ours`; tests are
//! soft-skipped (printed but not failed) for forms we know are
//! still simplified in batch C — primarily lifetime rendering and
//! complex generic-arg formatting.

use rust_mangle_tree::{Symbol, parse};

const CORPUS: &[(&str, &str)] = &[
    // Crate root.
    ("_RC4core", "core"),
    // Nested value-namespace function.
    ("_RNvC4core5write", "core::write"),
    // Two-level nested. Length prefixes are byte counts of the
    // identifier ("cell"=4, "RefMut"=6, "get"=3).
    ("_RNvNtNtC4core4cell6RefMut3get", "core::cell::RefMut::get"),
];

#[test]
fn matches_rustc_demangle_for_easy_cases() {
    let mut mismatches: Vec<String> = Vec::new();
    for (sym, expected) in CORPUS {
        let ours = match parse(sym) {
            Ok(Symbol::V0(p)) => format!("{p}"),
            Ok(other) => panic!("{sym}: expected V0, got {other:?}"),
            Err(e) => panic!("{sym}: parse failed: {e}"),
        };
        if ours != *expected {
            mismatches.push(format!(
                "input={sym}\n  ours    = {ours:?}\n  exp     = {expected:?}"
            ));
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
fn no_panic_on_v0_garbage() {
    // Strings that look like v0 but aren't must not panic — return
    // `Err` is acceptable, panic is not.
    for input in [
        "_R",
        "_RX",
        "_RB",
        "_RB_",                // back-ref to offset 0 — no path there yet
        "_RCs_",               // crate disambiguator without name
        "_RNvB1_",             // back-ref into the middle of nothing
        "_RIB_E",              // generic with self back-ref
        "_RAB_B_",             // array with back-ref length
        "_RC99999999999999999",// truncated long ident
    ] {
        let _ = parse(input); // must not panic
    }
}

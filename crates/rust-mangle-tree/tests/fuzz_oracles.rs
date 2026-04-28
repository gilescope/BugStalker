// SPDX-License-Identifier: MIT OR Apache-2.0
//! Property tests over random inputs. The headline invariant of
//! `rust-mangle-tree::parse` is that it must never panic — every
//! byte string must come back as either `Ok(Symbol::*)` or
//! `Err(ParseError)`.
//!
//! cargo-fuzz proper (libFuzzer) needs nightly Rust; these
//! `proptest` cases are the stable-Rust equivalent and run on every
//! `cargo test`. Phase 8 wires up the real `cargo fuzz` flow in
//! `ci-fuzz.yml` for the long-running coverage; this file catches
//! anything quick-to-find at PR time.

use proptest::prelude::*;
use rust_mangle_tree::parse;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        // Shrinking takes too long on large random byte strings; we
        // just want the panic-free guarantee, so disable shrinking
        // and trust the failing case as-is.
        max_shrink_iters: 0,
        ..ProptestConfig::default()
    })]

    /// Arbitrary UTF-8 strings up to 256 bytes. Covers the two
    /// recognised prefixes (`_R…`, `_ZN…`) plus the common
    /// `Symbol::NotRust(s)` case.
    #[test]
    fn parse_never_panics_on_utf8(s in "\\PC{0,256}") {
        let _ = parse(&s);
    }

    /// Arbitrary bytes interpreted as utf-8 if possible. Catches
    /// inputs that the v0 parser shouldn't see (non-ASCII bytes
    /// can't appear in a valid v0 symbol) but should still
    /// reject without panicking.
    #[test]
    fn parse_never_panics_on_lossy_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let s = String::from_utf8_lossy(&bytes);
        let _ = parse(&s);
    }

    /// Inputs that intentionally start with `_R` to exercise the v0
    /// parser specifically. Most will fail to parse; none should
    /// panic.
    #[test]
    fn v0_prefixed_never_panics(body in "[a-zA-Z0-9_]{0,128}") {
        let mut s = String::with_capacity(body.len() + 2);
        s.push_str("_R");
        s.push_str(&body);
        let _ = parse(&s);
    }

    /// Inputs that intentionally start with `_ZN` to exercise the
    /// legacy parser. Same panic-free guarantee.
    #[test]
    fn legacy_prefixed_never_panics(body in "[a-zA-Z0-9._$]{0,128}") {
        let mut s = String::with_capacity(body.len() + 4);
        s.push_str("_ZN");
        s.push_str(&body);
        s.push('E');
        let _ = parse(&s);
    }

    /// Inputs that look like back-reference torture: a `_R` prefix
    /// followed by repeated `B<digits>_`. The v0 single-pass
    /// guarantee says back-refs always point earlier than the
    /// current position, so these must fail (not panic) on
    /// invalid offsets.
    #[test]
    fn v0_backref_torture(digits in proptest::collection::vec("[0-9]{1,5}", 0..32)) {
        let mut s = String::from("_R");
        for d in digits {
            s.push('B');
            s.push_str(&d);
            s.push('_');
        }
        let _ = parse(&s);
    }
}

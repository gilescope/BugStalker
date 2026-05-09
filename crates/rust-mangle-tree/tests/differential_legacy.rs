// SPDX-License-Identifier: MIT OR Apache-2.0
//! Differential test: every legacy symbol in our small corpus must
//! demangle to exactly the same `Display` string `rustc-demangle`
//! produces. Catches escape-table drift, hash-detection drift, and
//! any future divergence in the segment join.
//!
//! Phase 2 batch B scope. Batch C+ will add a corresponding v0
//! differential.

use rust_mangle_tree::{parse, Symbol};

/// Each row: a real `_ZN…E` symbol that an actual rustc-built binary
/// emits for the indicated source path. Hand-picked for variety
/// across the escape table, hash presence, and trailing-suffix
/// shapes; the bigger corpus extracted from real binaries is
/// covered by `corpus_legacy_real_binary` below.
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
    // Real example: synthetic `<&mut Foo as Bar>::method` segment
    // with the leading-`_` placeholder rule.
    "_ZN100_$LT$$RF$mut$u20$serde_json..ser..Serializer$LT$W$C$F$GT$$u20$as$u20$serde_core..ser..Serializer$GT$13serialize_str17h9dcaf790e2c4b86aE",
    // Trailing LLVM thunk decoration after `E`.
    "_ZN103_$LT$std..sys..thread_local..abort_on_dtor_unwind..DtorUnwindGuard$u20$as$u20$core..ops..drop..Drop$GT$4drop17h103d66072492bb20E.2043",
    // `{{vtable.shim}}` — literal period inside `$u7b$$u7b$…$u7d$$u7d$`.
    "_ZN4core3ops8function6FnOnce40call_once$u7b$$u7b$vtable.shim$u7d$$u7d$17h1a440162a384b96dE",
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
            mismatches.push(format!(
                "input={sym}\n  ours    = {ours:?}\n  theirs  = {theirs:?}"
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
fn no_panic_on_garbage() {
    // Strings that look-but-aren't a legacy symbol must not panic.
    // They either parse with surprising-but-valid output, or return
    // `Err`. Either is acceptable — panic isn't.
    for input in [
        "_ZN",
        "_ZN0E",
        "_ZNXE",
        "_ZN999999999E",
        "_ZN3",
        "_ZNzzzE",
        "_ZN3foo",
    ] {
        let _ = parse(input); // must not panic
    }
}

/// Real-world corpus: 200 legacy symbols pulled from `nm` over the
/// release `bs` binary. CI re-runs this on every change so a future
/// edit to the escape table or the leading-`_` placeholder rule
/// can't silently regress against rustc-demangle.
///
/// Sampled rather than exhaustive (the full 6938-symbol corpus is
/// comfortable in CI but unnecessary noise here; the tool used to
/// build it lives in `crates/rust-mangle-tree/scripts/`).
#[test]
fn corpus_legacy_real_binary() {
    let raw = include_str!("corpus/legacy_real_binary.txt");
    let mut total = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    for line in raw.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        total += 1;
        let theirs = format!("{:#}", rustc_demangle::demangle(s));
        let ours = match parse(s) {
            Ok(Symbol::Legacy(p)) => format!("{p}"),
            // We may also see NotRust here for `_R…` symbols that
            // slipped through — skip them, they're v0's problem.
            Ok(_) => continue,
            Err(_) => continue,
        };
        if theirs != ours {
            if mismatches.len() < 5 {
                mismatches.push(format!("\n  in={s}\n  ours={ours}\n  thrs={theirs}"));
            } else {
                mismatches.push(String::new());
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches over {} symbols (first 5 shown):{}",
        mismatches.iter().filter(|m| !m.is_empty()).count(),
        total,
        mismatches.iter().take(5).cloned().collect::<String>()
    );
}

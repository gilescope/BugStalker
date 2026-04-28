<!-- markdownlint-disable MD041 -->
# rust-mangle-tree

Parsed AST for Rust v0 (RFC 2603) and legacy mangled symbols.

`rustc-demangle` exposes `Display` only. This crate exposes the
**tree** so debuggers, profilers, and any tool that needs
programmatic access to a symbol's structure can inspect generic
args, impl self-type, and closure coordinates without re-parsing
the rendered string.

## Status

Phase 2 in [`BugStalker`](https://github.com/godzie44/BugStalker)'s
roadmap. Current progress:

| Batch | Scope                                         | Status     |
| :---- | :-------------------------------------------- | :--------- |
| A     | crate skeleton, public types, stub parser     | ✅ landed  |
| B     | legacy Itanium-style `_ZN…E` parser           | ✅ landed  |
| C     | v0 grammar core (paths, segments, generics)   | ⏳ pending |
| D     | v0 Punycode + back-references                 | ⏳ pending |
| E     | Display impls byte-matching `rustc-demangle`  | ⏳ pending |
| F     | differential corpus from real binaries        | ⏳ pending |
| G     | `cargo fuzz` target                           | ⏳ pending |
| H     | BugStalker integration (3 call sites)         | ⏳ pending |

Until batch C lands, `parse()` returns `Err(Syntax)` for v0 inputs
and `Ok(Symbol::NotRust(…))` for everything else.

## Quick start

```rust
use rust_mangle_tree::{parse, Symbol};

match parse("_ZN3foo3bar17h0123456789abcdefE") {
    Ok(Symbol::Legacy(path)) => {
        // Walk the segments, get the trailing per-mono hash.
        for seg in path.segments() { println!("{seg}"); }
        println!("hash = {:?}", path.hash());
    }
    Ok(Symbol::V0(path))    => { let _ = path; /* once batch C lands */ }
    Ok(Symbol::NotRust(s))  => println!("not a Rust symbol: {s}"),
    Err(e)                  => eprintln!("parse error: {e}"),
}
```

## Crate features

| Feature | Default | Effect                                                |
| :------ | :-----: | :---------------------------------------------------- |
| `std`   |   yes   | implements `std::error::Error` for `ParseError`.      |

`no_std + alloc` build: `default-features = false`. The AST itself
needs `alloc` for `Vec`, `Box`, and `String`.

## Why a new crate

Long answer in
[`BugStalker/doc/plans/phase-2-rust-mangle-tree.md`](../../doc/plans/phase-2-rust-mangle-tree.md).
Short answer: the BugStalker debugger needs `impl_self_type()`
to recover concrete types behind `dyn Trait`, the closure
coordinates to attribute async frames, and the namespace tree to
build symbol-search hierarchies. None of this is possible against
a `Display`-only API; a parsed AST is the natural fit and is
useful enough that other tools (`samply`, `addr2line`, `inferno`)
are plausible downstream consumers.

## License

Dual-licensed under `MIT OR Apache-2.0`.

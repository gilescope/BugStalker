# Phase 2 — `rust-mangle-tree`

A standalone workspace crate that parses Rust v0 (RFC 2603) and legacy
Itanium-style mangled symbols into a borrowed AST. Unlocks Phase 3
features (vtable-driven `dyn Trait` recovery, async frame attribution).

## Why a new crate

`rustc-demangle` exposes `Display` only — no parsed tree, no programmatic
access to generic args, impl self-type, or closure coordinates. We need
the tree. Vendoring `rust-demangle.c` is the wrong direction; a clean
Rust crate is more useful, more testable, and publishable to crates.io
where `samply`, `addr2line`, `inferno`, and `cargo-asm` are plausible
downstream consumers.

## Crate identity

- **Name**: `rust-mangle-tree`
- **Location**: `crates/rust-mangle-tree/` (new top-level workspace
  member)
- **Edition**: 2021
- **MSRV**: 1.70 (low, to keep adoption plausible)
- **Deps**: none in default; `proptest` and `arbitrary` dev-only
- **`no_std`**: yes, with `alloc` for the AST

## Public API

```rust
pub fn parse(s: &str) -> Result<Symbol<'_>, ParseError>;

pub enum Symbol<'a> {
    V0(Path<'a>),
    Legacy(LegacyPath<'a>),
    NotRust(&'a str),
}

pub struct ParseError {
    pub kind: ParseErrorKind,
    pub byte_offset: usize,
}

// v0 AST
pub struct Path<'a> { /* ... */ }

impl<'a> Path<'a> {
    pub fn crate_name(&self) -> Option<&'a str>;
    pub fn crate_disambiguator(&self) -> Option<u64>;
    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>>;
    pub fn generic_args(&self) -> &[GenericArg<'a>];
    pub fn impl_self_type(&self) -> Option<&Type<'a>>;
    pub fn impl_trait(&self) -> Option<&Path<'a>>;
    pub fn is_drop_glue(&self) -> bool;
    pub fn is_closure(&self) -> Option<ClosureCoords<'a>>;
    pub fn is_async(&self) -> Option<AsyncFlavour>; // best-effort
}

pub enum GenericArg<'a> {
    Lifetime(Lifetime),
    Type(Type<'a>),
    Const(Const<'a>),
}

pub enum Type<'a> {
    Primitive(Primitive),
    Path(Path<'a>),
    Ref { mutability: Mutability, lifetime: Lifetime, ty: Box<Type<'a>> },
    RawPtr { mutability: Mutability, ty: Box<Type<'a>> },
    Array(Box<Type<'a>>, Const<'a>),
    Slice(Box<Type<'a>>),
    Tuple(Vec<Type<'a>>),
    Fn(FnSig<'a>),
    DynTrait { bounds: Vec<DynBound<'a>>, lifetime: Lifetime },
    Placeholder, // for `impl Trait` opaque
}

pub enum Const<'a> {
    Int { ty: Primitive, value: i128 },
    Bool(bool),
    Char(char),
    Str(&'a str),
    Placeholder,
}

pub struct ClosureCoords<'a> {
    pub parent: Path<'a>,
    pub index: u64,
}

// Display impls for Display parity with rustc-demangle
impl<'a> fmt::Display for Symbol<'a> { /* full form */ }
impl<'a> fmt::Display for Path<'a>   { /* `:#` for short form */ }
```

The `Symbol::Legacy` arm exposes a smaller surface — `crate_name()`,
`segments()`, `hash()` — since the legacy scheme cannot losslessly
encode generic args.

## Parser design

Single-pass with a backreference offset table. v0's `B<base62>_`
substitutions are byte-offset-into-input references, decodable in one
pass with a small `Vec<(usize, ParsedRef)>` of already-parsed nodes.

Internal types are private:

```rust
struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    arena: Bump,                    // bumpalo, behind alloc feature
    backrefs: Vec<(usize, NodeKind)>,
    depth: u16,                     // recursion guard
}
```

Maximum depth: 256 (matches `rustc-demangle`). Recursion attempt past
that returns `ParseErrorKind::TooDeep`.

Punycode for Unicode identifiers: a 60-line implementation of the
v0-modified Punycode (hyphen replaced with `_`). No external `idna`
crate.

## Robustness

- Property tests via `proptest` generating random byte strings;
  parser must never panic.
- Differential test against `rustc-demangle`: for a corpus of mangled
  symbols, the `Display` output of `rust-mangle-tree::parse(s).unwrap()`
  matches `rustc_demangle::demangle(s)` to the byte.
- `cargo fuzz` target with `arbitrary`; CI runs short fuzz on every PR.
- Corpus seeded from real binaries: `nm` over `target/debug/bugstalker`
  itself and a handful of large rustc-built binaries (`ripgrep`,
  `cargo`, `bat`).
- Negative tests for adversarial constructs: deeply nested generics,
  truncated input, malformed back-references, invalid Punycode,
  oversize const values.

## Legacy `_ZN…` parsing

Same crate, separate parse path. Legacy is a subset and produces a
flatter AST (`LegacyPath` with `segments` and trailing hash). Shares
no internal state with the v0 parser. Justification: callers already
need both schemes — mixed binaries are normal on stable until v0
becomes the default.

## Integration into BugStalker

Three call sites currently use `rustc_demangle`:

- `src/debugger/debugee/dwarf/symbol.rs:90` — bulk demangle of symbol
  table at load time. Switch to `rust_mangle_tree::parse` and cache the
  parsed `Path` (or its short-form string) in the `SymbolTab` entry.
- `src/debugger/debugee/dwarf/mod.rs:1110–1111` — `NamespaceHierarchy::
  from_mangled` extracts namespace + function. Rewrite to use the
  parsed segments rather than string-splitting on `::`.
- New consumer (Phase 3): vtable-symbol lookup → demangle →
  `impl_self_type()` to recover concrete type behind `dyn Trait`.

We keep `rustc-demangle` as a fallback for one release cycle in case
of differential bugs; gate on a cargo feature
`legacy-rustc-demangle-fallback`.

## Workspace integration

`Cargo.toml` (root) gains:

```toml
[workspace]
members = [
    ".",
    "crates/rust-mangle-tree",
]
```

`crates/rust-mangle-tree/Cargo.toml`:

```toml
[package]
name = "rust-mangle-tree"
version = "0.1.0"
edition = "2021"
rust-version = "1.70"
license = "MIT OR Apache-2.0"
description = "Parsed AST for Rust v0 (RFC 2603) and legacy mangled symbols."
repository = "https://github.com/godzie44/BugStalker"
keywords = ["debugger", "demangle", "rust", "v0", "symbols"]
categories = ["development-tools::debugging"]

[features]
default = ["std"]
std = []
alloc = []

[dev-dependencies]
proptest = "1"
arbitrary = { version = "1", features = ["derive"] }
rustc-demangle = "0.1.27"  # differential testing only
```

## Test plan

```text
crates/rust-mangle-tree/
├── tests/
│   ├── corpus/                   # mangled / expected-demangle pairs
│   │   ├── v0_basic.txt
│   │   ├── v0_generics.txt
│   │   ├── v0_const_generics.txt
│   │   ├── v0_closures.txt
│   │   ├── v0_drop_glue.txt
│   │   ├── v0_punycode.txt
│   │   ├── v0_hrtbs.txt
│   │   ├── v0_recursive.txt
│   │   ├── legacy_basic.txt
│   │   └── legacy_with_hash.txt
│   ├── corpus_test.rs            # iterate, assert Display matches
│   ├── differential.rs           # vs rustc-demangle
│   ├── fuzz_oracles.rs           # property tests
│   └── api_shape.rs              # exercise Path::crate_name etc.
└── fuzz/
    ├── Cargo.toml
    └── fuzz_targets/
        ├── parse_random.rs
        └── parse_real_binary.rs
```

## Performance budget

- Parsing throughput: 50 µs / symbol typical, 500 µs worst-case for
  deeply-generic. Measured against the BugStalker symbol-load path which
  parses ~20 k symbols on a real binary — total < 1 s.
- Zero allocations on the hot path for symbols < 256 bytes (arena
  pre-reserves; common case fits).
- Memory: ~3× input size for the AST including arena overhead.

Bench in `crates/rust-mangle-tree/benches/parse.rs` using `criterion`
(dev-dep). CI fails if median regresses > 10 % vs baseline.

## Acceptance criteria

- 100 % of the differential corpus matches `rustc-demangle` `Display`
  output.
- Fuzz target runs 1 M iterations on CI without panic.
- BugStalker symbol-load path uses the new crate, all existing tests
  pass.
- `crates.io` publish-ready (license headers, README, docs.rs examples).
- MSRV verified by `cargo +1.70 build`.

## Effort estimate

~3 weeks engineer-time. v0 grammar is ~300 lines of parser; the rest
is corpus assembly, fuzzing, differential test harness, and the
BugStalker integration.

## Risks

- **rustc emits invalid v0 in edge cases.** rust-lang/rust#134479 is
  open. Workaround: return `Err` rather than panic; tag the symbol as
  `Symbol::NotRust`; BugStalker falls back to displaying the raw
  mangled string.
- **rustc-demangle's `Display` is not strictly stable.** Differential
  test must allow whitespace and bracket-style variation; pin against
  a specific `rustc-demangle` version and update the pin deliberately.
- **MSRV drift.** Keep `clippy::msrv = "1.70"` enforced; CI builds on
  1.70 explicitly.

## Specifications

- RFC 2603 — Rust Symbol Name Mangling v0 — <https://rust-lang.github.io/rfcs/2603-rust-symbol-name-mangling-v0.html>. Authoritative grammar.
- v0 reference in the rustc Book — <https://doc.rust-lang.org/rustc/symbol-mangling/v0.html>. Worked examples.
- Itanium C++ ABI mangling section — <https://itanium-cxx-abi.github.io/cxx-abi/abi.html#mangling>. Underlying scheme for legacy `_ZN…` Rust mangling.
- RFC 3492 — Punycode — <https://datatracker.ietf.org/doc/html/rfc3492>. Modified per RFC 2603 (hyphen separator replaced by `_`).
- `rustc-demangle` source — <https://github.com/rust-lang/rustc-demangle>. Differential oracle; pin specific version.
- `rust-demangle.c` — <https://github.com/LykenSol/rust-demangle.c>. Third oracle for cross-checking.
- rustc dev guide on monomorphisation and mangling — <https://rustc-dev-guide.rust-lang.org/backend/monomorph.html>.
- Stabilisation PR for `-Csymbol-mangling-version=v0` — rust-lang/rust#90128.
- Tracking issue for v0 — rust-lang/rust#60705.
- v0 nightly default switch announcement — <https://blog.rust-lang.org/2025/11/20/switching-to-v0-mangling-on-nightly/>.
- rust-lang/rust#104830 — async/closure namespace tag ambiguity in v0.
- rust-lang/rust#134479 — v0 ICE with `generic_const_exprs`. We must surface as `Err`, never panic.

## Invariants

The parser is total and panic-free: every byte sequence, however malformed, must
return either `Ok` or `Err` without unwinding. The `debug_assert!` checks below
document the internal consistency properties the parser maintains at runtime in
debug builds.

```rust
// Parser position never escapes the input.
debug_assert!(self.pos <= self.input.len());

// Recursion depth bounded; matches rustc-demangle's 256.
debug_assert!(self.depth <= MAX_RECURSION_DEPTH);

// Backreferences point strictly earlier (single-pass guarantee).
debug_assert!(backref_offset < self.pos,
    "v0 backref {} >= current pos {}", backref_offset, self.pos);

// Display output is valid UTF-8 (checked because we may write
// arbitrary input bytes through Punycode decode).
debug_assert!(out.is_char_boundary(out.len()));

// Punycode-decoded identifier remains within the v0 ASCII subset.
debug_assert!(decoded_ident.chars().all(|c| !c.is_ascii_control()));

// Parsed Path's segment count is bounded.
debug_assert!(parsed.segments().count() <= MAX_PATH_SEGMENTS);

// Const-generic literal width matches its declared primitive type.
debug_assert_eq!(const_value.byte_width(), declared_primitive.byte_width());

// Symbol kind classified before display call.
debug_assert!(matches!(symbol.kind(),
    SymbolKind::V0 | SymbolKind::Legacy | SymbolKind::NotRust));
```

**No panic** is itself the headline invariant. While the `debug_assert!` checks
above catch violations in debug builds, the no-panic guarantee is enforced at the
boundary via `cargo fuzz`: the fuzz target feeds arbitrary byte strings to
`parse()` and treats any panic as a test failure. CI runs the fuzz target on every
PR to ensure the invariant holds across new parser paths.

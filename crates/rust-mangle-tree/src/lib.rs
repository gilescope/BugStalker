// SPDX-License-Identifier: MIT OR Apache-2.0
//! Parsed AST for Rust v0 (RFC 2603) and legacy mangled symbols.
//!
//! `rustc-demangle` exposes `Display` only — no parsed tree, no
//! programmatic access to generic args, impl self-type, or closure
//! coordinates. This crate gives you the tree.
//!
//! # Quick start
//!
//! ```no_run
//! use rust_mangle_tree::{parse, Symbol};
//! let s = "_RNvCs1234_4core3fmt5write";
//! match parse(s) {
//!     Ok(Symbol::V0(path))     => { let _ = path; /* walk segments, generic args, … */ }
//!     Ok(Symbol::Legacy(path)) => { let _ = path; /* simpler shape; no generics */ }
//!     Ok(Symbol::NotRust(raw)) => println!("not a Rust symbol: {raw}"),
//!     Err(e)                   => eprintln!("parse error at byte {}: {:?}", e.byte_offset, e.kind),
//! }
//! ```
//!
//! # Status
//!
//! Phase 2 batch A — public API and types only. The parser bodies for
//! v0 (batch C) and legacy (batch B) land in subsequent commits. Until
//! then, [`parse`] returns [`Symbol::NotRust`] for every input that
//! isn't trivially recognisable.
//!
//! # Crate features
//!
//! * `std` (default) — implements `std::error::Error` for [`ParseError`].
//!   Disable for `no_std + alloc`.
//!
//! # Invariants
//!
//! The parser is total and panic-free. Every byte sequence — however
//! malformed — returns either `Ok` or `Err`. CI's fuzz target enforces
//! this; see `crates/rust-mangle-tree/fuzz/`.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

mod legacy;
mod v0;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

// Re-exports of the parser entrypoints. The actual bodies are
// stubbed out in batch A; later batches fill them in.
pub use legacy::LegacyPath;
pub use v0::Path;

/// Parse a single mangled symbol.
///
/// Recognises three shapes:
///
/// 1. v0 RFC 2603 — `_RNv…` and longer. Returns [`Symbol::V0`].
/// 2. Legacy Itanium-style — `_ZN…E` (with optional trailing
///    `$hash$`). Returns [`Symbol::Legacy`].
/// 3. Anything else — returns [`Symbol::NotRust`] with the input
///    string unchanged.
///
/// Errors are returned for inputs that *look* like one of the
/// schemes (right prefix) but fail to parse. Inputs that don't even
/// have a recognised prefix never error — they round-trip as
/// [`Symbol::NotRust`].
pub fn parse(s: &str) -> Result<Symbol<'_>, ParseError> {
    if s.starts_with("_R") || s.starts_with('R') {
        v0::parse(s).map(Symbol::V0)
    } else if s.starts_with("_ZN") || s.starts_with("__ZN") {
        legacy::parse(s).map(Symbol::Legacy)
    } else {
        Ok(Symbol::NotRust(s))
    }
}

/// A parsed mangled symbol.
#[derive(Debug, Clone)]
pub enum Symbol<'a> {
    /// Rust v0 (RFC 2603) symbol.
    V0(Path<'a>),
    /// Legacy Itanium-style Rust symbol — `_ZN<len>name<len>name…E`
    /// with an optional trailing `$hash$`.
    Legacy(LegacyPath<'a>),
    /// Not a recognised Rust mangled symbol. Carries the input
    /// unchanged so callers can fall through to printing the raw
    /// string.
    NotRust(&'a str),
}

impl<'a> Symbol<'a> {
    /// Was the symbol recognised as a Rust mangled form?
    pub fn is_rust(&self) -> bool {
        !matches!(self, Symbol::NotRust(_))
    }
}

impl fmt::Display for Symbol<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::V0(p) => fmt::Display::fmt(p, f),
            Symbol::Legacy(p) => fmt::Display::fmt(p, f),
            Symbol::NotRust(raw) => f.write_str(raw),
        }
    }
}

/// What went wrong while parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// What kind of failure.
    pub kind: ParseErrorKind,
    /// Byte offset into the input where the parser gave up.
    pub byte_offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} at byte {}", self.kind, self.byte_offset)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

/// Discrete failure modes. Matches the granularity Phase 2's plan
/// calls for so callers can branch on `TooDeep` (tag the symbol and
/// fall back) versus a generic syntax error (almost certainly a
/// non-Rust input wearing a `_R` / `_Z` mask).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The input doesn't start with a recognised Rust prefix.
    NotMangled,
    /// Recursion limit (256) was exceeded — likely a malicious or
    /// pathologically generic symbol.
    TooDeep,
    /// A length-prefixed identifier claimed more bytes than remain.
    TruncatedIdent,
    /// A v0 backreference pointed at a byte we hadn't parsed yet
    /// (or past the end).
    InvalidBackref,
    /// A v0 base-62 digit was out of range.
    InvalidBase62,
    /// A Punycode-encoded identifier failed to decode.
    InvalidPunycode,
    /// A const-generic value didn't match its declared type.
    InvalidConst,
    /// Generic syntax error — input doesn't fit the grammar.
    Syntax,
    /// Input ended in the middle of a production.
    UnexpectedEof,
}

// ------------------------------------------------------------------
// Shared AST node types. v0 uses these heavily; legacy uses a small
// subset (just identifiers and lifetimes).
// ------------------------------------------------------------------

/// One generic argument in a `<…>` list.
#[derive(Debug, Clone)]
pub enum GenericArg<'a> {
    /// `'a` lifetime.
    Lifetime(Lifetime),
    /// `T` — a type.
    Type(Type<'a>),
    /// `N`, `'static`, `true`, `'c'`, `"…"` — a const generic value.
    Const(Const<'a>),
}

/// A lifetime by index. v0 numbers lifetimes at use site rather than
/// preserving names; rendering picks fresh `'a`, `'b`, … names from
/// the index. `0` means `'_` (placeholder); `'static` is encoded
/// distinctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetime(pub u32);

impl Lifetime {
    /// `'_` placeholder.
    pub const ANON: Lifetime = Lifetime(0);
    /// `'static`.
    pub const STATIC: Lifetime = Lifetime(u32::MAX);
}

/// `&` mutability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    /// `&T`
    Shared,
    /// `&mut T`
    Mut,
}

/// One type expression.
#[derive(Debug, Clone)]
pub enum Type<'a> {
    /// `i32`, `u8`, `bool`, `char`, `()`, `!`, etc.
    Primitive(Primitive),
    /// `crate::module::Type<…>`.
    Path(Path<'a>),
    /// `&'a T`, `&'a mut T`.
    Ref {
        /// shared / mut.
        mutability: Mutability,
        /// `'a`.
        lifetime: Lifetime,
        /// `T`.
        ty: Box<Type<'a>>,
    },
    /// `*const T`, `*mut T`.
    RawPtr {
        /// const / mut.
        mutability: Mutability,
        /// `T`.
        ty: Box<Type<'a>>,
    },
    /// `[T; N]`.
    Array(Box<Type<'a>>, Const<'a>),
    /// `[T]`.
    Slice(Box<Type<'a>>),
    /// `(T1, T2, …)`. The unit type `()` is the empty-tuple case.
    Tuple(Vec<Type<'a>>),
    /// `fn(…) -> …`.
    Fn(FnSig<'a>),
    /// `dyn Trait1 + Trait2 + 'a`.
    DynTrait {
        /// One bound per `+` in the source.
        bounds: Vec<DynBound<'a>>,
        /// `+ 'a`.
        lifetime: Lifetime,
    },
    /// `impl Trait` opaque type. v0 doesn't preserve which `impl
    /// Trait` site this came from.
    Placeholder,
}

/// Built-in primitive types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    /// `bool`
    Bool,
    /// `char`
    Char,
    /// `str`
    Str,
    /// `()`
    Unit,
    /// `!`
    Never,
    /// `i8`
    I8,
    /// `i16`
    I16,
    /// `i32`
    I32,
    /// `i64`
    I64,
    /// `i128`
    I128,
    /// `isize`
    Isize,
    /// `u8`
    U8,
    /// `u16`
    U16,
    /// `u32`
    U32,
    /// `u64`
    U64,
    /// `u128`
    U128,
    /// `usize`
    Usize,
    /// `f32`
    F32,
    /// `f64`
    F64,
    /// Placeholder type variable used in opaque positions.
    Placeholder,
}

impl Primitive {
    /// Render to its source-level keyword.
    pub fn as_str(self) -> &'static str {
        match self {
            Primitive::Bool => "bool",
            Primitive::Char => "char",
            Primitive::Str => "str",
            Primitive::Unit => "()",
            Primitive::Never => "!",
            Primitive::I8 => "i8",
            Primitive::I16 => "i16",
            Primitive::I32 => "i32",
            Primitive::I64 => "i64",
            Primitive::I128 => "i128",
            Primitive::Isize => "isize",
            Primitive::U8 => "u8",
            Primitive::U16 => "u16",
            Primitive::U32 => "u32",
            Primitive::U64 => "u64",
            Primitive::U128 => "u128",
            Primitive::Usize => "usize",
            Primitive::F32 => "f32",
            Primitive::F64 => "f64",
            Primitive::Placeholder => "_",
        }
    }
}

/// One const-generic value.
#[derive(Debug, Clone)]
pub enum Const<'a> {
    /// Numeric — `42i32`, `0u64`, etc.
    Int {
        /// Declared integer primitive.
        ty: Primitive,
        /// Two's-complement value, fits any of i128/u128.
        value: i128,
    },
    /// `true` / `false`.
    Bool(bool),
    /// `'c'`.
    Char(char),
    /// `"…"`. Lifetime tied to the input string.
    Str(&'a str),
    /// `_` — value not encoded (rare; legal in some impl-trait positions).
    Placeholder,
}

/// One `dyn Trait` bound.
#[derive(Debug, Clone)]
pub struct DynBound<'a> {
    /// The trait path.
    pub trait_path: Path<'a>,
    /// Associated-type bindings: `Item = T`.
    pub assoc: Vec<AssocBinding<'a>>,
}

/// One `Item = T` associated-type binding inside a `dyn Trait`.
#[derive(Debug, Clone)]
pub struct AssocBinding<'a> {
    /// The associated type's name (`"Item"`).
    pub name: &'a str,
    /// What it's bound to.
    pub ty: Type<'a>,
}

/// `fn(args…) -> ret` signature.
#[derive(Debug, Clone)]
pub struct FnSig<'a> {
    /// `extern "C" fn` etc. `None` for plain `fn`.
    pub abi: Option<&'a str>,
    /// `unsafe fn`?
    pub is_unsafe: bool,
    /// Argument types in order.
    pub args: Vec<Type<'a>>,
    /// Return type. `None` is sugar for `()`.
    pub ret: Option<Box<Type<'a>>>,
}

/// Closure coordinates: which fn the closure was defined inside, and
/// its index among same-fn closures.
#[derive(Debug, Clone)]
pub struct ClosureCoords<'a> {
    /// The enclosing fn / impl path.
    pub parent: Path<'a>,
    /// Closure index inside `parent`. Starts at 0.
    pub index: u64,
}

/// async-fn flavour, when recognisable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncFlavour {
    /// `async fn` — encodes as a closure under a wrapper.
    AsyncFn,
    /// `async {}` block.
    AsyncBlock,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_rust_passes_through() {
        let s = "main";
        match parse(s).unwrap() {
            Symbol::NotRust(raw) => assert_eq!(raw, "main"),
            other => panic!("expected NotRust, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_is_not_rust() {
        match parse("").unwrap() {
            Symbol::NotRust(raw) => assert_eq!(raw, ""),
            other => panic!("expected NotRust, got {other:?}"),
        }
    }

    #[test]
    fn display_passthrough() {
        let s = "main";
        let parsed = parse(s).unwrap();
        assert_eq!(format!("{parsed}"), "main");
    }

    #[test]
    fn primitive_render() {
        assert_eq!(Primitive::I32.as_str(), "i32");
        assert_eq!(Primitive::Unit.as_str(), "()");
        assert_eq!(Primitive::Never.as_str(), "!");
    }
}

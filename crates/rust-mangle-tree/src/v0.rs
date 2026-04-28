// SPDX-License-Identifier: MIT OR Apache-2.0
//! Rust v0 (RFC 2603) symbol parser.
//!
//! Phase 2 batch A — public type only. Parser body lands in batch C
//! (grammar core) + batch D (Punycode + back-references).

use crate::{GenericArg, ParseError, ParseErrorKind};
use alloc::vec::Vec;
use core::fmt;

/// A parsed v0 symbol: a sequence of namespace segments with
/// optional generic args, an optional `<impl Self as Trait>` self-
/// type, and miscellaneous flags (drop glue, closure coordinates,
/// async wrappers).
#[derive(Debug, Clone)]
pub struct Path<'a> {
    /// Outermost crate name in the symbol (`core`, `alloc`, etc.).
    pub(crate) crate_name: Option<&'a str>,
    /// Crate disambiguator hash. Always `Some` for first-class
    /// crates; `None` for synthetic paths the compiler generates.
    pub(crate) crate_disambiguator: Option<u64>,
    /// Inner segments — `module::module::function`.
    pub(crate) segments: Vec<Segment<'a>>,
    /// Generic arguments at the outermost path. Empty when the
    /// outermost segment isn't generic.
    pub(crate) generic_args: Vec<GenericArg<'a>>,
}

impl<'a> Path<'a> {
    /// Outermost crate name.
    pub fn crate_name(&self) -> Option<&'a str> {
        self.crate_name
    }

    /// Crate disambiguator hash, when one was encoded.
    pub fn crate_disambiguator(&self) -> Option<u64> {
        self.crate_disambiguator
    }

    /// Walk segments outermost → innermost.
    pub fn segments(&self) -> impl Iterator<Item = &Segment<'a>> {
        self.segments.iter()
    }

    /// Generic arguments at the outermost path.
    pub fn generic_args(&self) -> &[GenericArg<'a>] {
        &self.generic_args
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Stub display: render `<crate>::<seg>::<seg>…`. Once batch C
        // lands, the real impl preserves rustc-demangle's spacing
        // and punctuation byte-for-byte.
        let mut first = true;
        if let Some(c) = self.crate_name {
            f.write_str(c)?;
            first = false;
        }
        for seg in &self.segments {
            if !first {
                f.write_str("::")?;
            }
            first = false;
            f.write_str(seg.name)?;
        }
        Ok(())
    }
}

/// One segment of a v0 path. Carries the segment name plus the kind
/// of namespace it lives in (value, type, closure, …).
#[derive(Debug, Clone)]
pub struct Segment<'a> {
    /// Segment name as written in the source.
    pub name: &'a str,
    /// Which v0 namespace the segment occupies. `None` for plain
    /// crate-root segments.
    pub namespace: Option<Namespace>,
    /// Segment-local generic args. `Path::generic_args` returns the
    /// outermost set; intermediate segments can carry their own.
    pub generic_args: Vec<GenericArg<'a>>,
}

/// v0 namespace tag — `C` for closure, `S` for shim, plus the value/
/// type split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// `N` — value namespace (functions, statics, consts).
    Value,
    /// `N` — type namespace (structs, enums, traits).
    Type,
    /// `C` — closure.
    Closure,
    /// `S` — shim (drop glue, vtable thunk, …).
    Shim,
    /// Anything not in the table above. Carries the raw tag byte.
    Other(u8),
}

/// Parse a v0 symbol. Phase 2 batch A returns the
/// `NotMangled`/`Syntax` errors only — the real parser lands in
/// batches C/D.
pub(crate) fn parse(s: &str) -> Result<Path<'_>, ParseError> {
    // Strip the leading underscore (some toolchains emit `_R…`,
    // others `R…`; both are valid).
    let body = s.strip_prefix("_R").or_else(|| s.strip_prefix('R'));
    let _body = body.ok_or(ParseError {
        kind: ParseErrorKind::NotMangled,
        byte_offset: 0,
    })?;
    // TODO(phase 2 batch C): real grammar.
    Err(ParseError {
        kind: ParseErrorKind::Syntax,
        byte_offset: 0,
    })
}

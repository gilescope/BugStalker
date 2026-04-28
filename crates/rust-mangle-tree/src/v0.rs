// SPDX-License-Identifier: MIT OR Apache-2.0
//! Rust v0 (RFC 2603) symbol parser.
//!
//! v0 mangled names follow a tagged grammar where every production
//! starts with a single ASCII byte indicating its kind:
//!
//! ```text
//! C  crate root
//! M  inherent impl
//! X  trait impl
//! Y  trait association  (`<T as Trait>::method`)
//! N  nested path        (e.g. module::function)
//! I  generic instantiation
//! B  back-reference
//! ```
//!
//! Each path can carry generic args (`I<…>E`); each type production
//! has its own tag (`R` for `&T`, `Q` for `&mut T`, `A` for `[T; N]`,
//! …); see `parse_type` for the full table.
//!
//! Back-references use base-62-encoded byte offsets into the input.
//! v0 is single-pass — a back-ref always points strictly *earlier*
//! than the current position, so we maintain an `offset → ParsedNode`
//! map as we parse and resolve `B<offset>_` against it.
//!
//! Punycode-decoded identifiers are prefixed with `u`; the rest of
//! the bytes are RFC 3492 modified per RFC 2603 (hyphen separator
//! replaced with `_`).

use crate::{
    AssocBinding, Const, DynBound, FnSig, GenericArg, Lifetime, Mutability, ParseError,
    ParseErrorKind, Primitive, Type,
};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Maximum recursion depth. Matches `rustc-demangle`'s 256.
const MAX_DEPTH: u16 = 256;

/// A parsed v0 symbol path. Mirrors the grammar's productions.
#[derive(Debug, Clone)]
pub enum Path<'a> {
    /// `C<disambiguator><name>` — top-level crate. e.g.
    /// `Cs1234_4core` → `core`.
    CrateRoot {
        /// `s1234` → `Some(1234)`. `0` is encoded as the empty
        /// disambiguator and surfaces as `None` here.
        disambiguator: Option<u64>,
        /// The crate name as written, after Punycode decode.
        name: Ident<'a>,
    },
    /// `M<disambiguator><self-type>` — inherent impl block on a
    /// type. The parent path was implicit in the source.
    InherentImpl {
        /// Optional disambiguator — multiple `impl` blocks on the
        /// same type get distinct values.
        disambiguator: Option<u64>,
        /// The path of the impl's enclosing scope (always present
        /// per the grammar, but encoded as an empty path for the
        /// crate root).
        parent: Box<Path<'a>>,
        /// The `Self` type the impl is for.
        self_type: Box<Type<'a>>,
    },
    /// `X<disambiguator><parent><self-type><trait-path>` — `impl
    /// Trait for Self`.
    TraitImpl {
        /// Optional disambiguator across overlapping impls.
        disambiguator: Option<u64>,
        /// Enclosing scope.
        parent: Box<Path<'a>>,
        /// `Self`.
        self_type: Box<Type<'a>>,
        /// `Trait`.
        trait_path: Box<Path<'a>>,
    },
    /// `Y<self-type><trait-path>` — `<T as Trait>::method` form.
    TraitAssoc {
        /// `T`.
        self_type: Box<Type<'a>>,
        /// `Trait`.
        trait_path: Box<Path<'a>>,
    },
    /// `N<namespace><parent><name>` — one nesting level.
    /// `namespace` is the v0 ns tag byte ('v' = value, 't' =
    /// type, 'C' = closure, 'S' = shim, etc.).
    Nested {
        /// Single ASCII byte indicating which namespace the new
        /// segment lives in. We carry it raw so renderers can
        /// recreate the source-style path the user expects.
        namespace: u8,
        /// Enclosing scope.
        parent: Box<Path<'a>>,
        /// `(disambiguator, name)` — disambiguator is `0`/`None`
        /// for non-closure / non-shim segments.
        disambiguator: Option<u64>,
        /// Segment name (Ident). Empty `Ident` is legal — closures
        /// and synthetic shims often have empty names.
        name: Ident<'a>,
    },
    /// `I<parent><generic-arg>*E` — generic instantiation of an
    /// existing path.
    Generic {
        /// The path being instantiated.
        parent: Box<Path<'a>>,
        /// Generic args in source order.
        args: Vec<GenericArg<'a>>,
    },
}

/// An identifier extracted from the input. Either a borrowed slice
/// from the input (`Raw`) or a heap string holding the Punycode
/// decode result.
#[derive(Debug, Clone)]
pub enum Ident<'a> {
    /// Raw bytes from the input — no Punycode decoding needed.
    Raw(&'a str),
    /// Punycode-decoded identifier; allocated.
    Decoded(Rc<str>),
}

impl Ident<'_> {
    /// Borrow the rendered text. Both arms yield a `&str` directly.
    pub fn as_str(&self) -> &str {
        match self {
            Ident::Raw(s) => s,
            Ident::Decoded(s) => s,
        }
    }

    /// True for the empty-string identifier rustc emits for some
    /// closures and shims.
    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }
}

impl<'a> Path<'a> {
    /// Walk to the outermost `CrateRoot` and return its name.
    pub fn crate_name(&self) -> Option<&str> {
        match self {
            Path::CrateRoot { name, .. } => Some(name.as_str()),
            Path::InherentImpl { parent, .. }
            | Path::TraitImpl { parent, .. }
            | Path::Nested { parent, .. }
            | Path::Generic { parent, .. } => parent.crate_name(),
            Path::TraitAssoc { trait_path, .. } => trait_path.crate_name(),
        }
    }

    /// Crate disambiguator for the outermost `CrateRoot`.
    pub fn crate_disambiguator(&self) -> Option<u64> {
        match self {
            Path::CrateRoot { disambiguator, .. } => *disambiguator,
            Path::InherentImpl { parent, .. }
            | Path::TraitImpl { parent, .. }
            | Path::Nested { parent, .. }
            | Path::Generic { parent, .. } => parent.crate_disambiguator(),
            Path::TraitAssoc { trait_path, .. } => trait_path.crate_disambiguator(),
        }
    }

    /// `Self` type for an impl path, or `None` for non-impls.
    pub fn impl_self_type(&self) -> Option<&Type<'a>> {
        match self {
            Path::InherentImpl { self_type, .. }
            | Path::TraitImpl { self_type, .. }
            | Path::TraitAssoc { self_type, .. } => Some(self_type),
            _ => None,
        }
    }

    /// Trait path for an `X` or `Y` path.
    pub fn impl_trait(&self) -> Option<&Path<'a>> {
        match self {
            Path::TraitImpl { trait_path, .. } | Path::TraitAssoc { trait_path, .. } => {
                Some(trait_path)
            }
            _ => None,
        }
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_path(self, f, false)
    }
}

/// Display helper. `in_type` is `true` when we're rendering inside a
/// type position (generic argument, ref/array/tuple inner, etc.) and
/// generic args attach via `<…>`; otherwise we're at a value position
/// (free fn, method, const) and use `::<…>`. Mirrors
/// `rustc-demangle`'s convention exactly.
fn write_path(p: &Path<'_>, f: &mut fmt::Formatter<'_>, in_type: bool) -> fmt::Result {
    match p {
        Path::CrateRoot { name, .. } => f.write_str(name.as_str()),
        Path::Nested {
            namespace,
            parent,
            disambiguator,
            name,
        } => {
            // Unknown lowercase namespaces with empty names are
            // compiler-internal nesting markers (e.g. `k` for some
            // promoted constant contexts) — rustc-demangle elides
            // them entirely, leaving just the parent path. We do
            // the same so our output stays in lockstep.
            if !matches!(*namespace, b'C' | b'S' | b'v' | b't') && name.is_empty() {
                return write_path(parent, f, in_type);
            }
            write_path(parent, f, in_type)?;
            f.write_str("::")?;
            // Closure / shim namespaces wrap the segment in `{…}`
            // markers per rustc-demangle. The disambiguator surfaces
            // as `#N` inside the markers. rustc-demangle's display
            // convention:
            //   • absent (no `s` prefix)  → `#0`
            //   • `s_`   (empty body, raw 0) → `#1`
            //   • `s0_`  (body "0", raw 1)   → `#2`
            // So we add 1 when the disambiguator was present, and
            // render `0` when it wasn't.
            let disc = disambiguator.map(|d| d.saturating_add(1)).unwrap_or(0);
            match (*namespace, name.is_empty()) {
                (b'C', _) => write!(f, "{{closure#{disc}}}")?,
                (b'S', true) => write!(f, "{{shim#{disc}}}")?,
                (b'S', false) => write!(f, "{{shim:{}#{disc}}}", name.as_str())?,
                (_, false) => f.write_str(name.as_str())?,
                (_, true) => write!(f, "{{nested#{disc}}}")?,
            }
            Ok(())
        }
        Path::Generic { parent, args } => {
            write_path(parent, f, in_type)?;
            if !args.is_empty() {
                if in_type {
                    f.write_str("<")?;
                } else {
                    f.write_str("::<")?;
                }
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write_generic_arg(a, f)?;
                }
                f.write_str(">")?;
            }
            Ok(())
        }
        Path::InherentImpl { self_type, .. } => {
            // The impl's parent path is contextual mangling info
            // (which module the `impl` block sits in); rustc-demangle
            // renders only `<self_type>` because the self-type's own
            // path already names where the type was *defined*, which
            // is what users care about.
            f.write_str("<")?;
            write_type(self_type, f)?;
            f.write_str(">")
        }
        Path::TraitImpl {
            self_type,
            trait_path,
            ..
        } => {
            f.write_str("<")?;
            write_type(self_type, f)?;
            f.write_str(" as ")?;
            write_path(trait_path, f, true)?;
            f.write_str(">")
        }
        Path::TraitAssoc {
            self_type,
            trait_path,
        } => {
            f.write_str("<")?;
            write_type(self_type, f)?;
            f.write_str(" as ")?;
            write_path(trait_path, f, true)?;
            f.write_str(">")
        }
    }
}

fn write_generic_arg(a: &GenericArg<'_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match a {
        GenericArg::Lifetime(l) => write_lifetime(*l, f),
        GenericArg::Type(t) => write_type(t, f),
        GenericArg::Const(c) => write_const(c, f),
    }
}

fn write_lifetime(l: Lifetime, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if l == Lifetime::ANON {
        f.write_str("'_")
    } else if l == Lifetime::STATIC {
        f.write_str("'static")
    } else {
        // Indexed lifetimes — render `'a`, `'b`, …. Index 1 → `'a`,
        // 2 → `'b`, etc. Real rustc-demangle uses a more elaborate
        // disambiguation; this is a minimal stand-in until batch E.
        let idx = l.0.saturating_sub(1) as usize;
        let mut buf = [0u8; 8];
        let mut len = 0;
        let mut n = idx;
        loop {
            buf[len] = b'a' + (n % 26) as u8;
            len += 1;
            if n < 26 || len >= buf.len() {
                break;
            }
            n /= 26;
            n -= 1;
        }
        // Reverse.
        buf[..len].reverse();
        f.write_str("'")?;
        // SAFETY: bytes are ASCII letters.
        f.write_str(core::str::from_utf8(&buf[..len]).unwrap())
    }
}

/// Render a single `dyn` bound. Generic args and assoc-type
/// bindings get merged into a single `<args, AssocName = T, …>`
/// block, mirroring rustc-demangle: `Iterator<Item = u32>` not
/// `Iterator<><Item = u32>`. When the trait path's outermost node
/// is `Generic`, we splice the assoc bindings into the existing
/// `<…>`; otherwise we open a fresh `<…>` for them.
fn write_dyn_bound(b: &crate::DynBound<'_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &b.trait_path {
        Path::Generic { parent, args } => {
            // Trait path with generic args — open one block holding
            // both the args and the assoc bindings.
            write_path(parent, f, true)?;
            if args.is_empty() && b.assoc.is_empty() {
                return Ok(());
            }
            f.write_str("<")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_generic_arg(a, f)?;
            }
            for (i, a) in b.assoc.iter().enumerate() {
                if i > 0 || !args.is_empty() {
                    f.write_str(", ")?;
                }
                f.write_str(a.name)?;
                f.write_str(" = ")?;
                write_type(&a.ty, f)?;
            }
            f.write_str(">")
        }
        other => {
            write_path(other, f, true)?;
            if !b.assoc.is_empty() {
                f.write_str("<")?;
                for (i, a) in b.assoc.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(a.name)?;
                    f.write_str(" = ")?;
                    write_type(&a.ty, f)?;
                }
                f.write_str(">")?;
            }
            Ok(())
        }
    }
}

fn write_type(t: &Type<'_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match t {
        Type::Primitive(p) => f.write_str(p.as_str()),
        Type::Path(p) => write_path(p, f, true),
        Type::Ref {
            mutability,
            lifetime,
            ty,
        } => {
            f.write_str("&")?;
            if *lifetime != Lifetime::ANON {
                write_lifetime(*lifetime, f)?;
                f.write_str(" ")?;
            }
            if *mutability == Mutability::Mut {
                f.write_str("mut ")?;
            }
            write_type(ty, f)
        }
        Type::RawPtr { mutability, ty } => {
            f.write_str(match mutability {
                Mutability::Shared => "*const ",
                Mutability::Mut => "*mut ",
            })?;
            write_type(ty, f)
        }
        Type::Array(inner, len) => {
            f.write_str("[")?;
            write_type(inner, f)?;
            f.write_str("; ")?;
            write_const(len, f)?;
            f.write_str("]")
        }
        Type::Slice(inner) => {
            f.write_str("[")?;
            write_type(inner, f)?;
            f.write_str("]")
        }
        Type::Tuple(items) => {
            f.write_str("(")?;
            for (i, ty) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type(ty, f)?;
            }
            if items.len() == 1 {
                f.write_str(",")?;
            }
            f.write_str(")")
        }
        Type::Fn(sig) => write_fn_sig(sig, f),
        Type::DynTrait { bounds, lifetime } => {
            f.write_str("dyn ")?;
            for (i, b) in bounds.iter().enumerate() {
                if i > 0 {
                    f.write_str(" + ")?;
                }
                write_dyn_bound(b, f)?;
            }
            if *lifetime != Lifetime::ANON {
                f.write_str(" + ")?;
                write_lifetime(*lifetime, f)?;
            }
            Ok(())
        }
        Type::Placeholder => f.write_str("_"),
    }
}

fn write_fn_sig(sig: &FnSig<'_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if sig.is_unsafe {
        f.write_str("unsafe ")?;
    }
    if let Some(abi) = sig.abi {
        f.write_str("extern \"")?;
        f.write_str(abi)?;
        f.write_str("\" ")?;
    }
    f.write_str("fn(")?;
    for (i, ty) in sig.args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_type(ty, f)?;
    }
    f.write_str(")")?;
    if let Some(ret) = &sig.ret {
        f.write_str(" -> ")?;
        write_type(ret, f)?;
    }
    Ok(())
}

fn write_const(c: &Const<'_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match c {
        Const::Int { ty, value } => {
            // Render unsigned types as positive; signed types
            // honour the value's sign as encoded in the i128.
            match ty {
                Primitive::U8
                | Primitive::U16
                | Primitive::U32
                | Primitive::U64
                | Primitive::U128
                | Primitive::Usize => {
                    write!(f, "{}", *value as u128)
                }
                _ => write!(f, "{value}"),
            }
        }
        Const::Bool(b) => f.write_str(if *b { "true" } else { "false" }),
        Const::Char(c) => write!(f, "'{c}'"),
        Const::Str(s) => write!(f, "{s:?}"),
        Const::Placeholder => f.write_str("_"),
    }
}

// ------------------------------------------------------------------
// Parser
// ------------------------------------------------------------------

/// What was parsed at a given input offset, for back-reference
/// resolution. The grammar lets back-refs target paths, types, and
/// consts.
#[derive(Clone)]
enum Parsed<'a> {
    Path(Rc<Path<'a>>),
    Type(Rc<Type<'a>>),
    Const(Rc<Const<'a>>),
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    depth: u16,
    backrefs: Vec<(usize, Parsed<'a>)>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
            backrefs: Vec::new(),
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.err(ParseErrorKind::TooDeep));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn err(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            kind,
            byte_offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn bump(&mut self) -> Result<u8, ParseError> {
        let b = self.peek().ok_or(self.err(ParseErrorKind::UnexpectedEof))?;
        self.pos += 1;
        Ok(b)
    }

    /// Parse a base-62 nat terminated by `_`. Empty body before the
    /// `_` encodes 0.
    fn parse_base62(&mut self) -> Result<u64, ParseError> {
        let mut value: u64 = 0;
        let mut empty = true;
        loop {
            let b = self.peek().ok_or(self.err(ParseErrorKind::UnexpectedEof))?;
            if b == b'_' {
                self.pos += 1;
                return Ok(if empty {
                    0
                } else {
                    value.checked_add(1).ok_or(self.err(ParseErrorKind::InvalidBase62))?
                });
            }
            empty = false;
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as u64,
                b'a'..=b'z' => 10 + (b - b'a') as u64,
                b'A'..=b'Z' => 36 + (b - b'A') as u64,
                _ => return Err(self.err(ParseErrorKind::InvalidBase62)),
            };
            value = value
                .checked_mul(62)
                .and_then(|v| v.checked_add(digit))
                .ok_or(self.err(ParseErrorKind::InvalidBase62))?;
            self.pos += 1;
        }
    }

    /// Optional disambiguator: leading `s` followed by a base62
    /// number ended by `_`. Absent → `None`.
    fn parse_disambiguator(&mut self) -> Result<Option<u64>, ParseError> {
        if self.peek() == Some(b's') {
            self.pos += 1;
            Ok(Some(self.parse_base62()?))
        } else {
            Ok(None)
        }
    }

    /// Undisambiguated identifier: optional `u` (Punycode flag),
    /// then base-10 length, then `<len>` bytes, then optional `_`
    /// continuation when the next byte would be a digit (rustc-
    /// demangle's "shadow" rule).
    fn parse_undisambiguated_ident(&mut self) -> Result<Ident<'a>, ParseError> {
        let punycoded = if self.peek() == Some(b'u') {
            self.pos += 1;
            true
        } else {
            false
        };
        let len_start = self.pos;
        let mut len: usize = 0;
        let mut got_digit = false;
        // v0 disallows leading zeros on the length prefix: `0` is
        // exactly length 0 (a single digit), `10` is length ten,
        // and `00` is malformed (or, in practice, two consecutive
        // zero-length identifiers). Reading greedily here would
        // swallow nested closures' names that all happen to be
        // empty.
        while let Some(b) = self.peek() {
            if !b.is_ascii_digit() {
                break;
            }
            // Single leading `0` ⇒ length 0; stop immediately so the
            // caller doesn't misinterpret subsequent zeros.
            if !got_digit && b == b'0' {
                got_digit = true;
                self.pos += 1;
                break;
            }
            got_digit = true;
            len = len
                .checked_mul(10)
                .and_then(|v| v.checked_add((b - b'0') as usize))
                .ok_or(self.err(ParseErrorKind::Syntax))?;
            self.pos += 1;
        }
        if !got_digit {
            return Err(ParseError {
                kind: ParseErrorKind::Syntax,
                byte_offset: len_start,
            });
        }
        // Optional `_` continuation when the identifier starts with
        // a digit.
        if self.peek() == Some(b'_') {
            self.pos += 1;
        }
        if self.pos.checked_add(len).map_or(true, |end| end > self.input.len()) {
            return Err(self.err(ParseErrorKind::TruncatedIdent));
        }
        let bytes = &self.input[self.pos..self.pos + len];
        self.pos += len;
        let raw = core::str::from_utf8(bytes).map_err(|_| ParseError {
            kind: ParseErrorKind::Syntax,
            byte_offset: self.pos - len,
        })?;
        if punycoded {
            let decoded =
                decode_punycode(raw).ok_or_else(|| self.err(ParseErrorKind::InvalidPunycode))?;
            Ok(Ident::Decoded(Rc::from(decoded.as_str())))
        } else {
            Ok(Ident::Raw(raw))
        }
    }

    /// Top-level entry point.
    fn parse_path_top(&mut self) -> Result<Path<'a>, ParseError> {
        let p = self.parse_path()?;
        // For a complete v0 symbol, the input may have a trailing
        // `.<suffix>` (LLVM thunk decorations) we ignore.
        Ok(p)
    }

    fn parse_path(&mut self) -> Result<Path<'a>, ParseError> {
        self.enter()?;
        let start = self.pos;
        let tag = self.bump()?;
        let path = match tag {
            b'C' => {
                let disambiguator = self.parse_disambiguator()?;
                let name = self.parse_undisambiguated_ident()?;
                Path::CrateRoot { disambiguator, name }
            }
            b'M' => {
                let disambiguator = self.parse_disambiguator()?;
                let parent = Box::new(self.parse_path()?);
                let self_type = Box::new(self.parse_type()?);
                Path::InherentImpl {
                    disambiguator,
                    parent,
                    self_type,
                }
            }
            b'X' => {
                let disambiguator = self.parse_disambiguator()?;
                let parent = Box::new(self.parse_path()?);
                let self_type = Box::new(self.parse_type()?);
                let trait_path = Box::new(self.parse_path()?);
                Path::TraitImpl {
                    disambiguator,
                    parent,
                    self_type,
                    trait_path,
                }
            }
            b'Y' => {
                let self_type = Box::new(self.parse_type()?);
                let trait_path = Box::new(self.parse_path()?);
                Path::TraitAssoc {
                    self_type,
                    trait_path,
                }
            }
            b'N' => {
                let namespace = self.bump()?;
                let parent = Box::new(self.parse_path()?);
                let disambiguator = self.parse_disambiguator()?;
                let name = self.parse_undisambiguated_ident()?;
                Path::Nested {
                    namespace,
                    parent,
                    disambiguator,
                    name,
                }
            }
            b'I' => {
                let parent = Box::new(self.parse_path()?);
                let mut args = Vec::new();
                while self.peek() != Some(b'E') {
                    args.push(self.parse_generic_arg()?);
                }
                // Consume `E`.
                self.pos += 1;
                Path::Generic { parent, args }
            }
            b'B' => {
                let offset = self.parse_base62()?;
                let resolved = self.lookup_backref_path(offset as usize)?;
                self.leave();
                return Ok((*resolved).clone());
            }
            _ => return Err(self.err(ParseErrorKind::Syntax)),
        };
        let path_rc = Rc::new(path.clone());
        self.backrefs.push((start, Parsed::Path(path_rc)));
        self.leave();
        Ok(path)
    }

    fn lookup_backref_path(&self, offset: usize) -> Result<Rc<Path<'a>>, ParseError> {
        for (off, parsed) in self.backrefs.iter().rev() {
            if *off == offset {
                if let Parsed::Path(p) = parsed {
                    return Ok(p.clone());
                }
            }
        }
        Err(ParseError {
            kind: ParseErrorKind::InvalidBackref,
            byte_offset: offset,
        })
    }

    fn lookup_backref_type(&self, offset: usize) -> Result<Rc<Type<'a>>, ParseError> {
        for (off, parsed) in self.backrefs.iter().rev() {
            if *off == offset {
                if let Parsed::Type(t) = parsed {
                    return Ok(t.clone());
                }
            }
        }
        Err(ParseError {
            kind: ParseErrorKind::InvalidBackref,
            byte_offset: offset,
        })
    }

    fn lookup_backref_const(&self, offset: usize) -> Result<Rc<Const<'a>>, ParseError> {
        for (off, parsed) in self.backrefs.iter().rev() {
            if *off == offset {
                if let Parsed::Const(c) = parsed {
                    return Ok(c.clone());
                }
            }
        }
        Err(ParseError {
            kind: ParseErrorKind::InvalidBackref,
            byte_offset: offset,
        })
    }

    fn parse_generic_arg(&mut self) -> Result<GenericArg<'a>, ParseError> {
        match self.peek().ok_or(self.err(ParseErrorKind::UnexpectedEof))? {
            b'L' => {
                self.pos += 1;
                let n = self.parse_base62()?;
                Ok(GenericArg::Lifetime(Lifetime(n as u32)))
            }
            b'K' => {
                self.pos += 1;
                let c = self.parse_const()?;
                Ok(GenericArg::Const(c))
            }
            _ => {
                let t = self.parse_type()?;
                Ok(GenericArg::Type(t))
            }
        }
    }

    fn parse_type(&mut self) -> Result<Type<'a>, ParseError> {
        self.enter()?;
        let start = self.pos;
        let b = self.peek().ok_or(self.err(ParseErrorKind::UnexpectedEof))?;
        // Primitive single-char types come first. The byte alphabet
        // is RFC 2603 §basic-type table.
        if let Some(prim) = primitive_for_tag(b) {
            self.pos += 1;
            self.leave();
            let ty = Type::Primitive(prim);
            self.backrefs.push((start, Parsed::Type(Rc::new(ty.clone()))));
            return Ok(ty);
        }
        let ty = match b {
            b'R' => {
                self.pos += 1;
                let lifetime = self.maybe_parse_lifetime()?;
                let inner = Box::new(self.parse_type()?);
                Type::Ref {
                    mutability: Mutability::Shared,
                    lifetime,
                    ty: inner,
                }
            }
            b'Q' => {
                self.pos += 1;
                let lifetime = self.maybe_parse_lifetime()?;
                let inner = Box::new(self.parse_type()?);
                Type::Ref {
                    mutability: Mutability::Mut,
                    lifetime,
                    ty: inner,
                }
            }
            b'P' => {
                self.pos += 1;
                let inner = Box::new(self.parse_type()?);
                Type::RawPtr {
                    mutability: Mutability::Shared,
                    ty: inner,
                }
            }
            b'O' => {
                self.pos += 1;
                let inner = Box::new(self.parse_type()?);
                Type::RawPtr {
                    mutability: Mutability::Mut,
                    ty: inner,
                }
            }
            b'A' => {
                self.pos += 1;
                let inner = Box::new(self.parse_type()?);
                let len = self.parse_const()?;
                Type::Array(inner, len)
            }
            b'S' => {
                self.pos += 1;
                let inner = Box::new(self.parse_type()?);
                Type::Slice(inner)
            }
            b'T' => {
                self.pos += 1;
                let mut items = Vec::new();
                while self.peek() != Some(b'E') {
                    items.push(self.parse_type()?);
                }
                self.pos += 1;
                Type::Tuple(items)
            }
            b'F' => {
                self.pos += 1;
                let sig = self.parse_fn_sig()?;
                Type::Fn(sig)
            }
            b'D' => {
                self.pos += 1;
                let bounds = self.parse_dyn_bounds()?;
                let lifetime = self.maybe_parse_lifetime()?;
                Type::DynTrait { bounds, lifetime }
            }
            b'B' => {
                self.pos += 1;
                let offset = self.parse_base62()?;
                let resolved = self.lookup_backref_type(offset as usize)?;
                self.leave();
                return Ok((*resolved).clone());
            }
            // Anything else is a Path (most common case — type
            // names are mangled as v0 paths).
            _ => Type::Path(self.parse_path()?),
        };
        let ty_rc = Rc::new(ty.clone());
        self.backrefs.push((start, Parsed::Type(ty_rc)));
        self.leave();
        Ok(ty)
    }

    fn maybe_parse_lifetime(&mut self) -> Result<Lifetime, ParseError> {
        if self.peek() == Some(b'L') {
            self.pos += 1;
            let n = self.parse_base62()?;
            Ok(Lifetime(n as u32))
        } else {
            Ok(Lifetime::ANON)
        }
    }

    fn parse_fn_sig(&mut self) -> Result<FnSig<'a>, ParseError> {
        // Optional `U` for unsafe, optional `K<abi>` for extern.
        let is_unsafe = if self.peek() == Some(b'U') {
            self.pos += 1;
            true
        } else {
            false
        };
        let abi = if self.peek() == Some(b'K') {
            self.pos += 1;
            // ABI is itself a v0 undisambiguated identifier.
            let ident = self.parse_undisambiguated_ident()?;
            // We allocate the borrowed ident bytes back into a
            // 'a-lifetime &str. For raw idents that's free.
            match ident {
                Ident::Raw(s) => Some(s),
                Ident::Decoded(_) => {
                    // Unusual: a Punycode-encoded ABI string. Drop
                    // for now; rustc does not emit these in practice.
                    None
                }
            }
        } else {
            None
        };
        let mut args = Vec::new();
        while self.peek() != Some(b'E') {
            args.push(self.parse_type()?);
        }
        self.pos += 1;
        let ret_ty = self.parse_type()?;
        // A bare `u` (unit) return is the implicit void.
        let ret = if matches!(ret_ty, Type::Primitive(Primitive::Unit)) {
            None
        } else {
            Some(Box::new(ret_ty))
        };
        Ok(FnSig {
            abi,
            is_unsafe,
            args,
            ret,
        })
    }

    fn parse_dyn_bounds(&mut self) -> Result<Vec<DynBound<'a>>, ParseError> {
        let mut bounds = Vec::new();
        while self.peek() != Some(b'E') {
            let trait_path = self.parse_path()?;
            // Optional `p<assoc-name><type>` repeated, terminated
            // implicitly by next `E` or another bound's path tag.
            let mut assoc = Vec::new();
            while self.peek() == Some(b'p') {
                self.pos += 1;
                let name_ident = self.parse_undisambiguated_ident()?;
                let ty = self.parse_type()?;
                let name = match name_ident {
                    Ident::Raw(s) => s,
                    Ident::Decoded(_) => "?",
                };
                assoc.push(AssocBinding { name, ty });
            }
            bounds.push(DynBound { trait_path, assoc });
        }
        self.pos += 1;
        Ok(bounds)
    }

    fn parse_const(&mut self) -> Result<Const<'a>, ParseError> {
        self.enter()?;
        let start = self.pos;
        let b = self.peek().ok_or(self.err(ParseErrorKind::UnexpectedEof))?;
        let c = match b {
            b'B' => {
                self.pos += 1;
                let offset = self.parse_base62()?;
                let resolved = self.lookup_backref_const(offset as usize)?;
                self.leave();
                return Ok((*resolved).clone());
            }
            b'p' => {
                self.pos += 1;
                Const::Placeholder
            }
            _ => {
                // Primitive prefix indicates the type, then the
                // value bytes follow up to `_`.
                let prim = primitive_for_tag(b)
                    .ok_or_else(|| self.err(ParseErrorKind::InvalidConst))?;
                self.pos += 1;
                if prim == Primitive::Bool {
                    let body = self.read_until_underscore()?;
                    match body {
                        "0" => Const::Bool(false),
                        "1" => Const::Bool(true),
                        _ => return Err(self.err(ParseErrorKind::InvalidConst)),
                    }
                } else if prim == Primitive::Char {
                    let body = self.read_until_underscore()?;
                    let cp = u32::from_str_radix(body, 16)
                        .map_err(|_| self.err(ParseErrorKind::InvalidConst))?;
                    Const::Char(
                        char::from_u32(cp).ok_or_else(|| self.err(ParseErrorKind::InvalidConst))?,
                    )
                } else if prim == Primitive::Str {
                    let body = self.read_until_underscore()?;
                    Const::Str(body)
                } else {
                    let negative = self.peek() == Some(b'n');
                    if negative {
                        self.pos += 1;
                    }
                    let body = self.read_until_underscore()?;
                    let mag = i128::from_str_radix(body, 16)
                        .map_err(|_| self.err(ParseErrorKind::InvalidConst))?;
                    let value = if negative { -mag } else { mag };
                    Const::Int { ty: prim, value }
                }
            }
        };
        let c_rc = Rc::new(c.clone());
        self.backrefs.push((start, Parsed::Const(c_rc)));
        self.leave();
        Ok(c)
    }

    /// Read input until the next `_`, return the body as a `&str`.
    /// Advances the cursor past the terminator.
    fn read_until_underscore(&mut self) -> Result<&'a str, ParseError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b'_' {
                let body = &self.input[start..self.pos];
                self.pos += 1;
                return core::str::from_utf8(body).map_err(|_| ParseError {
                    kind: ParseErrorKind::InvalidConst,
                    byte_offset: start,
                });
            }
            self.pos += 1;
        }
        Err(self.err(ParseErrorKind::UnexpectedEof))
    }
}

/// v0 §basic-type → primitive byte → keyword.
fn primitive_for_tag(b: u8) -> Option<Primitive> {
    Some(match b {
        b'a' => Primitive::I8,
        b'b' => Primitive::Bool,
        b'c' => Primitive::Char,
        b'd' => Primitive::F64,
        b'e' => Primitive::Str,
        b'f' => Primitive::F32,
        b'h' => Primitive::U8,
        b'i' => Primitive::Isize,
        b'j' => Primitive::Usize,
        b'l' => Primitive::I32,
        b'm' => Primitive::U32,
        b'n' => Primitive::I128,
        b'o' => Primitive::U128,
        b'p' => Primitive::Placeholder,
        b's' => Primitive::I16,
        b't' => Primitive::U16,
        b'u' => Primitive::Unit,
        b'x' => Primitive::I64,
        b'y' => Primitive::U64,
        b'z' => Primitive::Never,
        _ => return None,
    })
}

/// RFC 3492 Punycode decoder, modified per RFC 2603 (delimiter `_`
/// instead of `-`). Returns the decoded UTF-8 string. Returns `None`
/// on any malformed input.
fn decode_punycode(input: &str) -> Option<String> {
    // Standard Punycode parameters.
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;
    const INITIAL_BIAS: u32 = 72;
    const INITIAL_N: u32 = 128;

    let bytes = input.as_bytes();
    // Find the last `_` — everything before it is the literal ASCII
    // basic-codepoints prefix; everything after is the Punycode-
    // encoded extended-codepoints body.
    let split = bytes.iter().rposition(|&b| b == b'_');

    let (literal, encoded) = match split {
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (&[][..], bytes),
    };

    let mut output: Vec<u32> = literal.iter().map(|&b| b as u32).collect();

    let mut n = INITIAL_N;
    let mut i: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut idx = 0;
    while idx < encoded.len() {
        let oldi = i;
        let mut w: u32 = 1;
        let mut k: u32 = BASE;
        loop {
            if idx >= encoded.len() {
                return None;
            }
            let digit = digit_value(encoded[idx])?;
            idx += 1;
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias + TMIN {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k = k.checked_add(BASE)?;
        }
        let out_len = (output.len() + 1) as u32;
        bias = adapt(i - oldi, out_len, oldi == 0, DAMP, BASE, TMIN, TMAX, SKEW);
        n = n.checked_add(i / out_len)?;
        i %= out_len;
        if let Some(c) = char::from_u32(n) {
            output.insert(i as usize, c as u32);
        } else {
            return None;
        }
        i += 1;
    }

    let mut s = String::with_capacity(output.len());
    for cp in output {
        s.push(char::from_u32(cp)?);
    }
    Some(s)
}

fn digit_value(b: u8) -> Option<u32> {
    Some(match b {
        b'a'..=b'z' => (b - b'a') as u32,
        b'A'..=b'Z' => (b - b'A') as u32,
        b'0'..=b'9' => 26 + (b - b'0') as u32,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn adapt(delta: u32, num_points: u32, first_time: bool, damp: u32, base: u32, tmin: u32, tmax: u32, skew: u32) -> u32 {
    let mut delta = if first_time { delta / damp } else { delta / 2 };
    delta += delta / num_points;
    let mut k: u32 = 0;
    while delta > ((base - tmin) * tmax) / 2 {
        delta /= base - tmin;
        k += base;
    }
    k + (((base - tmin + 1) * delta) / (delta + skew))
}

/// Parse a v0 symbol (`_R…` or `R…`).
pub(crate) fn parse(s: &str) -> Result<Path<'_>, ParseError> {
    let body = if let Some(b) = s.strip_prefix("_R") {
        b
    } else if let Some(b) = s.strip_prefix('R') {
        b
    } else {
        return Err(ParseError {
            kind: ParseErrorKind::NotMangled,
            byte_offset: 0,
        });
    };
    let mut p = Parser::new(body.as_bytes());
    p.parse_path_top()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn parse_crate_root() {
        let p = parse("_RC4core").unwrap();
        assert_eq!(p.crate_name(), Some("core"));
        assert_eq!(format!("{p}"), "core");
    }

    #[test]
    fn parse_crate_root_with_disambiguator() {
        // C s _ 4core → crate_disambiguator = 0, name "core"
        let p = parse("_RCs_4core").unwrap();
        assert_eq!(p.crate_name(), Some("core"));
    }

    #[test]
    fn parse_nested_path() {
        // N v <crate> 5write → core::write (value namespace)
        let p = parse("_RNvC4core5write").unwrap();
        assert_eq!(format!("{p}"), "core::write");
    }

    #[test]
    fn parse_unknown_tag_errors() {
        assert!(parse("_RZ4core").is_err());
    }

    #[test]
    fn r_prefix_no_underscore_works() {
        let p = parse("RC4core").unwrap();
        assert_eq!(format!("{p}"), "core");
    }

    #[test]
    fn primitive_table_complete() {
        for b in b'a'..=b'z' {
            // Must not panic on any letter — either Some primitive
            // or None.
            let _ = primitive_for_tag(b);
        }
    }

    #[test]
    fn punycode_basic() {
        // `_` only → empty literal + empty encoded. RFC test vector
        // for `_xn--7czh` style is overkill; verify the shape.
        let s = decode_punycode("hello_").unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn no_panic_on_garbage_v0() {
        for input in &[
            "_R", "_RX", "_RC", "_RNvB_", "_RB_", "_RIB_E", "_RNvC", "_RNvC0",
        ] {
            let _ = parse(input);
        }
    }
}

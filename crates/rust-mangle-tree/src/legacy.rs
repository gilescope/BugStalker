// SPDX-License-Identifier: MIT OR Apache-2.0
//! Legacy Itanium-style Rust symbol parser.
//!
//! Legacy symbols look like:
//!
//! ```text
//! _ZN<len>name<len>name…<len>name17h<16hex>E
//! ```
//!
//! Each `<len>name` is a length-prefixed identifier. The trailing
//! `17h<16hex>` is rustc's per-monomorphisation hash. The simpler
//! `_ZN…E` shape (no hash) is also legal — older compilers and
//! hand-mangled C exports.
//!
//! Identifier bodies aren't strictly ASCII source: rustc legacy-
//! mangles non-identifier characters with `$XX$` escapes (`$LT$` →
//! `<`, `$GT$` → `>`, `$u20$` → space, …), maps source `::` to `..`
//! and `-` to `.` Display undoes those mappings.

use crate::{ParseError, ParseErrorKind};
use alloc::vec::Vec;
use core::fmt;

/// A parsed legacy symbol. Carries the segment list (raw, still
/// `$XX$`-escaped — Display does the decoding) and the optional
/// trailing per-mono `17h…` hash. v0 generic args are not encoded
/// in this scheme; callers that need them must mangle to v0 first.
#[derive(Debug, Clone)]
pub struct LegacyPath<'a> {
    /// `crate::module::function` segments, outermost → innermost.
    /// Stored *escaped*; [`Display`] decodes on render.
    pub(crate) segments: Vec<&'a str>,
    /// Trailing per-mono hash, when present. Stored without the
    /// `len` prefix but with the leading `h` retained (e.g.
    /// `"h0123456789abcdef"`).
    pub(crate) hash: Option<&'a str>,
    /// Trailing LLVM thunk decoration after the `E` terminator.
    /// rustc / llvm sometimes append `.123` or `.llvm.456` to
    /// distinguish multiple monomorphisations of the same fn that
    /// the linker must keep separate. Empty string when absent.
    pub(crate) suffix: &'a str,
}

impl<'a> LegacyPath<'a> {
    /// Outermost segment — usually the crate name. Raw (escaped).
    pub fn crate_name(&self) -> Option<&'a str> {
        self.segments.first().copied()
    }

    /// Walk segments outermost → innermost. Raw (escaped).
    pub fn segments(&self) -> impl Iterator<Item = &&'a str> {
        self.segments.iter()
    }

    /// Trailing per-mono hash, if any. Carries the literal `h…`
    /// string; strip the leading `h` to get just the hex.
    pub fn hash(&self) -> Option<&'a str> {
        self.hash
    }
}

impl fmt::Display for LegacyPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `:#` short form: drop the trailing per-mono hash.
        let _alt = f.alternate();
        for (i, seg) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str("::")?;
            }
            decode_segment_into(seg, f)?;
        }
        if !self.suffix.is_empty() {
            f.write_str(self.suffix)?;
        }
        Ok(())
    }
}

/// Parse a legacy `_ZN…E` symbol.
pub(crate) fn parse(s: &str) -> Result<LegacyPath<'_>, ParseError> {
    let (prefix_len, body) = if let Some(b) = s.strip_prefix("_ZN") {
        (3, b)
    } else if let Some(b) = s.strip_prefix("__ZN") {
        (4, b)
    } else {
        return Err(ParseError {
            kind: ParseErrorKind::NotMangled,
            byte_offset: 0,
        });
    };

    let mut segments = Vec::new();
    let mut pos: usize = 0;
    let bytes = body.as_bytes();

    loop {
        // Read the length prefix — a run of ASCII digits.
        let len_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == len_start {
            // No digits: expect terminator `E`.
            if bytes.get(pos) == Some(&b'E') {
                pos += 1;
                break;
            }
            return Err(ParseError {
                kind: if pos >= bytes.len() {
                    ParseErrorKind::UnexpectedEof
                } else {
                    ParseErrorKind::Syntax
                },
                byte_offset: prefix_len + pos,
            });
        }
        // SAFETY: the slice is all ASCII digits.
        let digits = match core::str::from_utf8(&bytes[len_start..pos]) {
            Ok(s) => s,
            Err(_) => {
                return Err(ParseError {
                    kind: ParseErrorKind::Syntax,
                    byte_offset: prefix_len + len_start,
                });
            }
        };
        let len: usize = digits.parse().map_err(|_| ParseError {
            kind: ParseErrorKind::Syntax,
            byte_offset: prefix_len + len_start,
        })?;
        if pos.checked_add(len).map_or(true, |end| end > bytes.len()) {
            return Err(ParseError {
                kind: ParseErrorKind::TruncatedIdent,
                byte_offset: prefix_len + pos,
            });
        }
        let segment = &body[pos..pos + len];
        pos += len;
        // Identifier bytes must lie on a UTF-8 boundary (segments are
        // ASCII bar the `$uNN$` escapes, which are themselves ASCII).
        // If not, the input claims a length that splits a multi-byte
        // codepoint — treat as a syntax error rather than panicking
        // when we slice.
        if !body.is_char_boundary(pos) {
            return Err(ParseError {
                kind: ParseErrorKind::Syntax,
                byte_offset: prefix_len + pos,
            });
        }
        segments.push(segment);
    }

    // Trailing LLVM thunk decoration: `.<n>` or `.llvm.<n>` after
    // `E`. Carry it through so `Display` can re-emit it byte-for-
    // byte.
    let suffix = &body[pos..];

    // Promote the final `17h<16hex>` segment to `hash` when it
    // matches. Older compilers omit the hash entirely; that's a
    // valid legacy symbol with `hash = None`.
    let hash = match segments.last() {
        Some(last) if is_legacy_hash(last) => segments.pop(),
        _ => None,
    };

    Ok(LegacyPath {
        segments,
        hash,
        suffix,
    })
}

/// `17h<16 hex digits>` is the rustc per-mono hash signature.
fn is_legacy_hash(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    bytes.len() == 17 && bytes[0] == b'h' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

/// Decode the `$XX$` escapes rustc legacy-mangles non-identifier
/// characters with. Maps `..` → `::` and `.` → `-` so the rendered
/// output looks like the source. Mirrors the rules in
/// `rustc-demangle::legacy::demangle` so our Display matches it
/// byte-for-byte.
///
/// One subtlety: rustc inserts a syntactic `_` at the start of
/// synthetic segments that begin with a non-identifier character
/// (typical example: `<&mut Foo as Bar>::method` segments mangle
/// to `_$LT$$RF$mut$u20$Foo…$GT$`). The leading `_` is a placeholder
/// — it has no source meaning — so we strip it to match
/// rustc-demangle.
fn decode_segment_into(seg: &str, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let bytes = seg.as_bytes();
    // Detect and skip the leading-`_` placeholder: present when the
    // segment starts with `_$` (escape immediately following the
    // underscore).
    let start = if bytes.starts_with(b"_$") { 1 } else { 0 };
    let seg = &seg[start..];
    let mut iter = seg.bytes().enumerate().peekable();
    while let Some((i, b)) = iter.next() {
        match b {
            b'$' => {
                // Find the closing `$` and decode the inner code.
                let rest = &seg[i + 1..];
                if let Some(end) = rest.find('$') {
                    let inner = &rest[..end];
                    if let Some(s) = decode_escape(inner) {
                        f.write_str(s)?;
                        // Consume bytes through the closing `$`.
                        for _ in 0..(end + 1) {
                            iter.next();
                        }
                        continue;
                    }
                    if let Some(c) = decode_unicode_escape(inner) {
                        let mut buf = [0u8; 4];
                        f.write_str(c.encode_utf8(&mut buf))?;
                        for _ in 0..(end + 1) {
                            iter.next();
                        }
                        continue;
                    }
                }
                // Unknown escape: emit the literal `$`.
                f.write_str("$")?;
            }
            b'.' => {
                // `..` → `::`; lone `.` is a literal period in
                // modern legacy output (rustc-demangle preserves
                // it). The legacy `.` → `-` mapping only applied
                // to very old rustc; rustc-demangle dropped it
                // years ago and we match.
                if let Some(&(_, b'.')) = iter.peek() {
                    iter.next();
                    f.write_str("::")?;
                } else {
                    f.write_str(".")?;
                }
            }
            _ => {
                // ASCII bytes pass through; non-ASCII can't occur
                // inside legacy identifiers (the `$uNN$` escape is
                // the encoding for non-ASCII).
                let mut buf = [0u8; 4];
                if b < 0x80 {
                    buf[0] = b;
                    f.write_str(core::str::from_utf8(&buf[..1]).unwrap_or(""))?;
                } else {
                    // Defensive: write the byte-aligned char if any.
                    f.write_str("?")?;
                }
            }
        }
    }
    Ok(())
}

/// Static escape table — `$LT$` → `<`, etc. Matches the table in
/// rustc-demangle's legacy demangle.
fn decode_escape(code: &str) -> Option<&'static str> {
    Some(match code {
        "SP" => "@",
        "BP" => "*",
        "RF" => "&",
        "LT" => "<",
        "GT" => ">",
        "LP" => "(",
        "RP" => ")",
        "C" => ",",
        _ => return None,
    })
}

/// Decode a `$uNN$` Unicode-escape body — `NN` is hex, lowercase
/// usually but we accept either case.
fn decode_unicode_escape(code: &str) -> Option<char> {
    let hex = code.strip_prefix('u')?;
    let cp = u32::from_str_radix(hex, 16).ok()?;
    char::from_u32(cp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Symbol};

    fn roundtrip(input: &str, expected: &str) {
        let parsed = parse(input).unwrap_or_else(|e| panic!("parse({input}): {e}"));
        let rendered = alloc::format!("{parsed}");
        assert_eq!(rendered, expected, "input={input}");
    }

    #[test]
    fn basic_two_segments() {
        roundtrip("_ZN3foo3barE", "foo::bar");
    }

    #[test]
    fn trailing_hash_extracted() {
        let sym = "_ZN3foo3bar17h0123456789abcdefE";
        match parse(sym).unwrap() {
            Symbol::Legacy(p) => {
                assert_eq!(p.hash(), Some("h0123456789abcdef"));
                assert_eq!(alloc::format!("{p}"), "foo::bar");
            }
            other => panic!("expected Legacy, got {other:?}"),
        }
    }

    #[test]
    fn double_underscore_zn_prefix() {
        roundtrip("__ZN3foo3barE", "foo::bar");
    }

    #[test]
    fn escapes_decoded() {
        // Identifier `foo<ibar>` legacy-mangles to `foo$LT$ibar$GT$`
        // — 15 bytes — so the length prefix is 15.
        roundtrip("_ZN15foo$LT$ibar$GT$E", "foo<ibar>");
    }

    #[test]
    fn double_dot_becomes_double_colon() {
        // `a::b` mangles to `a..b` — 4 bytes — inside a single
        // segment (rustc emits this when the source path crosses
        // a generic boundary).
        roundtrip("_ZN4a..bE", "a::b");
    }

    #[test]
    fn unicode_escape() {
        // `a b` (with literal space) → `a$u20$b` — 7 bytes.
        roundtrip("_ZN7a$u20$bE", "a b");
    }

    #[test]
    fn missing_terminator_errors() {
        // `_ZN3foo` has the `_ZN` prefix and a complete first
        // segment but no `E`. `parse()` should surface an `Err`.
        let res = super::parse("_ZN3foo");
        assert!(matches!(
            res.as_ref().map_err(|e| &e.kind),
            Err(ParseErrorKind::UnexpectedEof | ParseErrorKind::Syntax)
        ));
    }

    #[test]
    fn truncated_ident_errors() {
        // Length 99 but only "fo" follows.
        let res = super::parse("_ZN99fo");
        assert!(matches!(
            res.as_ref().map_err(|e| &e.kind),
            Err(ParseErrorKind::TruncatedIdent | ParseErrorKind::Syntax)
        ));
    }

    #[test]
    fn empty_namespace_is_just_terminator() {
        // `_ZNE` — zero segments. Legal degenerate input.
        let res = super::parse("_ZNE");
        let p = res.unwrap();
        assert_eq!(p.segments.len(), 0);
        assert_eq!(alloc::format!("{p}"), "");
    }
}

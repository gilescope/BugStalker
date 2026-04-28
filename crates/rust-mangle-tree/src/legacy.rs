// SPDX-License-Identifier: MIT OR Apache-2.0
//! Legacy Itanium-style Rust symbol parser.
//!
//! Phase 2 batch A — public type only. Parser body lands in batch B.
//!
//! Legacy symbols look like:
//!
//! ```text
//! _ZN<len>name<len>name…<len>name17h<16hex>E
//! ```
//!
//! Each `<len>name` is a length-prefixed identifier. The trailing
//! `17h<16hex>E` is rustc's per-mono hash. The simpler `_ZN…E`
//! shape (no hash) and the older `$hash$` separator both predate
//! this scheme and are still seen in the wild on legacy binaries.

use crate::{ParseError, ParseErrorKind};
use alloc::vec::Vec;
use core::fmt;

/// A parsed legacy symbol. Carries the segment list and the optional
/// trailing hash. v0 generic args are not encoded in this scheme;
/// callers that need them must mangle to v0 first.
#[derive(Debug, Clone)]
pub struct LegacyPath<'a> {
    /// `crate::module::function` segments, outermost → innermost.
    pub(crate) segments: Vec<&'a str>,
    /// Trailing per-mono `17h…` hash, when present.
    pub(crate) hash: Option<&'a str>,
}

impl<'a> LegacyPath<'a> {
    /// Outermost segment — usually the crate name.
    pub fn crate_name(&self) -> Option<&'a str> {
        self.segments.first().copied()
    }

    /// Walk segments outermost → innermost.
    pub fn segments(&self) -> impl Iterator<Item = &&'a str> {
        self.segments.iter()
    }

    /// Trailing per-mono hash, if any. Carries the literal string
    /// (e.g. `"h0123456789abcdef"`); strip the leading `h` to get
    /// just the hex.
    pub fn hash(&self) -> Option<&'a str> {
        self.hash
    }
}

impl fmt::Display for LegacyPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str("::")?;
            }
            f.write_str(seg)?;
        }
        Ok(())
    }
}

/// Parse a legacy `_ZN…E` symbol. Phase 2 batch A stub — returns the
/// generic syntax error. Batch B fills in the real parser.
pub(crate) fn parse(s: &str) -> Result<LegacyPath<'_>, ParseError> {
    let body = s
        .strip_prefix("_ZN")
        .or_else(|| s.strip_prefix("__ZN"))
        .ok_or(ParseError {
            kind: ParseErrorKind::NotMangled,
            byte_offset: 0,
        })?;
    let _body = body;
    // TODO(phase 2 batch B): real parser.
    Err(ParseError {
        kind: ParseErrorKind::Syntax,
        byte_offset: 0,
    })
}

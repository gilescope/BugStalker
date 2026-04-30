// SPDX-License-Identifier: MIT
//! Wire format for the `.bs_viz_spec` binary section.
//!
//! Phase 4 step 1 carries a deliberately tiny subset of
//! `TypeViewSpec`: type name + optional `summary` template +
//! per-field hidden / rename / format markers. Future batches
//! extend the format additively (new optional fields → bump the
//! `version` byte and gate readers).
//!
//! ## Layout
//!
//! Each entry in the section is length-prefixed so the loader
//! can stream multiple specs from a single section without
//! pre-knowing how many derives a translation unit emitted.
//!
//! ```text
//! entry := u32 magic = 0x42_53_56_31 ("BSV1")  -- "BugStalker Viz v1"
//!        u8   version = 1
//!        u32  payload_len
//!        bytes[payload_len] payload
//!
//! payload := str type_name
//!          opt-str summary
//!          u32 num_fields
//!          field[num_fields]
//!
//! field := str name
//!        opt-str rename
//!        u8 hidden = 0|1
//!        u8 format    -- enum tag, see `Format::*`
//!
//! str     := u32 len + bytes[len]
//! opt-str := u8 present + str (when present == 1)
//! ```
//!
//! Numbers are little-endian. The format is *not* self-describing
//! beyond the magic/version pair — that is intentional: this is a
//! private contract between `bs-viz-derive` and BugStalker, not a
//! public interchange format. Bump the version every time the
//! payload layout changes.

#![deny(rustdoc::broken_intra_doc_links)]

use thiserror::Error;

/// Magic bytes at the head of every entry. Lets the loader
/// distinguish a real spec record from arbitrary debuggee bytes
/// that happen to land in a section with our name.
pub const MAGIC: [u8; 4] = *b"BSV1";

/// Wire-format version. Bump on every breaking layout change;
/// readers must reject unknown versions and skip the entry.
pub const VERSION: u8 = 1;

/// Per-field display format override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Format {
    /// No override — render as the field's type would normally
    /// render.
    Default = 0,
    /// Hex (`0x...`).
    Hex = 1,
    /// Binary (`0b...`).
    Bin = 2,
    /// Octal (`0o...`).
    Oct = 3,
    /// ISO-8601 timestamp (only meaningful for time types).
    Iso8601 = 4,
    /// Human-readable duration (only meaningful for `Duration`).
    Duration = 5,
    /// Force UTF-8 string interpretation (only meaningful for
    /// byte-array-shaped types).
    Utf8 = 6,
    /// Hex dump (binary-blob style display).
    Hexdump = 7,
}

impl Format {
    pub fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::Default,
            1 => Self::Hex,
            2 => Self::Bin,
            3 => Self::Oct,
            4 => Self::Iso8601,
            5 => Self::Duration,
            6 => Self::Utf8,
            7 => Self::Hexdump,
            _ => return None,
        })
    }
}

/// Per-field spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: String,
    pub rename: Option<String>,
    pub hidden: bool,
    pub format: Format,
}

/// Top-level spec for one `#[derive(DebugView)]` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeViewSpec {
    /// Fully qualified type name as the v0 demangler renders it
    /// (e.g. `my_crate::Person`). Generic instantiations register
    /// distinct entries — one per monomorphisation — so this
    /// always matches a concrete demangled name.
    pub type_name: String,
    /// Optional `summary = "..."` template. Placeholders are
    /// `{field_name}`; literal `{` / `}` produced by `{{` / `}}`.
    /// Templates are only parsed at render time — encoded
    /// verbatim here.
    pub summary: Option<String>,
    /// Per-field overrides. Empty for unit / tuple structs in
    /// step 1.
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("truncated input: needed {needed} more bytes for {what}")]
    Truncated {
        needed: usize,
        what: &'static str,
    },
    #[error("bad magic: expected {:?}, got {got:?}", MAGIC)]
    BadMagic { got: [u8; 4] },
    #[error("unsupported version {got}; this build understands {VERSION}")]
    UnsupportedVersion { got: u8 },
    #[error("invalid utf-8 in {what}")]
    InvalidUtf8 { what: &'static str },
    #[error("unknown format tag {tag}")]
    UnknownFormatTag { tag: u8 },
}

/// Encode one spec into its on-wire form (entry header +
/// payload). Used by the proc-macro at expansion time; the
/// resulting bytes are emitted as a `static` byte literal.
pub fn encode(spec: &TypeViewSpec) -> Vec<u8> {
    let mut payload = Vec::new();
    write_str(&mut payload, &spec.type_name);
    write_opt_str(&mut payload, spec.summary.as_deref());
    let n_fields: u32 = spec
        .fields
        .len()
        .try_into()
        .expect("more than u32::MAX fields is impossible in practice");
    payload.extend_from_slice(&n_fields.to_le_bytes());
    for f in &spec.fields {
        write_str(&mut payload, &f.name);
        write_opt_str(&mut payload, f.rename.as_deref());
        payload.push(f.hidden as u8);
        payload.push(f.format as u8);
    }

    let mut out = Vec::with_capacity(4 + 1 + 4 + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    let payload_len: u32 = payload
        .len()
        .try_into()
        .expect("a single spec payload exceeding u32::MAX bytes is implausible");
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode one spec entry starting at `bytes[0]`. Returns the
/// parsed spec and the number of bytes consumed; the caller can
/// advance the slice and repeat to stream a multi-entry section.
pub fn decode_one(bytes: &[u8]) -> Result<(TypeViewSpec, usize), DecodeError> {
    let mut r = Reader::new(bytes);
    let magic = r.read_array::<4>("magic")?;
    if magic != MAGIC {
        return Err(DecodeError::BadMagic { got: magic });
    }
    let version = r.read_u8("version")?;
    if version != VERSION {
        return Err(DecodeError::UnsupportedVersion { got: version });
    }
    let payload_len = r.read_u32("payload_len")? as usize;
    let payload_start = r.pos;
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or(DecodeError::Truncated {
            needed: payload_len,
            what: "payload",
        })?;
    if payload_end > bytes.len() {
        return Err(DecodeError::Truncated {
            needed: payload_end - bytes.len(),
            what: "payload",
        });
    }
    let mut p = Reader::new(&bytes[payload_start..payload_end]);
    let type_name = p.read_str("type_name")?;
    let summary = p.read_opt_str("summary")?;
    let n_fields = p.read_u32("n_fields")? as usize;
    let mut fields = Vec::with_capacity(n_fields);
    for _ in 0..n_fields {
        let name = p.read_str("field.name")?;
        let rename = p.read_opt_str("field.rename")?;
        let hidden = p.read_u8("field.hidden")? != 0;
        let fmt_tag = p.read_u8("field.format")?;
        let format =
            Format::from_tag(fmt_tag).ok_or(DecodeError::UnknownFormatTag { tag: fmt_tag })?;
        fields.push(FieldSpec {
            name,
            rename,
            hidden,
            format,
        });
    }

    Ok((
        TypeViewSpec {
            type_name,
            summary,
            fields,
        },
        payload_end,
    ))
}

/// Decode every spec entry in `bytes`. Stops at the first
/// malformed entry (returning what was parsed before plus the
/// error) so a corrupt section doesn't poison the entire
/// registry.
pub fn decode_all(bytes: &[u8]) -> (Vec<TypeViewSpec>, Option<DecodeError>) {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match decode_one(&bytes[cursor..]) {
            Ok((spec, consumed)) => {
                cursor += consumed;
                out.push(spec);
            }
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

// ---------------- internal io helpers ----------------

fn write_str(out: &mut Vec<u8>, s: &str) {
    let len: u32 = s
        .len()
        .try_into()
        .expect("string longer than u32::MAX in a viz spec is implausible");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => {
            out.push(1);
            write_str(out, s);
        }
        None => out.push(0),
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn need(&self, n: usize, what: &'static str) -> Result<(), DecodeError> {
        if self.pos.saturating_add(n) > self.buf.len() {
            Err(DecodeError::Truncated {
                needed: self.pos + n - self.buf.len(),
                what,
            })
        } else {
            Ok(())
        }
    }

    fn read_u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        self.need(1, what)?;
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_u32(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        self.need(4, what)?;
        let bytes = &self.buf[self.pos..self.pos + 4];
        self.pos += 4;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], DecodeError> {
        self.need(N, what)?;
        let mut a = [0u8; N];
        a.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        Ok(a)
    }

    fn read_str(&mut self, what: &'static str) -> Result<String, DecodeError> {
        let len = self.read_u32(what)? as usize;
        self.need(len, what)?;
        let bytes = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        std::str::from_utf8(bytes)
            .map(|s| s.to_owned())
            .map_err(|_| DecodeError::InvalidUtf8 { what })
    }

    fn read_opt_str(&mut self, what: &'static str) -> Result<Option<String>, DecodeError> {
        match self.read_u8(what)? {
            0 => Ok(None),
            1 => Ok(Some(self.read_str(what)?)),
            other => {
                // Treat as no Option-tag: be strict and report.
                Err(DecodeError::UnknownFormatTag { tag: other })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TypeViewSpec {
        TypeViewSpec {
            type_name: "my_crate::Person".to_string(),
            summary: Some("Person({name}, age {age})".to_string()),
            fields: vec![
                FieldSpec {
                    name: "name".to_string(),
                    rename: None,
                    hidden: false,
                    format: Format::Default,
                },
                FieldSpec {
                    name: "age".to_string(),
                    rename: None,
                    hidden: false,
                    format: Format::Default,
                },
                FieldSpec {
                    name: "private_token".to_string(),
                    rename: None,
                    hidden: true,
                    format: Format::Hexdump,
                },
                FieldSpec {
                    name: "created".to_string(),
                    rename: Some("when".to_string()),
                    hidden: false,
                    format: Format::Iso8601,
                },
            ],
        }
    }

    #[test]
    fn roundtrip_one() {
        let s = sample();
        let bytes = encode(&s);
        let (back, consumed) = decode_one(&bytes).unwrap();
        assert_eq!(back, s);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn roundtrip_many_concatenated() {
        let s1 = sample();
        let s2 = TypeViewSpec {
            type_name: "other::Thing".to_string(),
            summary: None,
            fields: vec![],
        };
        let mut buf = Vec::new();
        buf.extend(encode(&s1));
        buf.extend(encode(&s2));
        let (specs, err) = decode_all(&buf);
        assert!(err.is_none(), "unexpected: {err:?}");
        assert_eq!(specs, vec![s1, s2]);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(&sample());
        bytes[0] = 0xFF;
        assert!(matches!(decode_one(&bytes), Err(DecodeError::BadMagic { .. })));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = encode(&sample());
        bytes[4] = 99; // version byte
        assert!(matches!(
            decode_one(&bytes),
            Err(DecodeError::UnsupportedVersion { got: 99 })
        ));
    }

    #[test]
    fn truncated_payload_reports_clear_error() {
        let bytes = encode(&sample());
        let truncated = &bytes[..bytes.len() - 5];
        assert!(matches!(
            decode_one(truncated),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn decode_all_stops_at_corruption_but_keeps_prefix() {
        let s1 = sample();
        let mut buf = encode(&s1);
        // Append a deliberately broken entry.
        buf.extend_from_slice(b"\xFF\xFF\xFF\xFF\xFF");
        let (specs, err) = decode_all(&buf);
        assert_eq!(specs, vec![s1]);
        assert!(err.is_some());
    }
}

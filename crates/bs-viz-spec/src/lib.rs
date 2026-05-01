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
//!        u8   version = 2
//!        u32  payload_len
//!        bytes[payload_len] payload
//!
//! payload := str type_name
//!          opt-str summary
//!          u32 num_fields
//!          field[num_fields]
//!          u32 num_variants               -- v2: 0 for non-enums
//!          variant[num_variants]          -- v2 only
//!
//! field := str name
//!        opt-str rename
//!        u8 hidden = 0|1
//!        u8 format    -- enum tag, see `Format::*`
//!
//! variant := str name
//!          opt-str summary
//!          opt-str tag
//!          u32 num_fields
//!          field[num_fields]              -- per-variant overrides
//!
//! str     := u32 len + bytes[len]
//! opt-str := u8 present + str (when present == 1)
//! ```
//!
//! **Version 2 changes vs. v1:** appended `num_variants +
//! variant[]` block at the end of payload. The wire-format
//! version bumped to keep the contract a single-source-of-truth
//! check; readers that understand v2 also know to expect the
//! variant block. Pre-v2 readers reject v2 entries cleanly via
//! `UnsupportedVersion`, so a stale BugStalker against a fresh
//! debuggee fails fast rather than silently misinterpreting.
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
pub const VERSION: u8 = 2;

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

    /// Stable lower-case wire string. Used by the DAP
    /// `bs/visualiserList` response and any other JSON
    /// surface that surfaces format tags to clients.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Hex => "hex",
            Self::Bin => "bin",
            Self::Oct => "oct",
            Self::Iso8601 => "iso8601",
            Self::Duration => "duration",
            Self::Utf8 => "utf8",
            Self::Hexdump => "hexdump",
        }
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

/// Per-variant spec entry on an enum (v2+). `name` is the
/// variant's identifier as it appears in source (e.g.
/// `Connected`); the renderer matches on this when dispatching
/// at the `RustEnum.value.field_name` level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantSpec {
    pub name: String,
    /// Variant-specific `summary = "..."` template that
    /// overrides the type-level one when this variant is
    /// active. Placeholder semantics identical to type-level.
    pub summary: Option<String>,
    /// Phase 4 plan §"Variant-level" — `tag = "..."` lets a
    /// variant carry a state-tag string (e.g. `Connected`,
    /// `Offline`) for renderers that surface it as a colour
    /// chip / status icon. Stored verbatim; consumers decide
    /// presentation.
    pub tag: Option<String>,
    /// Per-field overrides scoped to this variant. Field
    /// names follow the same convention as struct fields
    /// (`__0`, `__1` for tuple variants, named fields for
    /// struct variants).
    pub fields: Vec<FieldSpec>,
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
    /// step 1, and for enums (per-field overrides on enums live
    /// inside `variants[].fields`).
    pub fields: Vec<FieldSpec>,
    /// Per-variant overrides for enums. Empty for non-enums.
    /// V2+ wire format only; older readers reject the entry.
    pub variants: Vec<VariantSpec>,
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

fn write_field(out: &mut Vec<u8>, f: &FieldSpec) {
    write_str(out, &f.name);
    write_opt_str(out, f.rename.as_deref());
    out.push(f.hidden as u8);
    out.push(f.format as u8);
}

fn write_field_list(out: &mut Vec<u8>, fields: &[FieldSpec]) {
    let n: u32 = fields
        .len()
        .try_into()
        .expect("more than u32::MAX fields is implausible");
    out.extend_from_slice(&n.to_le_bytes());
    for f in fields {
        write_field(out, f);
    }
}

/// Encode one spec into its on-wire form (entry header +
/// payload). Used by the proc-macro at expansion time; the
/// resulting bytes are emitted as a `static` byte literal.
pub fn encode(spec: &TypeViewSpec) -> Vec<u8> {
    let mut payload = Vec::new();
    write_str(&mut payload, &spec.type_name);
    payload.extend_from_slice(&encode_payload_suffix(spec));

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

/// Encode the post-`type_name` portion of the payload — summary,
/// fields, variants. Used by the proc-macro to pre-encode every
/// part of a spec entry that's known at expansion time. The
/// generated derive then assembles the final entry at user-crate
/// compile time via [`assemble_with_module_path`], plugging in
/// the `module_path!()`-resolved type-name prefix that the proc-
/// macro can't see.
pub fn encode_payload_suffix(spec: &TypeViewSpec) -> Vec<u8> {
    let mut out = Vec::new();
    write_opt_str(&mut out, spec.summary.as_deref());
    write_field_list(&mut out, &spec.fields);
    let n_variants: u32 = spec
        .variants
        .len()
        .try_into()
        .expect("more than u32::MAX variants is implausible");
    out.extend_from_slice(&n_variants.to_le_bytes());
    for v in &spec.variants {
        write_str(&mut out, &v.name);
        write_opt_str(&mut out, v.summary.as_deref());
        write_opt_str(&mut out, v.tag.as_deref());
        write_field_list(&mut out, &v.fields);
    }
    out
}

/// Const-fn version of `encode` that assembles a spec entry at
/// the user crate's compile time, plugging in a runtime-known
/// `module_path` + `local_name` pair as the recorded
/// `type_name`. Returns a fixed-size array so the bytes can sit
/// in a `static` placed in the `.bs_viz_spec` / `__bs_viz_spec`
/// section.
///
/// The total size `N` must equal:
///   `15 + module.len() + local.len() + suffix.len()`
/// — header (9) + type_name length prefix (4) + module bytes +
/// `"::"` separator (2) + local bytes + the suffix payload.
/// `bs-viz-derive` computes this via plain `const` arithmetic.
///
/// **`module` and `local` must each be valid UTF-8 byte slices.**
/// The const fn doesn't validate — callers in the proc-macro
/// always pass `&str` forms.
pub const fn assemble_with_module_path<const N: usize>(
    module: &str,
    local: &str,
    suffix: &[u8],
) -> [u8; N] {
    let mut out = [0u8; N];
    // Magic.
    out[0] = MAGIC[0];
    out[1] = MAGIC[1];
    out[2] = MAGIC[2];
    out[3] = MAGIC[3];
    // Version.
    out[4] = VERSION;
    // Payload length = N - 9. The compiler will fail to
    // construct the static if N < 9; const generics make that a
    // compile-time error.
    let payload_len = (N - 9) as u32;
    let pl = payload_len.to_le_bytes();
    out[5] = pl[0];
    out[6] = pl[1];
    out[7] = pl[2];
    out[8] = pl[3];

    // type_name length prefix (str: u32 len + bytes).
    let m = module.as_bytes();
    let l = local.as_bytes();
    let type_name_len = (m.len() + 2 + l.len()) as u32;
    let tnl = type_name_len.to_le_bytes();
    out[9] = tnl[0];
    out[10] = tnl[1];
    out[11] = tnl[2];
    out[12] = tnl[3];

    // Module bytes.
    let mut i = 0;
    while i < m.len() {
        out[13 + i] = m[i];
        i += 1;
    }
    // `::` separator.
    out[13 + m.len()] = b':';
    out[13 + m.len() + 1] = b':';
    // Local name bytes.
    let off = 13 + m.len() + 2;
    let mut i = 0;
    while i < l.len() {
        out[off + i] = l[i];
        i += 1;
    }
    // Suffix bytes (proc-macro-encoded summary + fields +
    // variants).
    let suf_off = off + l.len();
    let mut i = 0;
    while i < suffix.len() {
        out[suf_off + i] = suffix[i];
        i += 1;
    }
    out
}

/// Const-fn variant for the `name = "..."` override case where
/// the user supplied the full type name verbatim — no
/// `module_path!()` composition needed.
pub const fn assemble_verbatim<const N: usize>(
    type_name: &str,
    suffix: &[u8],
) -> [u8; N] {
    let mut out = [0u8; N];
    out[0] = MAGIC[0];
    out[1] = MAGIC[1];
    out[2] = MAGIC[2];
    out[3] = MAGIC[3];
    out[4] = VERSION;
    let payload_len = (N - 9) as u32;
    let pl = payload_len.to_le_bytes();
    out[5] = pl[0];
    out[6] = pl[1];
    out[7] = pl[2];
    out[8] = pl[3];

    let t = type_name.as_bytes();
    let type_name_len = t.len() as u32;
    let tnl = type_name_len.to_le_bytes();
    out[9] = tnl[0];
    out[10] = tnl[1];
    out[11] = tnl[2];
    out[12] = tnl[3];
    let mut i = 0;
    while i < t.len() {
        out[13 + i] = t[i];
        i += 1;
    }
    let suf_off = 13 + t.len();
    let mut i = 0;
    while i < suffix.len() {
        out[suf_off + i] = suffix[i];
        i += 1;
    }
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
    let fields = p.read_field_list("fields")?;
    // V2 variants block. The wire format always has it (length
    // 0 for non-enums), so an absent block means a corrupt
    // payload and the truncation error from `read_u32` is the
    // right signal.
    let n_variants = p.read_u32("n_variants")? as usize;
    let mut variants = Vec::with_capacity(n_variants);
    for _ in 0..n_variants {
        let name = p.read_str("variant.name")?;
        let v_summary = p.read_opt_str("variant.summary")?;
        let tag = p.read_opt_str("variant.tag")?;
        let v_fields = p.read_field_list("variant.fields")?;
        variants.push(VariantSpec {
            name,
            summary: v_summary,
            tag,
            fields: v_fields,
        });
    }

    Ok((
        TypeViewSpec {
            type_name,
            summary,
            fields,
            variants,
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

    fn read_field_list(&mut self, what: &'static str) -> Result<Vec<FieldSpec>, DecodeError> {
        let n = self.read_u32(what)? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.read_str("field.name")?;
            let rename = self.read_opt_str("field.rename")?;
            let hidden = self.read_u8("field.hidden")? != 0;
            let fmt_tag = self.read_u8("field.format")?;
            let format = Format::from_tag(fmt_tag)
                .ok_or(DecodeError::UnknownFormatTag { tag: fmt_tag })?;
            out.push(FieldSpec {
                name,
                rename,
                hidden,
                format,
            });
        }
        Ok(out)
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
            variants: vec![],
        }
    }

    fn enum_sample() -> TypeViewSpec {
        TypeViewSpec {
            type_name: "my_crate::Status".to_string(),
            summary: Some("Status[{__0}]".to_string()),
            fields: vec![],
            variants: vec![
                VariantSpec {
                    name: "Connected".to_string(),
                    summary: Some("✓ Connected (port {__0})".to_string()),
                    tag: Some("ok".to_string()),
                    fields: vec![FieldSpec {
                        name: "__0".to_string(),
                        rename: None,
                        hidden: false,
                        format: Format::Default,
                    }],
                },
                VariantSpec {
                    name: "Disconnected".to_string(),
                    summary: None,
                    tag: Some("warn".to_string()),
                    fields: vec![],
                },
                VariantSpec {
                    name: "Error".to_string(),
                    summary: Some("✗ Error: {__0}".to_string()),
                    tag: Some("err".to_string()),
                    fields: vec![],
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
            variants: vec![],
        };
        let mut buf = Vec::new();
        buf.extend(encode(&s1));
        buf.extend(encode(&s2));
        let (specs, err) = decode_all(&buf);
        assert!(err.is_none(), "unexpected: {err:?}");
        assert_eq!(specs, vec![s1, s2]);
    }

    #[test]
    fn roundtrip_enum_with_variants() {
        let s = enum_sample();
        let bytes = encode(&s);
        let (back, consumed) = decode_one(&bytes).unwrap();
        assert_eq!(back, s);
        assert_eq!(consumed, bytes.len());
        // Sanity: variants survived in order, with their full
        // attribute payload.
        assert_eq!(back.variants.len(), 3);
        assert_eq!(back.variants[0].tag.as_deref(), Some("ok"));
        assert_eq!(back.variants[1].summary, None);
        assert_eq!(back.variants[2].tag.as_deref(), Some("err"));
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
    fn assemble_with_module_path_matches_encode() {
        // Step 10: the const-fn assembly path must produce
        // *exactly* the same bytes as the runtime `encode`
        // function, given the same inputs. This is the contract
        // that lets the proc-macro generate a static array
        // initialised at user-crate compile time and have the
        // section reader at debug time decode it identically.
        let spec = TypeViewSpec {
            type_name: "my_crate::Person".to_string(),
            summary: Some("Person({name})".to_string()),
            fields: vec![FieldSpec {
                name: "name".to_string(),
                rename: None,
                hidden: false,
                format: Format::Default,
            }],
            variants: vec![],
        };
        let encoded = encode(&spec);
        let suffix = encode_payload_suffix(&spec);
        // Formula: header(9) + type_name_str_prefix(4) + module
        // bytes + "::"(2) + local bytes + suffix.
        let module = "my_crate";
        let local = "Person";
        const TOTAL: usize = 15 + "my_crate".len() + "Person".len() + 38;
        assert_eq!(
            TOTAL,
            encoded.len(),
            "size formula must match `encode` (suffix len was {})",
            suffix.len(),
        );
        let assembled: [u8; TOTAL] =
            assemble_with_module_path::<TOTAL>(module, local, &suffix);
        assert_eq!(&assembled[..], &encoded[..]);
    }

    #[test]
    fn assemble_verbatim_matches_encode() {
        let spec = TypeViewSpec {
            type_name: "qualified::Marker".to_string(),
            summary: Some("Marker#{__0}".to_string()),
            fields: vec![FieldSpec {
                name: "__0".to_string(),
                rename: None,
                hidden: false,
                format: Format::Default,
            }],
            variants: vec![],
        };
        let encoded = encode(&spec);
        let suffix = encode_payload_suffix(&spec);
        // Formula: header(9) + type_name_str_prefix(4) + name
        // bytes + suffix.
        const TOTAL: usize = 13 + "qualified::Marker".len() + 35;
        assert_eq!(
            TOTAL,
            encoded.len(),
            "size formula must match `encode` (suffix len was {})",
            suffix.len(),
        );
        let assembled: [u8; TOTAL] = assemble_verbatim::<TOTAL>(&spec.type_name, &suffix);
        assert_eq!(&assembled[..], &encoded[..]);
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

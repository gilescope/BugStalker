// SPDX-License-Identifier: MIT
use log::warn;

use crate::debugger::call::fmt::DebugFormattable;
use crate::debugger::variable::dqe::Dqe;
use crate::debugger::variable::execute::QueryResult;
use crate::debugger::variable::render::{ByteRenderMode, RenderValue};
use crate::debugger::variable::value::{SpecializedValue, SupportedScalar, Value};
use crate::debugger::{self, Debugger};
use crate::ui::command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenderMode {
    Builtin,
    Debug,
}

/// Phase 1 F4 — slash-suffix format spec on `print`/`var`/`argd`.
/// Applies to the top-level result of a path expression. Mismatched
/// type-vs-spec combinations fall back to the default render with a
/// `tracing::warn!` rather than erroring.
///
/// Recognised today:
/// - `/x`, `/b`, `/o`, `/d` — integer base for scalar integers.
/// - `/iso` — RFC3339/ISO-8601 form for `Duration` /
///   `SystemTime` / `Instant` (currently a no-op for SystemTime
///   since the default render is already RFC3339).
/// - `/utf8`, `/hex` — byte-slice S16 overrides. `/utf8` forces a
///   lossy utf-8 render (invalid bytes → `\u{FFFD}`); `/hex` forces
///   the 16-bytes-per-row hex dump with ASCII column.
///
/// Deferred to a future batch:
/// - `/p`, `/c`, `/s`, `/y`, `/[N]`, `/[N..M]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatSpec {
    Hex,
    Bin,
    Oct,
    Dec,
    Iso,
    /// Phase 1 S16 — force utf-8 byte-slice rendering with lossy
    /// decode. Invalid utf-8 sequences are substituted with
    /// `\u{FFFD}` rather than falling through to a hex dump.
    Utf8,
    /// Phase 1 S16 — force hex-dump byte-slice rendering even when
    /// the bytes are valid utf-8.
    BytesHex,
}

/// One parsed `print`-family command. `Variable` covers `var`/`vard`,
/// `Argument` covers `argd`/`arg`. Each carries the render mode (built-in
/// pretty render vs `Debug` impl), the parsed [`Dqe`] path expression, and
/// an optional [`FormatSpec`] from the trailing `/x`-style suffix.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Variable {
        mode: RenderMode,
        dqe: Dqe,
        format: Option<FormatSpec>,
    },
    Argument {
        mode: RenderMode,
        dqe: Dqe,
        format: Option<FormatSpec>,
    },
}

impl Command {
    fn render_mode(&self) -> RenderMode {
        match self {
            Command::Variable { mode, .. } => *mode,
            Command::Argument { mode, .. } => *mode,
        }
    }

    fn format(&self) -> Option<FormatSpec> {
        match self {
            Command::Variable { format, .. } => *format,
            Command::Argument { format, .. } => *format,
        }
    }
}

pub enum ReadVariableResult<'a> {
    PreRender(QueryResult<'a>, String),
    Raw(QueryResult<'a>),
}

pub struct Handler<'a> {
    dbg: &'a Debugger,
}

impl<'a> Handler<'a> {
    pub fn new(debugger: &'a Debugger) -> Self {
        Self { dbg: debugger }
    }

    pub fn handle(self, cmd: Command) -> command::CommandResult<Vec<ReadVariableResult<'a>>> {
        let render_mode = cmd.render_mode();
        let format = cmd.format();
        let read_result = match cmd {
            Command::Variable { dqe, .. } => self.dbg.read_variable(dqe)?,
            Command::Argument { dqe, .. } => self.dbg.read_argument(dqe)?,
        };

        // Phase 1 F4 — colon-suffix format spec. Apply at the
        // top-level of each result; mismatched type-vs-spec falls
        // back to the default render with a `warn!`.
        if let Some(spec) = format {
            return Ok(read_result
                .into_iter()
                .map(|qr| match apply_format_spec(qr.value(), spec) {
                    Some(rendered) => ReadVariableResult::PreRender(qr, rendered),
                    None => {
                        warn!(
                            target: "debugger",
                            "format spec {spec:?} doesn't apply to {} (type {}); using default render",
                            qr.identity(),
                            qr.value().r#type().name_fmt()
                        );
                        ReadVariableResult::Raw(qr)
                    }
                })
                .collect());
        }

        Ok(self.prepare_results(read_result, render_mode))
    }

    fn prepare_results(
        &self,
        read_result: Vec<QueryResult<'a>>,
        mode: RenderMode,
    ) -> Vec<ReadVariableResult<'a>> {
        let rr_iter = read_result.into_iter();
        if mode == RenderMode::Builtin {
            return rr_iter.map(ReadVariableResult::Raw).collect();
        }

        rr_iter
            .map(|qr| {
                // Call debug trait only for some types
                if qr.value().formattable()  {
                    match debugger::call::fmt::call_debug_fmt(self.dbg, &qr) {
                        Ok(s) => ReadVariableResult::PreRender(qr, s),
                        Err(e) => {
                            warn!(target: "debugger", "error {} while render variable {} using Debug trait, fallback to a builtin render", e, qr.identity());
                            ReadVariableResult::Raw(qr)
                        }
                    }
                } else {
                    ReadVariableResult::Raw(qr)
                }
            })
            .collect()
    }
}

/// Phase 1 F4 — apply a slash-suffix format spec to the top-level
/// of a value. Returns `None` when the spec doesn't match the value
/// shape (caller falls back to the default render with a warning).
fn apply_format_spec(val: &Value, spec: FormatSpec) -> Option<String> {
    match spec {
        FormatSpec::Hex => format_int_with_radix(val, 16, "0x"),
        FormatSpec::Bin => format_int_with_radix(val, 2, "0b"),
        FormatSpec::Oct => format_int_with_radix(val, 8, "0o"),
        FormatSpec::Dec => format_int_with_radix(val, 10, ""),
        FormatSpec::Iso => format_iso(val),
        FormatSpec::Utf8 => format_byte_slice(val, ByteRenderMode::ForceUtf8),
        FormatSpec::BytesHex => format_byte_slice(val, ByteRenderMode::ForceHex),
    }
}

/// Phase 1 S16 / F4 — pull the byte-slice members out of any of the
/// shapes that hold `[u8]` data and run them through the byte-preview
/// renderer with the caller-chosen mode. Recognises `Vec<u8>` /
/// `VecDeque<u8>` (specialised), `Box<[u8]>` (the `data_ptr`/`length`
/// fat-pointer struct can't be re-read here without the parser
/// context, so we accept the in-line `[u8; N]` arrays only) and bare
/// `[u8; N]` arrays. Falls back to `None` for non-byte shapes;
/// the print handler logs a `warn!` and uses the default render.
fn format_byte_slice(val: &Value, mode: ByteRenderMode) -> Option<String> {
    use crate::debugger::variable::value::SupportedScalar;
    use crate::debugger::variable::value::ArrayValue;
    let render_array = |arr: &ArrayValue| -> Option<String> {
        let items = arr.items.as_ref()?;
        if items.is_empty() {
            return Some("b\"\"".to_string());
        }
        match &items[0].value {
            Value::Scalar(s) if matches!(s.value, Some(SupportedScalar::U8(_))) => {}
            _ => return None,
        }
        let mut bytes: Vec<u8> = Vec::with_capacity(items.len().min(1024));
        for item in items.iter().take(1024) {
            let Value::Scalar(s) = &item.value else {
                return None;
            };
            match s.value {
                Some(SupportedScalar::U8(b)) => bytes.push(b),
                _ => return None,
            }
        }
        let truncated = items.len() > 1024;
        Some(crate::debugger::variable::render::render_bytes(
            &bytes, mode, truncated,
        ))
    };
    match val {
        Value::Specialized {
            value: Some(SpecializedValue::Vector(vec))
            | Some(SpecializedValue::VecDeque(vec)),
            ..
        } => crate::debugger::variable::render::render_byte_slice_members(
            vec.structure.members.as_ref(),
            mode,
        ),
        Value::Array(arr) => render_array(arr),
        _ => None,
    }
}

fn format_int_with_radix(val: &Value, radix: u32, prefix: &str) -> Option<String> {
    let Value::Scalar(s) = val else {
        return None;
    };
    let scalar = s.value.as_ref()?;
    // Render via the canonical signed/unsigned integer formats; rustc
    // formatters give us the radix-N output for free.
    let body = match (radix, scalar) {
        (10, _) => format!("{scalar}"),
        (16, SupportedScalar::I8(n)) => format!("{:x}", *n as u8),
        (16, SupportedScalar::I16(n)) => format!("{:x}", *n as u16),
        (16, SupportedScalar::I32(n)) => format!("{:x}", *n as u32),
        (16, SupportedScalar::I64(n)) => format!("{:x}", *n as u64),
        (16, SupportedScalar::I128(n)) => format!("{:x}", *n as u128),
        (16, SupportedScalar::Isize(n)) => format!("{:x}", *n as usize),
        (16, SupportedScalar::U8(n)) => format!("{n:x}"),
        (16, SupportedScalar::U16(n)) => format!("{n:x}"),
        (16, SupportedScalar::U32(n)) => format!("{n:x}"),
        (16, SupportedScalar::U64(n)) => format!("{n:x}"),
        (16, SupportedScalar::U128(n)) => format!("{n:x}"),
        (16, SupportedScalar::Usize(n)) => format!("{n:x}"),
        (8, SupportedScalar::I8(n)) => format!("{:o}", *n as u8),
        (8, SupportedScalar::I16(n)) => format!("{:o}", *n as u16),
        (8, SupportedScalar::I32(n)) => format!("{:o}", *n as u32),
        (8, SupportedScalar::I64(n)) => format!("{:o}", *n as u64),
        (8, SupportedScalar::I128(n)) => format!("{:o}", *n as u128),
        (8, SupportedScalar::Isize(n)) => format!("{:o}", *n as usize),
        (8, SupportedScalar::U8(n)) => format!("{n:o}"),
        (8, SupportedScalar::U16(n)) => format!("{n:o}"),
        (8, SupportedScalar::U32(n)) => format!("{n:o}"),
        (8, SupportedScalar::U64(n)) => format!("{n:o}"),
        (8, SupportedScalar::U128(n)) => format!("{n:o}"),
        (8, SupportedScalar::Usize(n)) => format!("{n:o}"),
        (2, SupportedScalar::I8(n)) => format!("{:b}", *n as u8),
        (2, SupportedScalar::I16(n)) => format!("{:b}", *n as u16),
        (2, SupportedScalar::I32(n)) => format!("{:b}", *n as u32),
        (2, SupportedScalar::I64(n)) => format!("{:b}", *n as u64),
        (2, SupportedScalar::I128(n)) => format!("{:b}", *n as u128),
        (2, SupportedScalar::Isize(n)) => format!("{:b}", *n as usize),
        (2, SupportedScalar::U8(n)) => format!("{n:b}"),
        (2, SupportedScalar::U16(n)) => format!("{n:b}"),
        (2, SupportedScalar::U32(n)) => format!("{n:b}"),
        (2, SupportedScalar::U64(n)) => format!("{n:b}"),
        (2, SupportedScalar::U128(n)) => format!("{n:b}"),
        (2, SupportedScalar::Usize(n)) => format!("{n:b}"),
        // Float / Bool / Char / Empty don't support hex/bin/oct.
        _ => return None,
    };
    Some(format!("{prefix}{body}"))
}

fn format_iso(val: &Value) -> Option<String> {
    // Iso applies to time-shaped specialised values. SystemTime
    // already renders RFC3339 by default (S5) so we re-emit; Duration
    // gets the ISO-8601 duration form `PT…`. Instant has no ISO form
    // (the absolute monotonic-clock reading isn't a calendar time).
    let Value::Specialized {
        value: Some(spec_val),
        ..
    } = val
    else {
        return None;
    };
    match spec_val {
        SpecializedValue::SystemTime((sec, n_sec)) => chrono::DateTime::from_timestamp(*sec, *n_sec)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        SpecializedValue::Duration((secs, nanos)) => Some(format_iso_duration(*secs, *nanos)),
        _ => None,
    }
}

fn format_iso_duration(secs: u64, nanos: u32) -> String {
    if secs == 0 && nanos == 0 {
        return "PT0S".to_string();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut out = String::from("PT");
    if h != 0 {
        out.push_str(&format!("{h}H"));
    }
    if m != 0 {
        out.push_str(&format!("{m}M"));
    }
    if s != 0 || nanos != 0 || (h == 0 && m == 0) {
        if nanos == 0 {
            out.push_str(&format!("{s}S"));
        } else {
            // RFC3339 / ISO-8601 allows a decimal second.
            out.push_str(&format!("{s}.{nanos:09}S"));
        }
    }
    out
}

#[cfg(test)]
mod format_spec_tests {
    use super::format_iso_duration;

    #[test]
    fn iso_zero() {
        assert_eq!(format_iso_duration(0, 0), "PT0S");
    }

    #[test]
    fn iso_seconds() {
        assert_eq!(format_iso_duration(7, 0), "PT7S");
    }

    #[test]
    fn iso_h_m_s() {
        assert_eq!(format_iso_duration(3661, 0), "PT1H1M1S");
    }

    #[test]
    fn iso_with_nanos() {
        assert_eq!(format_iso_duration(3, 500_000_000), "PT3.500000000S");
    }
}

#[cfg(test)]
mod byte_format_spec_tests {
    use crate::debugger::variable::render::{ByteRenderMode, render_bytes};

    #[test]
    fn force_utf8_lossy_substitutes_replacement_char() {
        // 0xff is invalid as utf-8; lossy decode replaces with U+FFFD.
        let rendered = render_bytes(&[b'a', 0xff, b'b'], ByteRenderMode::ForceUtf8, false);
        assert!(rendered.starts_with("b\""), "got {rendered:?}");
        assert!(
            rendered.contains("\\u{fffd}") || rendered.contains('\u{FFFD}'),
            "expected replacement char in {rendered:?}"
        );
    }

    #[test]
    fn force_hex_renders_hex_dump_for_valid_utf8() {
        // "hi" is valid utf-8 — Auto would render `b"hi"`. ForceHex
        // bypasses the probe and emits the dump anyway.
        let rendered = render_bytes(b"hi", ByteRenderMode::ForceHex, false);
        assert!(rendered.contains("68 69"), "expected hex `68 69` in {rendered:?}");
        assert!(rendered.contains("|hi|"), "expected ASCII column in {rendered:?}");
    }

    #[test]
    fn auto_keeps_existing_behaviour_for_valid_utf8() {
        let rendered = render_bytes(b"hi", ByteRenderMode::Auto, false);
        assert_eq!(rendered, "b\"hi\"");
    }

    #[test]
    fn auto_falls_to_hex_dump_for_invalid_utf8() {
        let rendered = render_bytes(&[0xff, 0xfe], ByteRenderMode::Auto, false);
        assert!(rendered.contains("ff fe"), "got {rendered:?}");
    }

    #[test]
    fn truncated_marker_present() {
        let rendered = render_bytes(b"abc", ByteRenderMode::Auto, true);
        assert!(rendered.ends_with(" …"), "expected trailing ellipsis in {rendered:?}");
    }
}

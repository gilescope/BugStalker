// SPDX-License-Identifier: MIT
use crate::debugger::debugee::dwarf::r#type::TypeIdentity;
use crate::debugger::variable::value::{ArrayItem, Member, SpecializedValue, Value};
use nix::errno::Errno;
use nix::libc;
use nix::sys::time::TimeSpec;
use once_cell::sync::Lazy;
use std::borrow::Cow;
use std::fmt::{Debug, Formatter};
use std::mem::MaybeUninit;
use std::ops::Sub;
use std::time::Duration;

#[cfg(test)]
mod byte_preview_tests {
    use super::try_byte_string_preview;
    use crate::debugger::debugee::dwarf::r#type::TypeIdentity;
    use crate::debugger::variable::value::{
        ArrayItem, ArrayValue, Member, ScalarValue, SupportedScalar, Value,
    };

    fn vec_of_bytes(bs: &[u8]) -> Vec<Member> {
        let items: Vec<ArrayItem> = bs
            .iter()
            .enumerate()
            .map(|(i, b)| ArrayItem {
                index: i as i64,
                value: Value::Scalar(ScalarValue {
                    type_id: None,
                    type_ident: TypeIdentity::no_namespace("u8"),
                    value: Some(SupportedScalar::U8(*b)),
                    raw_address: None,
                }),
            })
            .collect();
        vec![
            Member {
                field_name: Some("buf".into()),
                value: Value::Array(ArrayValue {
                    type_id: None,
                    type_ident: TypeIdentity::no_namespace("[u8]"),
                    items: Some(items),
                    raw_address: None,
                }),
            },
            Member {
                field_name: Some("cap".into()),
                value: Value::Scalar(ScalarValue {
                    type_id: None,
                    type_ident: TypeIdentity::no_namespace("usize"),
                    value: Some(SupportedScalar::Usize(bs.len())),
                    raw_address: None,
                }),
            },
        ]
    }

    #[test]
    fn empty() {
        assert_eq!(try_byte_string_preview(&vec_of_bytes(&[])).as_deref(), Some("b\"\""));
    }

    #[test]
    fn ascii_utf8() {
        assert_eq!(
            try_byte_string_preview(&vec_of_bytes(b"hello")).as_deref(),
            Some("b\"hello\"")
        );
    }

    #[test]
    fn invalid_utf8_hex_dump() {
        let preview = try_byte_string_preview(&vec_of_bytes(&[0xff, 0xfe, 0x68, 0x69])).unwrap();
        assert!(
            preview.contains("ff fe 68 69"),
            "expected hex bytes, got {preview:?}"
        );
        assert!(preview.contains("|..hi|"), "expected ASCII column, got {preview:?}");
    }

    #[test]
    fn non_u8_returns_none() {
        let int_items: Vec<ArrayItem> = (0..3)
            .map(|i| ArrayItem {
                index: i,
                value: Value::Scalar(ScalarValue {
                    type_id: None,
                    type_ident: TypeIdentity::no_namespace("i32"),
                    value: Some(SupportedScalar::I32(i as i32)),
                    raw_address: None,
                }),
            })
            .collect();
        let members = vec![Member {
            field_name: Some("buf".into()),
            value: Value::Array(ArrayValue {
                type_id: None,
                type_ident: TypeIdentity::no_namespace("[i32]"),
                items: Some(int_items),
                raw_address: None,
            }),
        }];
        assert!(try_byte_string_preview(&members).is_none());
    }
}

#[cfg(test)]
mod time_tests {
    use super::{format_instant, format_system_time};
    use nix::sys::time::TimeSpec;

    #[test]
    fn system_time_unix_epoch() {
        assert_eq!(format_system_time(0, 0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn system_time_with_millis() {
        // chrono's AutoSi promotes to millis when nanos are an exact
        // multiple of 1ms; nanosecond noise stays at full precision.
        assert_eq!(
            format_system_time(0, 123_000_000),
            "1970-01-01T00:00:00.123Z"
        );
    }

    #[test]
    fn system_time_with_nanos() {
        assert_eq!(
            format_system_time(0, 1),
            "1970-01-01T00:00:00.000000001Z"
        );
    }

    #[test]
    fn system_time_invalid() {
        // i64::MAX seconds is past chrono's representable range.
        assert_eq!(format_system_time(i64::MAX, 0), "Broken date time");
    }

    #[test]
    fn instant_in_the_future() {
        let now = TimeSpec::new(100, 0);
        let inst = TimeSpec::new(3661, 500_000_000); // +1h 1m 1.5s
        assert_eq!(format_instant(now, inst), "now + 00:59:21.500");
    }

    #[test]
    fn instant_in_the_past() {
        let now = TimeSpec::new(3661, 500_000_000);
        let inst = TimeSpec::new(100, 0);
        assert_eq!(format_instant(now, inst), "now - 00:59:21.500");
    }

    #[test]
    fn instant_now() {
        let t = TimeSpec::new(1234, 5_000_000);
        assert_eq!(format_instant(t, t), "now + 00:00:00.000");
    }
}

#[cfg(test)]
mod duration_tests {
    use super::format_duration;

    #[test]
    fn zero() {
        assert_eq!(format_duration(0, 0), "0s");
    }

    #[test]
    fn millis_only() {
        assert_eq!(format_duration(0, 1_500_000), "1.500ms");
        assert_eq!(format_duration(0, 999_000_000), "999ms");
    }

    #[test]
    fn micros_only() {
        assert_eq!(format_duration(0, 250_000), "250µs");
        assert_eq!(format_duration(0, 250_500), "250.500µs");
    }

    #[test]
    fn nanos_only() {
        assert_eq!(format_duration(0, 7), "7ns");
    }

    #[test]
    fn whole_seconds() {
        assert_eq!(format_duration(7, 0), "7s");
    }

    #[test]
    fn seconds_with_millis() {
        assert_eq!(format_duration(3, 500_000_000), "3.500s");
    }

    #[test]
    fn h_m_s_ms() {
        assert_eq!(format_duration(3661, 500_000_000), "1h 1m 1.500s");
    }

    #[test]
    fn minutes_only() {
        assert_eq!(format_duration(60, 0), "1m 0s");
    }
}

/// Phase 1 S16 — picks between utf-8 preview and hex dump for a
/// byte-slice render. `Auto` is the default behaviour: try utf-8,
/// fall back to hex dump on invalid bytes. The forced variants drive
/// the F4 `/utf8` and `/hex` format specs, which override the
/// auto-detect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRenderMode {
    /// Try utf-8 first; fall back to hex dump on invalid utf-8.
    Auto,
    /// Force utf-8 with lossy decode (invalid sequences become
    /// `\u{FFFD}` replacement characters in the output).
    ForceUtf8,
    /// Always render the hex dump regardless of byte content.
    ForceHex,
}

/// Phase 1 S16 — when a `Vec<T>` / `VecDeque<T>` carries `u8` items,
/// surface a utf-8 preview (`b"hello"`) when the first 1 KiB of bytes
/// decodes cleanly, or a 16-bytes-per-row hex dump (with ASCII column
/// when printable) when it doesn't. Returns `None` when the items
/// aren't all u8 scalars — caller falls through to the default
/// `Structure` layout.
fn try_byte_string_preview(structure_members: &[Member]) -> Option<String> {
    render_byte_slice_members(structure_members, ByteRenderMode::Auto)
}

/// Phase 1 S16 — entrypoint that honours an explicit [`ByteRenderMode`].
/// Used by the F4 `/utf8` and `/hex` format-spec dispatch in
/// `crate::ui::command::print` to override the auto-detect default.
pub fn render_byte_slice_members(
    structure_members: &[Member],
    mode: ByteRenderMode,
) -> Option<String> {
    use crate::debugger::variable::value::SupportedScalar;
    let buf = structure_members.first()?;
    let Value::Array(arr) = &buf.value else {
        return None;
    };
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
    Some(render_bytes(&bytes, mode, truncated))
}

/// Phase 1 S16 — the actual rendering, factored so a future caller
/// (e.g. `&[u8]` not yet wrapped in a `VecValue`) can plug in.
pub fn render_bytes(bytes: &[u8], mode: ByteRenderMode, truncated: bool) -> String {
    let utf8_form = |bytes: &[u8], lossy: bool| -> String {
        let s: std::borrow::Cow<'_, str> = if lossy {
            String::from_utf8_lossy(bytes)
        } else {
            std::str::from_utf8(bytes)
                .expect("caller must check validity")
                .into()
        };
        let mut out = format!("b{s:?}");
        if truncated {
            out.push_str(" …");
        }
        out
    };
    let hex_form = |bytes: &[u8]| -> String {
        let mut out = String::new();
        for (row_idx, chunk) in bytes.chunks(16).enumerate() {
            if row_idx > 0 {
                out.push('\n');
            }
            for b in chunk {
                out.push_str(&format!("{b:02x} "));
            }
            for _ in chunk.len()..16 {
                out.push_str("   ");
            }
            out.push(' ');
            out.push('|');
            for b in chunk {
                out.push(if (0x20..=0x7e).contains(b) { *b as char } else { '.' });
            }
            out.push('|');
        }
        if truncated {
            out.push_str("\n…");
        }
        out
    };
    match mode {
        ByteRenderMode::Auto => match std::str::from_utf8(bytes) {
            Ok(_) => utf8_form(bytes, false),
            Err(_) => hex_form(bytes),
        },
        ByteRenderMode::ForceUtf8 => utf8_form(bytes, true),
        ByteRenderMode::ForceHex => hex_form(bytes),
    }
}

/// Phase 3 Feature A — `dyn Trait` summary line. The detector
/// recognises the fat-pointer trait-object layout; the parser-side
/// resolver (see `parser::resolve_trait_object_concrete_type`) tries
/// to recover the concrete type behind the vtable and splices the
/// recovered name into `type_ident` as `… [→ Concrete]`. This
/// renderer consumes both states:
///
/// * **Resolved:** the type name carries `[→ Concrete]` already, so
///   we render `<full-name> { data: 0x…, vtable: 0x… }` without a
///   pending marker.
/// * **Unresolved:** rustc didn't export a vtable symbol, the
///   binary is stripped, or every method probe missed. We append a
///   `[concrete type unavailable; …]` hint so the user knows we
///   detected the trait object even though we can't show the inner.
fn render_trait_object_summary(s: &crate::debugger::variable::value::StructValue) -> String {
    use crate::debugger::variable::value::Value;
    let mut data_ptr: Option<*const ()> = None;
    let mut vtable_ptr: Option<*const ()> = None;
    for m in &s.members {
        if let Value::Pointer(p) = &m.value {
            match m.field_name.as_deref() {
                Some("vtable") => vtable_ptr = p.value,
                Some("pointer") | Some("data_ptr") => data_ptr = p.value,
                _ => {}
            }
        }
    }
    let trait_name = s.type_ident.name().unwrap_or("dyn Trait");
    let resolved = trait_name.contains("[→ ");
    match (data_ptr, vtable_ptr, resolved) {
        (Some(d), Some(v), true) => {
            format!("{trait_name} {{ data: {d:p}, vtable: {v:p} }}")
        }
        (Some(d), Some(v), false) => format!(
            "{trait_name} {{ data: {d:p}, vtable: {v:p} }}  [concrete type unavailable; rebuild with `-C symbol-mangling-version=v0` and exported vtable symbols]"
        ),
        _ => format!("{trait_name}  [trait object — pointer fields missing]"),
    }
}

/// Phase 1 S5 — `SystemTime` rendered as ISO-8601 / RFC3339 UTC.
/// `chrono`'s `AutoSi` format suppresses trailing zeros: whole
/// seconds render as `…30Z`, millis as `…30.123Z`, nanos only when
/// sub-microsecond precision is present.
fn format_system_time(sec: i64, n_sec: u32) -> String {
    chrono::DateTime::from_timestamp(sec, n_sec)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| "Broken date time".to_string())
}

/// Phase 1 S5 — `Instant` rendered as `now ± HH:MM:SS.mmm`. The
/// caller passes the current monotonic-clock reading; this helper
/// is pure so it can be unit-tested without touching the real clock.
fn format_instant(now: TimeSpec, instant: TimeSpec) -> String {
    let (sign, delta) = if now > instant {
        ("-", Duration::from(now.sub(instant)))
    } else {
        ("+", Duration::from(instant.sub(now)))
    };
    let secs = delta.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let ms = delta.subsec_millis();
    format!("now {sign} {h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Phase 1 S4 — render a `(secs, nanos)` duration to human-readable
/// form. Examples:
/// - `(0, 0)` → `0s`
/// - `(0, 1_500_000)` → `1.500ms`
/// - `(3, 500_000_000)` → `3.500s`
/// - `(3661, 500_000_000)` → `1h 1m 1.500s`
fn format_duration(secs: u64, nanos: u32) -> String {
    if secs == 0 && nanos == 0 {
        return "0s".to_string();
    }
    // Sub-second: prefer ms with three-digit fraction when ≥ 1ms,
    // µs with three-digit fraction when ≥ 1µs, ns otherwise.
    if secs == 0 {
        if nanos >= 1_000_000 {
            let ms = nanos / 1_000_000;
            let sub = nanos % 1_000_000;
            return if sub == 0 {
                format!("{ms}ms")
            } else {
                format!("{ms}.{:03}ms", sub / 1_000)
            };
        }
        if nanos >= 1_000 {
            let us = nanos / 1_000;
            let sub = nanos % 1_000;
            return if sub == 0 {
                format!("{us}µs")
            } else {
                format!("{us}.{sub:03}µs")
            };
        }
        return format!("{nanos}ns");
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let ms = nanos / 1_000_000;
    let mut out = String::new();
    if h != 0 {
        out.push_str(&format!("{h}h "));
    }
    if m != 0 || h != 0 {
        out.push_str(&format!("{m}m "));
    }
    if ms == 0 {
        out.push_str(&format!("{s}s"));
    } else {
        out.push_str(&format!("{s}.{ms:03}s"));
    }
    out
}

/// Layout of a value from debugee program.
/// Used by UI for representing value to a user.
pub enum ValueLayout<'a> {
    /// Value already rendered, just print it!
    PreRendered(Cow<'a, str>),
    /// Value is an address in debugee memory.
    Referential(*const ()),
    /// Value wraps another value.
    Wrapped(&'a Value),
    /// Value is a structure.
    Structure(&'a [Member]),
    /// Value is a list with indexed elements.
    IndexedList(&'a [ArrayItem]),
    /// Value is an unordered list.
    NonIndexedList(&'a [Value]),
    /// Value is a map where keys and values are values too.
    Map(&'a [(Value, Value)]),
}

impl Debug for ValueLayout<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueLayout::PreRendered(s) => f.debug_tuple("PreRendered").field(s).finish(),
            ValueLayout::Referential(addr) => f.debug_tuple("Referential").field(addr).finish(),
            ValueLayout::Wrapped(v) => f.debug_tuple("Wrapped").field(&v).finish(),
            ValueLayout::Structure(members) => {
                f.debug_struct("Nested").field("members", members).finish()
            }
            ValueLayout::Map(kvs) => {
                let mut list = f.debug_list();
                for kv in kvs.iter() {
                    list.entry(&kv);
                }
                list.finish()
            }
            ValueLayout::IndexedList(items) => {
                f.debug_struct("List").field("items", items).finish()
            }
            ValueLayout::NonIndexedList(items) => {
                f.debug_struct("List").field("items", items).finish()
            }
        }
    }
}

pub trait RenderValue {
    /// Return type identity for rendering.
    fn r#type(&self) -> &TypeIdentity;

    /// Return value layout for rendering.
    fn value_layout(&self) -> Option<ValueLayout<'_>>;
}

impl RenderValue for Value {
    fn r#type(&self) -> &TypeIdentity {
        static STRING_TYPE: Lazy<TypeIdentity> = Lazy::new(|| TypeIdentity::no_namespace("String"));
        static STR_TYPE: Lazy<TypeIdentity> = Lazy::new(|| TypeIdentity::no_namespace("&str"));
        static UNKNOWN_TYPE: Lazy<TypeIdentity> = Lazy::new(TypeIdentity::unknown);

        match self {
            Value::Scalar(s) => &s.type_ident,
            Value::Struct(s) => &s.type_ident,
            Value::Array(a) => &a.type_ident,
            Value::CEnum(e) => &e.type_ident,
            Value::RustEnum(e) => &e.type_ident,
            Value::Pointer(p) => &p.type_ident,
            Value::Specialized {
                value: Some(spec_val),
                original,
            } => match spec_val {
                SpecializedValue::Vector(vec) | SpecializedValue::VecDeque(vec) => {
                    &vec.structure.type_ident
                }
                SpecializedValue::String { .. } => &STRING_TYPE,
                SpecializedValue::Str { .. } => &STR_TYPE,
                SpecializedValue::Tls(value) => &value.inner_type,
                SpecializedValue::HashMap(map) => &map.type_ident,
                SpecializedValue::HashSet(set) => &set.type_ident,
                SpecializedValue::BTreeMap(map) => &map.type_ident,
                SpecializedValue::BTreeSet(set) => &set.type_ident,
                SpecializedValue::Cell(_) | SpecializedValue::RefCell(_) => &original.type_ident,
                SpecializedValue::Rc(_) | SpecializedValue::Arc(_) => &original.type_ident,
                SpecializedValue::Uuid(_) => &original.type_ident,
                SpecializedValue::SystemTime(_) => &original.type_ident,
                SpecializedValue::Instant(_) => &original.type_ident,
                // Atomic / NonNull / Pin / Range / Duration / CString /
                // OsString / MaybeUninit / Mutex / LockGuard / Weak:
                // keep the original wrapper name; the value side is
                // surfaced via `value_layout`.
                SpecializedValue::Atomic(_)
                | SpecializedValue::NonNull(_)
                | SpecializedValue::Pin(_)
                | SpecializedValue::Range(_)
                | SpecializedValue::Duration(_)
                | SpecializedValue::CString(_)
                | SpecializedValue::OsString(_)
                | SpecializedValue::MaybeUninit(_)
                | SpecializedValue::Mutex { .. }
                | SpecializedValue::LockGuard(_)
                | SpecializedValue::Weak { .. } => &original.type_ident,
            },
            Value::Specialized { original, .. } => &original.type_ident,
            Value::Subroutine(_) => {
                // currently this line is unreachable because dereference of fn pointer is forbidden
                &UNKNOWN_TYPE
            }
            Value::CModifiedVariable(v) => &v.type_ident,
        }
    }

    fn value_layout(&self) -> Option<ValueLayout<'_>> {
        let value_repr = match self {
            Value::Scalar(scalar) => {
                ValueLayout::PreRendered(Cow::Owned(scalar.value.as_ref()?.to_string()))
            }
            Value::Struct(r#struct) => {
                // Phase 3 Feature A — annotate `dyn Trait` fat-pointer
                // structs so the user knows the renderer recognised
                // the trait-object layout. Concrete-type recovery
                // (vtable → symbol → demangle → TypeId) is a follow-
                // up batch; this hop is detection + tagging only.
                if r#struct.is_trait_object() {
                    let body = render_trait_object_summary(r#struct);
                    return Some(ValueLayout::PreRendered(Cow::Owned(body)));
                }
                ValueLayout::Structure(r#struct.members.as_ref())
            }
            Value::Array(array) => ValueLayout::IndexedList(array.items.as_deref()?),
            Value::CEnum(r#enum) => ValueLayout::PreRendered(Cow::Borrowed(r#enum.value.as_ref()?)),
            Value::RustEnum(r#enum) => {
                let enum_val = &r#enum.value.as_ref()?.value;
                ValueLayout::Wrapped(enum_val)
            }
            Value::Pointer(pointer) => {
                // Phase 1 S9 — smart pointers (Box<T>) populate
                // `dereffed` at parse time. When present, surface the
                // pointee inline via `Wrapped`; raw `*const T` and
                // `&T` references fall through to address-only
                // display.
                if let Some(inner) = pointer.dereffed.as_deref() {
                    ValueLayout::Wrapped(inner)
                } else {
                    let ptr = pointer.value?;
                    ValueLayout::Referential(ptr)
                }
            }
            Value::Specialized {
                value: Some(spec_val),
                ..
            } => match spec_val {
                SpecializedValue::Vector(vec) | SpecializedValue::VecDeque(vec) => {
                    // Phase 1 S16: u8-element vectors get a utf-8 /
                    // hex-dump preview at render time. The underlying
                    // VecValue is unchanged so tests that walk the
                    // items by index still work.
                    if let Some(mut preview) = try_byte_string_preview(&vec.structure.members) {
                        // Phase 1 F3: append the elision marker when the
                        // collection length exceeded LEN_GUARD.
                        if let Some(n) = vec.elided {
                            preview.push_str(&format!(" (… {n} more elided)"));
                        }
                        ValueLayout::PreRendered(Cow::Owned(preview))
                    } else if let Some(n) = vec.elided {
                        // Structured render with truncation note: build
                        // a synthetic pre-rendered string that combines
                        // the items count and the elision marker. Real
                        // Structure rendering loses the trailer, so
                        // surface the marker as a wrapper line.
                        let item_count = match &vec.structure.members.first() {
                            Some(m) => match &m.value {
                                Value::Array(a) => a.items.as_ref().map(|i| i.len()).unwrap_or(0),
                                _ => 0,
                            },
                            None => 0,
                        };
                        ValueLayout::PreRendered(Cow::Owned(format!(
                            "[{item_count} items] (… {n} more elided)"
                        )))
                    } else {
                        ValueLayout::Structure(vec.structure.members.as_ref())
                    }
                }
                SpecializedValue::String(string) => {
                    // Phase 1 F3: append the elision marker for
                    // truncated strings.
                    if let Some(n) = string.elided {
                        ValueLayout::PreRendered(Cow::Owned(format!(
                            "{} (… {n} more elided)",
                            string.value
                        )))
                    } else {
                        ValueLayout::PreRendered(Cow::Borrowed(&string.value))
                    }
                }
                SpecializedValue::Str(string) => {
                    if let Some(n) = string.elided {
                        ValueLayout::PreRendered(Cow::Owned(format!(
                            "{} (… {n} more elided)",
                            string.value
                        )))
                    } else {
                        ValueLayout::PreRendered(Cow::Borrowed(&string.value))
                    }
                }
                SpecializedValue::Tls(tls_value) => match tls_value.inner_value.as_ref() {
                    None => ValueLayout::PreRendered(Cow::Borrowed("uninit")),
                    Some(tls_inner_val) => tls_inner_val.value_layout()?,
                },
                // Phase 1 F3: HashMap / BTreeMap render via the
                // structured `Map` layout, but when truncated we
                // collapse to a pre-rendered summary line so the
                // elision marker is visible.
                SpecializedValue::HashMap(map) | SpecializedValue::BTreeMap(map) => {
                    if let Some(n) = map.elided {
                        ValueLayout::PreRendered(Cow::Owned(format!(
                            "[{} entries] (… {n} more elided)",
                            map.kv_items.len()
                        )))
                    } else {
                        ValueLayout::Map(&map.kv_items)
                    }
                }
                SpecializedValue::HashSet(set) | SpecializedValue::BTreeSet(set) => {
                    if let Some(n) = set.elided {
                        ValueLayout::PreRendered(Cow::Owned(format!(
                            "[{} items] (… {n} more elided)",
                            set.items.len()
                        )))
                    } else {
                        ValueLayout::NonIndexedList(&set.items)
                    }
                }
                SpecializedValue::Cell(cell) | SpecializedValue::RefCell(cell) => {
                    cell.value_layout()?
                }
                SpecializedValue::Rc(ptr) | SpecializedValue::Arc(ptr) => {
                    let ptr = ptr.value?;
                    ValueLayout::Referential(ptr)
                }
                SpecializedValue::Uuid(bytes) => {
                    let uuid = uuid::Uuid::from_slice(bytes).expect("infallible");
                    ValueLayout::PreRendered(Cow::Owned(uuid.to_string()))
                }
                // Phase 1 S5: SystemTime renders as ISO-8601 / RFC3339
                // UTC; see `format_system_time` for the format detail.
                SpecializedValue::SystemTime((sec, n_sec)) => {
                    ValueLayout::PreRendered(Cow::Owned(format_system_time(*sec, *n_sec)))
                }
                // Phase 1 S5: Instant renders as a delta from the
                // debugger's wall-clock "now" since libstd does not
                // expose its monotonic-clock epoch and we cannot
                // recover a process-local start instant from DWARF
                // alone. Recovering a true program-start delta is
                // deferred until a TLS or symbol-table hook lands.
                SpecializedValue::Instant((sec, n_sec)) => {
                    let now = now_timespec().expect("broken system clock");
                    let instant = TimeSpec::new(*sec, *n_sec as i64);
                    ValueLayout::PreRendered(Cow::Owned(format_instant(now, instant)))
                }
                // Atomic delegates to the inner scalar/pointer's layout.
                SpecializedValue::Atomic(inner) => inner.value_layout()?,
                // NonNull renders as the bare pointer it carries.
                SpecializedValue::NonNull(ptr) => {
                    let p = ptr.value?;
                    ValueLayout::Referential(p)
                }
                // Pin delegates to the pinnee's layout — the `Pin<...>`
                // wrapper name on the value's type identity is enough
                // to convey that we're looking at a pinned reference.
                SpecializedValue::Pin(inner) => inner.value_layout()?,
                // Range: pre-render to a single string `start..end` /
                // `start..=end` / `start..` / `..end` / `..=end` /
                // `..`, with `[exhausted]` trailer for inclusive
                // ranges that have already drained.
                SpecializedValue::Range(r) => ValueLayout::PreRendered(Cow::Owned(r.render())),
                // Phase 1 S4: Duration renders human-readable
                // (`1h 2m 3.500s`, or `0s` for zero).
                SpecializedValue::Duration((secs, nanos)) => {
                    ValueLayout::PreRendered(Cow::Owned(format_duration(*secs, *nanos)))
                }
                // Phase 1 S12: CString — the inner StringVariable
                // already carries the rendered text (utf-8-quoted or
                // hex-preview), produced at parse time.
                SpecializedValue::CString(s) => {
                    ValueLayout::PreRendered(Cow::Borrowed(&s.value))
                }
                // Phase 1 S13/S14: OsString / PathBuf — same
                // pre-rendered shape as CString.
                SpecializedValue::OsString(s) => {
                    ValueLayout::PreRendered(Cow::Borrowed(&s.value))
                }
                // Phase 1 S10: MaybeUninit — render the inner value's
                // layout. The `[possibly uninit]` marker is on the
                // type-identity side so DAP clients can append it
                // when displaying the value.
                SpecializedValue::MaybeUninit(inner) => inner.value_layout()?,
                // Phase 1 S1: Mutex/RwLock — render the guarded
                // payload directly with optional `[locked]` and
                // `[poisoned]` badges. Lock-state detection works
                // on the futex backend only; non-futex platforms
                // (macOS pthread, Win7 SRWLOCK) report locked=false.
                SpecializedValue::Mutex {
                    inner,
                    poisoned,
                    locked,
                } => {
                    if *poisoned || *locked {
                        let inner_text = match inner.value_layout() {
                            Some(ValueLayout::PreRendered(s)) => s.into_owned(),
                            Some(ValueLayout::Referential(p)) => format!("0x{:x}", p as usize),
                            _ => "?".to_string(),
                        };
                        let mut badges = String::new();
                        if *locked {
                            badges.push_str(" [locked]");
                        }
                        if *poisoned {
                            badges.push_str(" [poisoned]");
                        }
                        ValueLayout::PreRendered(Cow::Owned(format!("{inner_text}{badges}")))
                    } else {
                        inner.value_layout()?
                    }
                }
                // Phase 1 S2: lock guards — render the guarded
                // payload directly. The `MutexGuard<T>` etc. wrapper
                // name on the type identity carries the "this is a
                // guard" framing.
                SpecializedValue::LockGuard(inner) => inner.value_layout()?,
                // Phase 1 S15: Weak — render `0xADDR (strong=N,
                // weak=M)` plus a `[dropped]` trailer when the
                // strong count has reached zero (the underlying T
                // has been dropped but the allocation is still alive
                // because at least one Weak handle remains).
                SpecializedValue::Weak { ptr, strong, weak } => {
                    let addr = ptr.value.map_or("?".to_string(), |p| format!("0x{:x}", p as usize));
                    let dropped_tag = if *strong == 0 { " [dropped]" } else { "" };
                    ValueLayout::PreRendered(Cow::Owned(format!(
                        "{addr} (strong={strong}, weak={weak}){dropped_tag}"
                    )))
                }
            },
            Value::Specialized { original, .. } => {
                ValueLayout::Structure(original.members.as_ref())
            }
            Value::Subroutine(_) => {
                // currently this line is unreachable because dereference of fn pointer is forbidden
                return None;
            }
            Value::CModifiedVariable(v) => ValueLayout::Wrapped(v.value.as_ref()?),
        };
        Some(value_repr)
    }
}

fn now_timespec() -> Result<TimeSpec, Errno> {
    let mut t = MaybeUninit::uninit();
    let res = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, t.as_mut_ptr()) };
    if res == -1 {
        return Err(Errno::last());
    }
    let t = unsafe { t.assume_init() };
    Ok(TimeSpec::new(t.tv_sec, t.tv_nsec))
}

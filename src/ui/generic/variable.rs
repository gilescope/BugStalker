// SPDX-License-Identifier: MIT
use crate::debugger::address::RelocatedAddress;
use crate::debugger::variable::execute::{QueryResult, QueryResultKind};
use crate::debugger::variable::render::{RenderValue, ValueLayout};
use crate::debugger::variable::value::{Member, Value};
use crate::debugger::viz::VizRegistry;
use crate::ui::syntax;
use crate::ui::syntax::StylizedLine;
use bs_viz_spec::{Format, TypeViewSpec};
use syntect::util::as_24_bit_terminal_escaped;

const TAB: &str = "    ";

pub fn render_variable(var: &QueryResult, prerender: Option<&str>) -> anyhow::Result<String> {
    render_variable_with_viz(var, prerender, None)
}

/// Phase 4 Tier-A — like [`render_variable`] but consults a
/// [`VizRegistry`] (see [`crate::debugger::Debugger::view_registry`])
/// so registered `#[derive(DebugView)]` types render through their
/// declarative spec instead of the default structure dump.
pub fn render_variable_with_viz(
    var: &QueryResult,
    prerender: Option<&str>,
    viz: Option<&VizRegistry>,
) -> anyhow::Result<String> {
    let syntax_renderer = syntax::rust_syntax_renderer();
    let mut line_renderer = syntax_renderer.line_renderer();
    let prefix = if var.kind() == QueryResultKind::Root && var.identity().name.is_some() {
        format!("{} = ", var.identity())
    } else {
        String::default()
    };

    let var_as_string = if let Some(value) = prerender {
        format!("{prefix}{value}")
    } else {
        format!("{prefix}{}", render_value_with_viz(var.value(), viz))
    };
    Ok(var_as_string
        .lines()
        .map(|l| -> anyhow::Result<String> {
            let line = match line_renderer.render_line(l)? {
                StylizedLine::NoneStyle(l) => l.to_string(),
                StylizedLine::Stylized(segments) => {
                    let line = as_24_bit_terminal_escaped(&segments, false);
                    format!("{line}\x1b[0m")
                }
            };
            Ok(line)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("\n"))
}

pub fn render_value(value: &Value) -> String {
    render_value_inner(value, 0, true, None)
}

/// Render with optional Tier-A spec lookup. When `viz` is
/// `Some`, struct rendering looks the type name up; on a hit
/// the rendered output prepends the `summary` template (with
/// `{field_name}` placeholders substituted) and applies
/// per-field `skip` / `rename` filters in the child list.
pub fn render_value_with_viz(value: &Value, viz: Option<&VizRegistry>) -> String {
    render_value_inner(value, 0, true, viz)
}

fn render_value_inner(
    value: &Value,
    depth: usize,
    print_type: bool,
    viz: Option<&VizRegistry>,
) -> String {
    match value.value_layout() {
        Some(layout) => match layout {
            ValueLayout::PreRendered(rendered_value) => {
                let type_name = value.r#type().name_fmt();
                match value {
                    Value::CEnum(_) => format!("{type_name}::{rendered_value}"),
                    // The unit type renders its own value as "()", so the
                    // generic `Type(value)` wrap below would produce the
                    // nonsense `()(())`. Special-case: emit just "()".
                    _ if type_name == "()" => "()".to_string(),
                    _ if print_type => format!("{type_name}({rendered_value})"),
                    _ => format!("{rendered_value}"),
                }
            }
            ValueLayout::Referential(addr) => {
                if print_type {
                    format!(
                        "{} [{}]",
                        value.r#type().name_fmt(),
                        RelocatedAddress::from(addr as usize)
                    )
                } else {
                    format!("{}", RelocatedAddress::from(addr as usize))
                }
            }
            ValueLayout::Wrapped(val) => {
                // Step 6 + 8 — enum dispatch on active variant.
                // Step 6 took the type-level summary; step 8
                // looks first for a *variant-level* spec, falls
                // back to the type-level one, and prepends the
                // variant's `tag` (when set) as a leading
                // `[tag]` chip. Render order:
                //
                //   <enum> [<tag>] <summary-or-default>
                //
                // The tag is purely a string the crate author
                // chose (`ok`, `warn`, `err`, ...); presentation
                // is the IDE's call. Wrapping it in `[...]` is
                // BugStalker's lowest-common-denominator default.
                if let Value::RustEnum(re) = value
                    && let Some(spec) = viz.and_then(|r| {
                        let outer = value.r#type().name_fmt();
                        r.find(outer)
                    })
                {
                    let active_variant_name =
                        re.value.as_ref().and_then(|m| m.field_name.as_deref());
                    let variant_spec = active_variant_name
                        .and_then(|n| spec.variants.iter().find(|v| v.name == n));
                    let tmpl = variant_spec
                        .and_then(|v| v.summary.as_deref())
                        .or(spec.summary.as_deref());
                    if let (Some(tmpl), Value::Struct(variant)) = (tmpl, val) {
                        // Variant-scoped fields override
                        // type-level ones for placeholder
                        // resolution. We synthesise a temp
                        // spec view via `substitute_with_fields`
                        // so renames / formats from the
                        // variant entry apply.
                        let outer_type = value.r#type().name_fmt();
                        let summary = match variant_spec {
                            Some(v) => {
                                substitute_template_with_fields(tmpl, &variant.members, &v.fields)
                            }
                            None => substitute_template(tmpl, &variant.members, spec),
                        };
                        let tag_prefix = variant_spec
                            .and_then(|v| v.tag.as_deref())
                            .map(|t| format!(" [{t}]"))
                            .unwrap_or_default();
                        return format!("{outer_type}{tag_prefix} {summary}");
                    }
                }
                format!(
                    "{}::{}",
                    value.r#type().name_fmt(),
                    render_value_inner(val, depth, true, viz)
                )
            }
            #[allow(clippy::useless_format)]
            ValueLayout::Structure(members) => {
                let type_name = value.r#type().name_fmt();
                let spec = viz.and_then(|r| r.find(type_name));

                // Detect tuple shape: all member field names are
                // `__0`, `__1`, … in order. Rust uses this naming
                // for tuple structs (`Wrap(7, 8)`), tuple variants
                // of enums (`Result::Ok(7)`), and bare tuples
                // (`(1, "one")`). Render as positional in parens
                // rather than struct-init braces. A user-supplied
                // viz spec opts out (the spec author chose
                // struct-style placeholders deliberately).
                let is_tuple_shape = spec.is_none()
                    && !members.is_empty()
                    && members
                        .iter()
                        .enumerate()
                        .all(|(i, m)| m.field_name.as_deref() == Some(&format!("__{i}")));
                if is_tuple_shape {
                    let inner: Vec<String> = members
                        .iter()
                        .map(|m| {
                            // Only scalar leaves (PreRendered) drop
                            // their type prefix — that's what makes
                            // `Result::Ok(7)` read better than
                            // `Result::Ok(i32(7))`. Anything else
                            // keeps it so we preserve:
                            //   * newtype wrappers: `Some(NonZero<u32>(42))`
                            //   * pointer addresses with their type:
                            //     `Some(&i32 [0x…])` not the bare
                            //     `Some(0x…)`
                            //   * type-name annotations like cycle
                            //     markers (`[cycle to 0x…]`) and dyn
                            //     concrete-type recovery (`[→ Type]`),
                            //     both of which live in the type ident
                            //     and would otherwise vanish.
                            let keep_type = !matches!(
                                m.value.value_layout(),
                                Some(ValueLayout::PreRendered(_))
                            );
                            render_value_inner(&m.value, depth, keep_type, viz)
                        })
                        .collect();
                    let body = format!("({})", inner.join(", "));
                    return if print_type && !type_name.starts_with('(') {
                        // Bare-tuple types are already parens-shaped
                        // (`(i32, &str)`); doubling them up gives
                        // `(i32, &str)(1, "one")`. Skip the prefix.
                        format!("{type_name}{body}")
                    } else {
                        body
                    };
                }

                let summary_str = spec.and_then(|s| {
                    s.summary
                        .as_deref()
                        .map(|tmpl| substitute_template(tmpl, members, s))
                });

                let header = match (print_type, summary_str.as_deref()) {
                    (_, Some(s)) => format!("{type_name} {s}"),
                    (true, None) => format!("{type_name}"),
                    (false, None) => String::new(),
                };

                // Apply per-field skip / rename overrides when a
                // spec is present. Without a spec, behave exactly
                // as before.
                let mut render = if header.is_empty() {
                    format!("{{")
                } else {
                    format!("{header} {{")
                };
                let tabs = TAB.repeat(depth + 1);

                for member in members {
                    let (display_name, hidden, format) =
                        apply_field_overrides(spec, member.field_name.as_deref());
                    if hidden {
                        continue;
                    }
                    let rendered = format
                        .filter(|f| *f != Format::Default)
                        .and_then(|f| {
                            format_scalar(&member.value, f)
                                .or_else(|| format_bytes(&member.value, f))
                        })
                        .unwrap_or_else(|| render_value_inner(&member.value, depth + 1, true, viz));
                    render = format!("{render}\n");
                    render = format!(
                        "{render}{tabs}{}: {}",
                        display_name.unwrap_or_default(),
                        rendered,
                    );
                }

                format!("{render}\n{}}}", TAB.repeat(depth))
            }
            ValueLayout::Map(kv_children) => {
                let mut render = format!("{} {{", value.r#type().name_fmt());

                let tabs = TAB.repeat(depth + 1);

                let mut last_seen_kv_types = None;
                let mut show_kv_type = false;
                for (key, val) in kv_children {
                    if last_seen_kv_types != Some((key.r#type(), val.r#type())) {
                        last_seen_kv_types = Some((key.r#type(), val.r#type()));
                        show_kv_type = true;
                    }

                    render = format!("{render}\n");
                    render = format!(
                        "{render}{tabs}{}: {}",
                        render_value_inner(key, depth + 1, show_kv_type, viz),
                        render_value_inner(val, depth + 1, show_kv_type, viz)
                    );
                    show_kv_type = false;
                }

                format!("{render}\n{}}}", TAB.repeat(depth))
            }
            ValueLayout::IndexedList(items) => render_linear_list(
                value.r#type().name_fmt().to_string(),
                items.iter().map(|i| &i.value),
                depth,
                print_type,
                viz,
            ),
            ValueLayout::NonIndexedList(values) => render_linear_list(
                value.r#type().name_fmt().to_string(),
                values.iter(),
                depth,
                print_type,
                viz,
            ),
        },
        None => format!("{}(unknown)", value.r#type().name_fmt()),
    }
}

/// TUI/console substitution — uses the type-suppressed inline
/// render so `Person({name}, age {age})` reads as
/// `Person(Ada, age 36)` rather than
/// `Person(String(Ada), age u32(36))`. Honours per-field
/// `format` overrides from `spec`, so a placeholder `{flags}`
/// for a field marked `format = "hex"` substitutes as
/// `0xff00ff` rather than the raw decimal form.
/// Render a linear collection (array, slice, Vec, set, …) as
/// `TypeName [a, b, c]` — matching Rust's `Debug` shape for slices.
/// Single-line; long lists wrap at the terminal naturally. Inner
/// items render with `print_type=false` so we don't get noise like
/// `Vec<i32> [i32(1), i32(2)]`.
fn render_linear_list<'a>(
    type_name: String,
    items: impl IntoIterator<Item = &'a Value>,
    depth: usize,
    print_type: bool,
    viz: Option<&VizRegistry>,
) -> String {
    let inner: Vec<String> = items
        .into_iter()
        .map(|v| render_value_inner(v, depth + 1, false, viz))
        .collect();
    let body = format!("[{}]", inner.join(", "));
    if print_type {
        format!("{type_name} {body}")
    } else {
        body
    }
}

fn substitute_template(template: &str, members: &[Member], spec: &TypeViewSpec) -> String {
    substitute_template_with_fields(template, members, &spec.fields)
}

/// Same substitution as [`substitute_template`] but takes the
/// per-field overrides directly. Step 8 needs this so a
/// variant-scoped field list (the `fields` of a
/// `VariantSpec`) can drive placeholder resolution without
/// having to round-trip through a synthetic `TypeViewSpec`.
fn substitute_template_with_fields(
    template: &str,
    members: &[Member],
    fields: &[bs_viz_spec::FieldSpec],
) -> String {
    crate::debugger::viz::substitute_template(template, members, |m| {
        let format = m
            .field_name
            .as_deref()
            .and_then(|name| fields.iter().find(|f| f.name == name))
            .map(|f| f.format)
            .filter(|f| *f != Format::Default);
        if let Some(fmt) = format {
            if let Some(s) = format_scalar(&m.value, fmt) {
                return s;
            }
            if let Some(s) = format_bytes(&m.value, fmt) {
                return s;
            }
        }
        render_value_inner(&m.value, 0, false, None)
    })
}

/// Resolve display name, visibility, and per-field format
/// against a spec entry. Returns `(displayed_name, hidden,
/// format)`. With no spec or no entry for this field, the raw
/// name is returned, `hidden = false`, and `format = None`.
fn apply_field_overrides<'a>(
    spec: Option<&'a TypeViewSpec>,
    field_name: Option<&'a str>,
) -> (Option<&'a str>, bool, Option<Format>) {
    let Some(name) = field_name else {
        return (None, false, None);
    };
    let Some(spec) = spec else {
        return (Some(name), false, None);
    };
    match spec.fields.iter().find(|f| f.name == name) {
        Some(f) => {
            let display = f.rename.as_deref().or(Some(name));
            (display, f.hidden, Some(f.format))
        }
        None => (Some(name), false, None),
    }
}

/// Public alias for the DAP path. The TUI/console rendering and
/// the DAP one-line rendering both share this same scalar
/// formatter — the formatting itself is independent of the
/// surrounding render style.
pub fn format_scalar_for_dap(value: &Value, fmt: Format) -> Option<String> {
    format_scalar(value, fmt)
}

/// Public alias for the DAP path of [`format_bytes`].
pub fn format_bytes_for_dap(value: &Value, fmt: Format) -> Option<String> {
    format_bytes(value, fmt)
}

/// Step 11 — apply `format = "utf8"` or `format = "hexdump"` to a
/// byte-array value. Recognises:
///
/// * `Vec<u8>` / `VecDeque<u8>` — via the specialised `Vector`
///   variant; reuses BugStalker's existing
///   [`render_byte_slice_members`] helper which already knows
///   how to walk the inner array.
/// * `String` / `&str` — already-decoded UTF-8 in the
///   specialised variants; we lift its bytes and run them
///   through [`render_bytes`] for uniform output shape.
/// * `[u8; N]` / `&[u8]`-shaped `Value::Array` — extracted to a
///   `Vec<u8>` of u8 scalars and rendered.
///
/// Returns `None` for anything that isn't a recognisable byte
/// shape so the caller falls through to default rendering.
fn format_bytes(value: &Value, fmt: Format) -> Option<String> {
    use crate::debugger::variable::render::{
        ByteRenderMode, render_byte_slice_members, render_bytes,
    };
    use crate::debugger::variable::value::{SpecializedValue, SupportedScalar};
    let mode = match fmt {
        Format::Utf8 => ByteRenderMode::ForceUtf8,
        Format::Hexdump => ByteRenderMode::ForceHex,
        _ => return None,
    };
    match value {
        Value::Specialized {
            value: Some(spec), ..
        } => match spec {
            SpecializedValue::Vector(v) | SpecializedValue::VecDeque(v) => {
                render_byte_slice_members(&v.structure.members, mode)
            }
            SpecializedValue::String(s) => Some(render_bytes(s.value.as_bytes(), mode, false)),
            SpecializedValue::Str(s) => Some(render_bytes(s.value.as_bytes(), mode, false)),
            _ => None,
        },
        Value::Array(arr) => {
            let items = arr.items.as_ref()?;
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
        _ => None,
    }
}

/// Apply a `#[bs_viz(format = "...")]` override to a scalar
/// value. Returns `Some(rendered)` when the value is a scalar
/// integer and the format is applicable. Otherwise `None`,
/// signalling the caller should fall back to default rendering.
///
/// Format conventions:
///
/// * `hex` / `bin` / `oct` — integer base.
/// * `iso8601` reads an integer as **Unix epoch seconds** (UTC).
/// * `duration` reads an integer as **nanoseconds** and renders
///   it in the largest sensible unit (ns / µs / ms / s).
/// * `utf8` / `hexdump` operate on byte-array values and route
///   through [`format_bytes`] instead — `format_scalar` is
///   integer-only and returns `None` for those.
fn format_scalar(value: &Value, fmt: Format) -> Option<String> {
    if let Value::Scalar(s) = value {
        let n = s.try_as_number()?;
        return Some(match fmt {
            Format::Hex => format!("{:#x}", n),
            Format::Bin => format!("{:#b}", n),
            Format::Oct => format!("{:#o}", n),
            Format::Iso8601 => format_unix_epoch_iso8601(n)?,
            Format::Duration => format_nanos(n)?,
            Format::Default | Format::Utf8 | Format::Hexdump => return None,
        });
    }
    // Non-scalar — let the default renderer handle it.
    None
}

/// Render `secs` interpreted as Unix epoch seconds in UTC, in
/// ISO-8601 form (`YYYY-MM-DDTHH:MM:SSZ`). Returns `None` for
/// values outside `chrono`'s representable range so we fall back
/// to default rendering rather than swallowing the value.
fn format_unix_epoch_iso8601(secs: i64) -> Option<String> {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = DateTime::from_timestamp(secs, 0)?;
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

#[cfg(test)]
mod format_tests {
    use super::{format_nanos, format_unix_epoch_iso8601};

    #[test]
    fn iso8601_known_fixed_point() {
        // 2024-01-15T12:34:56Z — same constant the integration
        // test in `tests/debugger/viz.rs` uses against `viz_demo`,
        // so the lookup tables on both sides agree.
        assert_eq!(
            format_unix_epoch_iso8601(1_705_322_096).as_deref(),
            Some("2024-01-15T12:34:56Z"),
        );
    }

    #[test]
    fn iso8601_unix_epoch() {
        assert_eq!(
            format_unix_epoch_iso8601(0).as_deref(),
            Some("1970-01-01T00:00:00Z"),
        );
    }

    #[test]
    fn iso8601_negative_pre_epoch() {
        assert_eq!(
            format_unix_epoch_iso8601(-1).as_deref(),
            Some("1969-12-31T23:59:59Z"),
        );
    }

    #[test]
    fn duration_units() {
        assert_eq!(format_nanos(0).as_deref(), Some("0ns"));
        assert_eq!(format_nanos(999).as_deref(), Some("999ns"));
        assert_eq!(format_nanos(1_500).as_deref(), Some("1.500µs"));
        assert_eq!(format_nanos(5_000_000).as_deref(), Some("5.000ms"));
        assert_eq!(format_nanos(1_500_000_000).as_deref(), Some("1.5s"));
        assert_eq!(format_nanos(2_000_000_000).as_deref(), Some("2s"));
    }

    #[test]
    fn duration_negative_falls_through() {
        assert!(format_nanos(-1).is_none());
    }
}

/// Render `nanos` as a duration, picking the largest sensible
/// unit. Negative values fall back to the default renderer
/// (`Duration` is unsigned in std, so a negative annotated
/// scalar is almost certainly a misuse rather than a real
/// duration we should pretend to format).
fn format_nanos(nanos: i64) -> Option<String> {
    if nanos < 0 {
        return None;
    }
    let n = nanos as u64;
    Some(if n >= 1_000_000_000 {
        let secs = n / 1_000_000_000;
        let rem = n % 1_000_000_000;
        // Trim trailing zeros for readability: 1.500s not
        // 1.500000000s. `format!` doesn't do this for us.
        let frac = format!("{rem:09}");
        let frac = frac.trim_end_matches('0');
        if frac.is_empty() {
            format!("{secs}s")
        } else {
            format!("{secs}.{frac}s")
        }
    } else if n >= 1_000_000 {
        format!("{:.3}ms", nanos as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.3}µs", nanos as f64 / 1_000.0)
    } else {
        format!("{n}ns")
    })
}

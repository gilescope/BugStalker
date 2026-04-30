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
            ValueLayout::PreRendered(rendered_value) => match value {
                Value::CEnum(_) => format!("{}::{}", value.r#type().name_fmt(), rendered_value),
                _ if print_type => format!("{}({})", value.r#type().name_fmt(), rendered_value),
                _ => format!("{rendered_value}"),
            },
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
                format!(
                    "{}::{}",
                    value.r#type().name_fmt(),
                    render_value_inner(val, depth, true, viz)
                )
            }
            #[allow(clippy::useless_format)]
            ValueLayout::Structure(members) => {
                let type_name = value.r#type().name_fmt();
                let spec = viz.and_then(|r| r.find(&type_name));
                let summary_str = spec
                    .and_then(|s| s.summary.as_deref())
                    .map(|tmpl| substitute_template(tmpl, members));

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
                        .and_then(|f| format_scalar(&member.value, f))
                        .unwrap_or_else(|| {
                            render_value_inner(&member.value, depth + 1, true, viz)
                        });
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
            ValueLayout::IndexedList(items) => {
                let mut render = format!("{} {{", value.r#type().name_fmt());

                let tabs = TAB.repeat(depth + 1);

                for item in items {
                    render = format!("{render}\n");
                    render = format!(
                        "{render}{tabs}{}: {}",
                        item.index,
                        render_value_inner(&item.value, depth + 1, false, viz)
                    );
                }

                format!("{render}\n{}}}", TAB.repeat(depth))
            }
            ValueLayout::NonIndexedList(values) => {
                let mut render = format!("{} {{", value.r#type().name_fmt());

                let tabs = TAB.repeat(depth + 1);

                for val in values {
                    render = format!("{render}\n");
                    render = format!(
                        "{render}{tabs}{}",
                        render_value_inner(val, depth + 1, false, viz)
                    );
                }

                format!("{render}\n{}}}", TAB.repeat(depth))
            }
        },
        None => format!("{}(unknown)", value.r#type().name_fmt()),
    }
}

/// TUI/console substitution — uses the type-suppressed inline
/// render so `Person({name}, age {age})` reads as
/// `Person("Ada", age 36)` rather than
/// `Person(String("Ada"), age u32(36))`.
fn substitute_template(template: &str, members: &[Member]) -> String {
    crate::debugger::viz::substitute_template(template, members, |m| {
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

/// Apply a `#[bs_viz(format = "...")]` override to a scalar
/// value. Returns `Some(rendered)` when the value is a scalar
/// integer (or, for `utf8`/`hexdump`, a byte slice we can read)
/// and the format is applicable. Otherwise `None`, signalling
/// the caller should fall back to default rendering.
fn format_scalar(value: &Value, fmt: Format) -> Option<String> {
    if let Value::Scalar(s) = value {
        let n = s.try_as_number()?;
        return Some(match fmt {
            Format::Hex => format!("{:#x}", n),
            Format::Bin => format!("{:#b}", n),
            Format::Oct => format!("{:#o}", n),
            // iso8601 / duration only meaningful for time types,
            // utf8 / hexdump only for byte arrays. Step 3
            // implements the integer formats; the others fall
            // back to default rendering until their type-specific
            // decoders land.
            Format::Default
            | Format::Iso8601
            | Format::Duration
            | Format::Utf8
            | Format::Hexdump => return None,
        });
    }
    // Non-scalar — let the default renderer handle it.
    None
}

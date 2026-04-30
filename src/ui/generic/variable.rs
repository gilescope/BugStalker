// SPDX-License-Identifier: MIT
use crate::debugger::address::RelocatedAddress;
use crate::debugger::variable::execute::{QueryResult, QueryResultKind};
use crate::debugger::variable::render::{RenderValue, ValueLayout};
use crate::debugger::variable::value::Member;
use crate::debugger::variable::value::Value;
use crate::debugger::viz::VizRegistry;
use crate::ui::syntax;
use crate::ui::syntax::StylizedLine;
use bs_viz_spec::TypeViewSpec;
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
                    let (display_name, hidden) =
                        apply_field_overrides(spec, member.field_name.as_deref());
                    if hidden {
                        continue;
                    }
                    render = format!("{render}\n");
                    render = format!(
                        "{render}{tabs}{}: {}",
                        display_name.unwrap_or_default(),
                        render_value_inner(&member.value, depth + 1, true, viz)
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

/// Resolve field name + visibility against a spec entry. Returns
/// `(displayed_name, hidden)`. With no spec or no entry for this
/// field, the raw name is returned and `hidden` is `false`.
fn apply_field_overrides<'a>(
    spec: Option<&'a TypeViewSpec>,
    field_name: Option<&'a str>,
) -> (Option<&'a str>, bool) {
    let Some(name) = field_name else {
        return (None, false);
    };
    let Some(spec) = spec else {
        return (Some(name), false);
    };
    match spec.fields.iter().find(|f| f.name == name) {
        Some(f) => {
            let display = f
                .rename
                .as_deref()
                .or(Some(name));
            (display, f.hidden)
        }
        None => (Some(name), false),
    }
}

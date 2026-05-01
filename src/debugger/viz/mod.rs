// SPDX-License-Identifier: MIT
//! Phase 4 Tier-A — declarative visualiser registry.
//!
//! Reads `.bs_viz_spec` (Linux/ELF) and `__DATA,__bs_viz_spec`
//! (Mach-O) sections out of the debuggee object file at startup,
//! decodes each entry via [`bs_viz_spec::decode_all`], and indexes
//! the result by type name. Lookup falls back to a *suffix*
//! match because the proc-macro currently emits the local-module
//! name (`Person`) rather than the fully-qualified one
//! (`my_crate::Person`); see the comment on `Lookup::find` for
//! why that's safe enough for step 1.
//!
//! The registry is read-only after construction. Mutation would
//! mean the debug state was out of sync with the binary, which
//! is exactly the bug class this design avoids.

use std::collections::HashMap;

use bs_viz_spec::TypeViewSpec;
use object::{Object, ObjectSection};

use crate::debugger::variable::value::Member;

/// Substitute `{field_name}` placeholders in `template`. `{{` /
/// `}}` produce literal braces. Unknown placeholders render as
/// `{?name}` so a typo is visible rather than silent. The
/// caller-supplied `render_field` closure decides how each
/// member's value gets stringified — DAP wants compact one-line
/// renders, TUI wants the type-suppressed inline form.
pub fn substitute_template(
    template: &str,
    members: &[Member],
    mut render_field: impl FnMut(&Member) -> String,
) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'{' && bytes.get(i + 1) == Some(&b'{') {
            out.push('{');
            i += 2;
        } else if b == b'}' && bytes.get(i + 1) == Some(&b'}') {
            out.push('}');
            i += 2;
        } else if b == b'{' {
            let start = i + 1;
            let end = match bytes[start..].iter().position(|&c| c == b'}') {
                Some(p) => start + p,
                None => {
                    out.push('{');
                    i += 1;
                    continue;
                }
            };
            let name = &template[start..end];
            match members
                .iter()
                .find(|m| m.field_name.as_deref() == Some(name))
            {
                Some(m) => out.push_str(&render_field(m)),
                None => {
                    out.push_str("{?");
                    out.push_str(name);
                    out.push('}');
                }
            }
            i = end + 1;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

/// Section names. ELF tolerates dots in section names; Mach-O
/// caps the `sectname` at 16 chars and pairs it with a segment
/// name. We carry both literals so a single binary can be
/// inspected on either platform without a cfg dance at the call
/// site.
const SECT_LINUX: &str = ".bs_viz_spec";
const SECT_DARWIN: &str = "__bs_viz_spec";

/// Strip the trailing `<...>` generic-args block from a type
/// name, with proper bracket-depth tracking so nested generics
/// (`HashMap<K, Vec<i32>>`) are removed cleanly. Returns the
/// original slice when no `<>` block is present, so callers can
/// cheaply compare for "did anything change".
fn strip_generic_args(name: &str) -> &str {
    let bytes = name.as_bytes();
    // Find the *outermost* `<` that opens the trailing block.
    // A trailing `>` is required for it to be a generic-args
    // block (rust doesn't have stray `<` in type names).
    if !name.ends_with('>') {
        return name;
    }
    // Walk the bytes once, tracking depth, and remember the
    // index of the matching `<` for the trailing `>`.
    let mut depth: i32 = 0;
    let mut open_idx: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' => {
                if depth == 0 {
                    open_idx = Some(i);
                }
                depth += 1;
            }
            b'>' => {
                depth -= 1;
            }
            _ => {}
        }
    }
    // Well-formed name with balanced brackets: depth == 0 at end.
    // Otherwise leave the name alone — better to miss a match
    // than mis-truncate a malformed name.
    if depth == 0
        && let Some(open) = open_idx
    {
        return &name[..open];
    }
    name
}

/// In-memory registry of every `#[derive(DebugView)]` spec
/// recovered from the debuggee.
#[derive(Debug, Clone, Default)]
pub struct VizRegistry {
    by_name: HashMap<String, TypeViewSpec>,
}

impl VizRegistry {
    /// Construct an empty registry. Cheap; primarily for
    /// platforms or builds where the section is absent.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Walk every section of `obj`, decode the spec entries it
    /// finds, and return the registry. Sections that don't match
    /// our names are ignored. Sections that match but contain
    /// trailing garbage produce a warning via `log::warn` but
    /// don't fail the load — the prefix that decoded cleanly is
    /// still indexed.
    pub fn from_object(obj: &object::File<'_>) -> Self {
        let mut by_name: HashMap<String, TypeViewSpec> = HashMap::new();

        for section in obj.sections() {
            let name = section.name().unwrap_or("");
            if name != SECT_LINUX && name != SECT_DARWIN {
                continue;
            }
            let Ok(bytes) = section.data() else {
                log::warn!(target: "viz", "{name}: section data unreadable");
                continue;
            };
            let (specs, err) = bs_viz_spec::decode_all(bytes);
            if let Some(e) = err {
                log::warn!(target: "viz", "{name}: stopped decoding mid-section: {e}");
            }
            for spec in specs {
                // Last writer wins. In practice the linker only
                // emits one entry per type per crate; cross-crate
                // duplicates are vanishingly rare and "the most
                // recently linked one" is a reasonable rule.
                by_name.insert(spec.type_name.clone(), spec);
            }
        }

        Self { by_name }
    }

    /// Return the spec whose `type_name` matches `query`. Match
    /// rules, tried in order:
    ///
    /// 1. **Exact match.** If a spec was registered under
    ///    exactly this name, return it.
    /// 2. **Strip generic args.** `Wrap<i32>` → `Wrap`. The
    ///    macro emits one spec per type *definition*, not per
    ///    monomorphisation; the renderer hands us the
    ///    monomorphised name from the v0 demangler, so we strip
    ///    `<...>` (with proper depth tracking — `HashMap<K,
    ///    Vec<i32>>` has nested `<>` pairs) before retrying.
    /// 3. **Suffix match against `::<query>`.** The proc-macro
    ///    currently emits the *local* type name (e.g. `Person`)
    ///    rather than the fully-qualified one
    ///    (`my_crate::Person`). The renderer asks with the
    ///    fully-qualified form, so a query of
    ///    `my_crate::Person` matches a registered `Person`. If
    ///    multiple specs end with the same suffix, the lookup
    ///    is ambiguous and returns `None` — caller should treat
    ///    "ambiguous" the same as "no spec" so we don't apply
    ///    the wrong template silently. Once the macro records
    ///    `module_path!()` (later batch) this fallback becomes
    ///    unreachable.
    pub fn find(&self, query: &str) -> Option<&TypeViewSpec> {
        if let Some(spec) = self.by_name.get(query) {
            return Some(spec);
        }
        // Generic-stripped exact + suffix match.
        let stripped = strip_generic_args(query);
        if stripped != query {
            if let Some(spec) = self.by_name.get(stripped) {
                return Some(spec);
            }
        }
        // Suffix match — operates on the already-stripped form
        // so `my_crate::Wrap<i32>` matches a registered `Wrap`.
        let mut hit: Option<&TypeViewSpec> = None;
        for (key, spec) in &self.by_name {
            if stripped.len() > key.len() + 2
                && stripped.ends_with(key)
                && stripped.as_bytes()[stripped.len() - key.len() - 2..stripped.len() - key.len()]
                    == *b"::"
            {
                if hit.is_some() {
                    return None;
                }
                hit = Some(spec);
            }
        }
        hit
    }

    /// Total number of indexed specs.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True iff no specs were found.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Iterate every (type_name, spec) pair. Order is
    /// unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TypeViewSpec)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_viz_spec::{FieldSpec, Format, TypeViewSpec};

    fn populate() -> VizRegistry {
        let mut by_name = HashMap::new();
        by_name.insert(
            "Person".to_string(),
            TypeViewSpec {
                type_name: "Person".to_string(),
                summary: Some("Person({name})".to_string()),
                fields: vec![FieldSpec {
                    name: "name".to_string(),
                    rename: None,
                    hidden: false,
                    format: Format::Default,
                }],
                variants: vec![],
            },
        );
        by_name.insert(
            "other::Thing".to_string(),
            TypeViewSpec {
                type_name: "other::Thing".to_string(),
                summary: None,
                fields: vec![],
                variants: vec![],
            },
        );
        VizRegistry { by_name }
    }

    #[test]
    fn exact_match() {
        let r = populate();
        assert!(r.find("Person").is_some());
        assert_eq!(r.find("Person").unwrap().summary.as_deref(), Some("Person({name})"));
    }

    #[test]
    fn suffix_match_qualified_name() {
        let r = populate();
        assert!(r.find("my_crate::Person").is_some());
        assert!(r.find("a::b::c::Person").is_some());
    }

    #[test]
    fn suffix_match_requires_module_separator() {
        let r = populate();
        // "MyPerson" must NOT match "Person" — otherwise we'd
        // mis-apply the spec to unrelated types.
        assert!(r.find("MyPerson").is_none());
        assert!(r.find("xPerson").is_none());
    }

    #[test]
    fn ambiguous_suffix_returns_none() {
        let mut r = populate();
        r.by_name.insert(
            "twin::Person".to_string(),
            TypeViewSpec {
                type_name: "twin::Person".to_string(),
                summary: None,
                fields: vec![],
                variants: vec![],
            },
        );
        // `crate::twin::Person` suffix-matches *both* registered
        // keys: `Person` (single-segment suffix) and
        // `twin::Person` (multi-segment suffix). The lookup
        // bails to `None` rather than picking one arbitrarily.
        assert!(r.find("crate::twin::Person").is_none());
        // Exact match still wins regardless of any suffix-match
        // ambiguity that could otherwise apply.
        assert!(r.find("Person").is_some());
        assert!(r.find("twin::Person").is_some());
    }

    #[test]
    fn miss_returns_none() {
        let r = populate();
        assert!(r.find("NotARegisteredType").is_none());
    }

    #[test]
    fn strip_generic_args_basic() {
        assert_eq!(strip_generic_args("Wrap<i32>"), "Wrap");
        assert_eq!(strip_generic_args("Wrap"), "Wrap");
        assert_eq!(strip_generic_args(""), "");
    }

    #[test]
    fn strip_generic_args_nested() {
        assert_eq!(
            strip_generic_args("HashMap<K, Vec<i32>>"),
            "HashMap"
        );
        assert_eq!(
            strip_generic_args("a::b::Wrap<Vec<HashMap<K, V>>>"),
            "a::b::Wrap",
        );
    }

    #[test]
    fn strip_generic_args_keeps_malformed() {
        // No trailing `>` → leave alone.
        assert_eq!(strip_generic_args("Wrap<i32"), "Wrap<i32");
        // Unbalanced → leave alone (better miss than mistruncate).
        assert_eq!(strip_generic_args("Wrap>"), "Wrap>");
    }

    #[test]
    fn find_with_generics() {
        let mut by_name = HashMap::new();
        by_name.insert(
            "Wrap".to_string(),
            TypeViewSpec {
                type_name: "Wrap".to_string(),
                summary: Some("Wrap[{inner}]".to_string()),
                fields: vec![],
                variants: vec![],
            },
        );
        let r = VizRegistry { by_name };
        assert!(r.find("Wrap<i32>").is_some());
        assert!(r.find("Wrap<Vec<u8>>").is_some());
        assert!(r.find("my_crate::Wrap<i32>").is_some());
        assert!(r.find("Other<i32>").is_none());
    }
}

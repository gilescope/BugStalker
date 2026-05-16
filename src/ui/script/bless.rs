// SPDX-License-Identifier: MIT
//! Surgical CST-style edit of `expect:` blocks in a JSON5 test script.
//!
//! When the test runner finds an assertion mismatch under `--bless`, it
//! rewrites that assertion's `expect:` value in place — preserving every
//! comment, blank line, and field ordering elsewhere in the file.
//!
//! Why not a full JSON5 round-tripper? The script is a flat sequence of
//! top-level objects, each `assert.*` request has its `expect` at a
//! predictable depth (`params.expect`), and the only edit we ever make
//! is "replace this one value." A targeted scanner — string-aware,
//! comment-aware, brace-balanced — is ~250 LOC and never reflows the
//! rest of the file.
//!
//! The recorded `got` value also passes through an address-masking
//! step: any literal string matching `0x[0-9a-fA-F]{4,}` is rewritten
//! into a `{ "$regex": "0x[0-9a-fA-F]+" }` operator object. Same idea
//! as `cargo insta`'s filters, opt-out via `--bless --no-masks` if you
//! actually want to assert on a specific address.

use serde_json::Value;

/// One assert.* request's `expect:` field — byte span in the file plus
/// the request index for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectSlot {
    /// 0-based index among the `assert.*` requests in source order.
    /// Lines up with the TAP test number minus one.
    pub assert_idx: usize,
    /// Method name (`"assert.var"`, `"assert.frame"`, …).
    pub method: String,
    /// Byte span of the value following `expect:`. If the request had
    /// no `expect:` field, this is `None` and `--bless` will insert a
    /// fresh entry just before the closing `}` of `params`.
    pub value_span: Option<(usize, usize)>,
    /// Byte span of the `params` object's interior — used when
    /// inserting a missing `expect:` field.
    pub params_interior: Option<(usize, usize)>,
}

/// Walk a script and surface every `assert.*` request's expect slot.
pub fn slots_in(source: &str) -> Vec<ExpectSlot> {
    let mut out = Vec::new();
    let mut assert_idx = 0usize;
    for (start, end) in top_level_objects(source) {
        let chunk = &source[start..end];
        let Some(method) = field_string_value(chunk, "method") else {
            continue;
        };
        if !method.starts_with("assert.") {
            continue;
        }
        let params_span = field_object_value_span(chunk, "params");
        let params_interior = params_span.map(|(s, e)| (start + s + 1, start + e - 1));
        let value_span = params_span.and_then(|(ps, pe)| {
            let params_text = &source[start + ps..start + pe];
            // params spans include the outer braces; pass the inner
            // text to the field finder.
            let inner_text = &params_text[1..params_text.len() - 1];
            field_value_span(inner_text, "expect")
                .map(|(s, e)| (start + ps + 1 + s, start + ps + 1 + e))
        });
        out.push(ExpectSlot {
            assert_idx,
            method,
            value_span,
            params_interior,
        });
        assert_idx += 1;
    }
    out
}

/// Apply a sequence of `(assert_idx, new_expect_value)` patches to the
/// source text. Patches are applied right-to-left so byte offsets stay
/// valid. Returns the rewritten text.
pub fn apply_patches(
    source: &str,
    slots: &[ExpectSlot],
    patches: &[(usize, Value)],
    address_mask: bool,
) -> Result<String, String> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (idx, value) in patches {
        let slot = slots
            .iter()
            .find(|s| s.assert_idx == *idx)
            .ok_or_else(|| format!("no assert slot for index {idx}"))?;
        let masked = if address_mask {
            mask_addresses(value.clone())
        } else {
            value.clone()
        };
        match slot.value_span {
            Some((s, e)) => {
                // Use the *line's* leading indent so nested children
                // align under the field, not after the `expect: `
                // colon. Otherwise rendering would step further and
                // further right.
                let rendered = render_json5_value(&masked, line_leading_indent(source, s));
                edits.push((s, e, rendered));
            }
            None => {
                let (_, end) = slot.params_interior.ok_or_else(|| {
                    format!(
                        "slot {} ({}) has neither expect nor params interior",
                        idx, slot.method
                    )
                })?;
                // Walk back over trailing whitespace inside the params
                // block so the new `expect:` lands right after the
                // previous field rather than after a blank line.
                let bytes = source.as_bytes();
                let mut splice_at = end;
                while splice_at > 0 && (bytes[splice_at - 1] as char).is_whitespace() {
                    splice_at -= 1;
                }
                let needs_comma =
                    splice_at > 0 && bytes[splice_at - 1] != b',' && bytes[splice_at - 1] != b'{';
                let field_indent = line_leading_indent(source, splice_at);
                let pad = " ".repeat(field_indent);
                let rendered = render_json5_value(&masked, field_indent);
                let prefix = if needs_comma { "," } else { "" };
                let insertion = format!("{prefix}\n{pad}expect: {rendered}\n");
                edits.push((splice_at, end, insertion));
            }
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = source.to_string();
    for (s, e, text) in edits {
        out.replace_range(s..e, &text);
    }
    Ok(out)
}

/// Render a JSON value as JSON5 with the script's prevailing style:
/// identifier-shaped object keys are unquoted (`name:` not `"name":`),
/// strings keep their double quotes. Short values render inline; long
/// ones lay out across lines, each indented by `base_indent` spaces.
pub fn render_json5_value(v: &Value, base_indent: usize) -> String {
    let mut out = String::new();
    render_into(v, base_indent, base_indent, &mut out);
    out
}

// `base_indent` flows through unchanged so per-line "back to the
// start of the value" rendering can hook in later; clippy flags it
// as unused-in-recursion until then.
#[allow(clippy::only_used_in_recursion)]
fn render_into(v: &Value, base_indent: usize, depth_indent: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&render_string(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            // Inline if short.
            let inline = inline_array(items);
            if inline.len() <= 60 && depth_indent + inline.len() <= 80 {
                out.push_str(&inline);
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                out.push('\n');
                out.push_str(&" ".repeat(depth_indent + 2));
                render_into(item, base_indent, depth_indent + 2, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
            }
            out.push('\n');
            out.push_str(&" ".repeat(depth_indent));
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let inline = inline_object(map);
            if inline.len() <= 60 && depth_indent + inline.len() <= 80 {
                out.push_str(&inline);
                return;
            }
            out.push('{');
            for (i, (k, vv)) in map.iter().enumerate() {
                out.push('\n');
                out.push_str(&" ".repeat(depth_indent + 2));
                out.push_str(&render_key(k));
                out.push_str(": ");
                render_into(vv, base_indent, depth_indent + 2, out);
                if i + 1 < map.len() {
                    out.push(',');
                }
            }
            out.push('\n');
            out.push_str(&" ".repeat(depth_indent));
            out.push('}');
        }
    }
}

fn inline_array(items: &[Value]) -> String {
    let parts: Vec<String> = items.iter().map(inline_value).collect();
    format!("[{}]", parts.join(", "))
}

fn inline_object(map: &serde_json::Map<String, Value>) -> String {
    let parts: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{}: {}", render_key(k), inline_value(v)))
        .collect();
    format!("{{ {} }}", parts.join(", "))
}

fn inline_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => render_string(s),
        Value::Array(items) => inline_array(items),
        Value::Object(map) => inline_object(map),
    }
}

/// Identifier-shaped keys render unquoted (JSON5 style). Everything
/// else falls back to a quoted form.
fn render_key(k: &str) -> String {
    let is_ident = !k.is_empty()
        && k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if is_ident {
        k.to_string()
    } else {
        render_string(k)
    }
}

fn render_string(s: &str) -> String {
    // Reuse serde_json's escape logic — JSON5 strings are a superset.
    serde_json::Value::String(s.to_string()).to_string()
}

/// Column (zero-based) of the byte at `pos` within its line.
#[allow(dead_code)] // helper kept for follow-up bless features
fn indent_of_value(source: &str, pos: usize) -> usize {
    let line_start = source[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    pos.saturating_sub(line_start)
}

/// Number of leading whitespace bytes on the line that contains `pos`.
/// This is the indent of the *line*, not the column of `pos` within it.
fn line_leading_indent(source: &str, pos: usize) -> usize {
    let bytes = source.as_bytes();
    let line_start = source[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut i = line_start;
    while i < bytes.len() && bytes[i] != b'\n' && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    i - line_start
}

/// Replace every literal hex-address string with a `$regex` operator
/// object. Only fires on strings that look strictly address-shaped
/// (`0x` + 4+ hex digits, possibly wrapped in whitespace).
pub fn mask_addresses(v: Value) -> Value {
    match v {
        Value::String(s) => mask_address_string(&s),
        Value::Array(items) => Value::Array(items.into_iter().map(mask_addresses).collect()),
        Value::Object(map) => {
            // Never recurse into something that's already an operator
            // form — caller already wrote what they meant.
            if map.keys().any(|k| k.starts_with('$')) {
                return Value::Object(map);
            }
            Value::Object(
                map.into_iter()
                    .map(|(k, vv)| (k, mask_addresses(vv)))
                    .collect(),
            )
        }
        other => other,
    }
}

fn mask_address_string(s: &str) -> Value {
    // Two common Rust-debug forms:
    //   "0xDEADBEEF" — a bare address
    //   "&i32 [0x0000...]" — pointer rendering with the address in []
    // Auto-mask both, by replacing every embedded address run with a
    // regex placeholder and wrapping the whole thing as a $regex.
    let addr = regex::Regex::new(r"0x[0-9a-fA-F]{4,}").expect("static regex");
    if !addr.is_match(s) {
        return Value::String(s.to_string());
    }
    // Escape the parts of the string outside the address runs, then
    // splice in the regex pattern for the address itself.
    let mut out = String::with_capacity(s.len() + 16);
    out.push('^');
    let mut cursor = 0;
    for m in addr.find_iter(s) {
        out.push_str(&regex::escape(&s[cursor..m.start()]));
        out.push_str("0x[0-9a-fA-F]+");
        cursor = m.end();
    }
    out.push_str(&regex::escape(&s[cursor..]));
    out.push('$');
    let mut obj = serde_json::Map::new();
    obj.insert("$regex".to_string(), Value::String(out));
    Value::Object(obj)
}

// -- low-level JSON5 scanning -----------------------------------------

/// Iterator over the byte ranges of each top-level `{ … }` object in a
/// JSON5 script. Skips comments and inter-object whitespace.
fn top_level_objects(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        i = skip_ws_and_comments(bytes, i);
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'{' {
            let end = match_brace(bytes, i, b'{', b'}');
            if let Some(end) = end {
                out.push((i, end + 1));
                i = end + 1;
            } else {
                break;
            }
        } else {
            // Unexpected token — skip to end of line and continue
            // (caller's responsibility to feed well-formed input).
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        }
    }
    out
}

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        // Whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        // Line comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        break;
    }
    i
}

/// Find the matching close brace/bracket starting at `start` (which
/// must hold `open`). Returns the byte index of the close, or `None`
/// on imbalance. String contents (single or double quoted) are skipped.
fn match_brace(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    debug_assert!(bytes[start] == open);
    let mut depth = 0i32;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = i.saturating_add(2);
            continue;
        }
        if c == b'"' || c == b'\'' {
            i = skip_string(bytes, i);
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Skip a JSON5 string literal starting at `start` (where bytes[start]
/// is the opening quote). Returns the index *after* the closing quote.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// Look for a top-level key in an object's *interior* text (excluding
/// the outer braces). Returns the byte range of the key's value.
/// Searches at depth 0 so it skips into nested objects/arrays.
fn field_value_span(inner: &str, key: &str) -> Option<(usize, usize)> {
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        i = skip_ws_and_comments(bytes, i);
        if i >= bytes.len() {
            break;
        }
        // Read the next key.
        let key_end;
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let q = bytes[i];
            let k_start = i + 1;
            let after = skip_string(bytes, i);
            let k_end = after - 1; // before closing quote
            let k = std::str::from_utf8(&bytes[k_start..k_end]).ok()?;
            i = after;
            // After the key, skip ws and expect `:`.
            i = skip_ws_and_comments(bytes, i);
            if i >= bytes.len() || bytes[i] != b':' {
                return None;
            }
            i += 1;
            i = skip_ws_and_comments(bytes, i);
            if k == key {
                return Some(value_span_at(bytes, i));
            }
            i = value_end(bytes, i);
            i = skip_after_value(bytes, i);
            let _ = q;
        } else {
            // Unquoted (JSON5) identifier key.
            let k_start = i;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            key_end = i;
            let k = std::str::from_utf8(&bytes[k_start..key_end]).ok()?;
            i = skip_ws_and_comments(bytes, i);
            if i >= bytes.len() || bytes[i] != b':' {
                return None;
            }
            i += 1;
            i = skip_ws_and_comments(bytes, i);
            if k == key {
                return Some(value_span_at(bytes, i));
            }
            i = value_end(bytes, i);
            i = skip_after_value(bytes, i);
        }
    }
    None
}

/// Return the value at `i` as a `Value` decoded by json5. We only need
/// it for the `method:` field which is always a string, but accept
/// anything for robustness.
fn field_string_value(chunk: &str, key: &str) -> Option<String> {
    // chunk is `{...}` — drop the outer braces before searching.
    let trimmed = chunk.trim();
    let inner = &trimmed[1..trimmed.len() - 1];
    let (s, e) = field_value_span(inner, key)?;
    let value_text = &inner[s..e];
    let v: Value = json5::from_str(value_text).ok()?;
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// Return the `params` object's byte span within the request chunk
/// (chunk = `{…}`). Span includes the outer braces. Returns offsets
/// relative to the chunk.
fn field_object_value_span(chunk: &str, key: &str) -> Option<(usize, usize)> {
    let trimmed = chunk.trim();
    let leading = chunk.len()
        - trimmed.len()
        - (chunk.len() - trimmed.len() - (chunk.len() - trimmed.trim_start().len()));
    let _ = leading; // intentionally unused; we use `chunk.find('{')` below
    let open = chunk.find('{')?;
    let inner_start = open + 1;
    let inner = &chunk[inner_start..chunk.len() - 1]; // strip outer braces
    let (s, e) = field_value_span(inner, key)?;
    Some((inner_start + s, inner_start + e))
}

fn value_span_at(bytes: &[u8], start: usize) -> (usize, usize) {
    (start, value_end(bytes, start))
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return start;
    }
    match bytes[start] {
        b'{' => match_brace(bytes, start, b'{', b'}')
            .map(|e| e + 1)
            .unwrap_or(bytes.len()),
        b'[' => match_brace(bytes, start, b'[', b']')
            .map(|e| e + 1)
            .unwrap_or(bytes.len()),
        b'"' | b'\'' => skip_string(bytes, start),
        _ => {
            let mut i = start;
            while i < bytes.len()
                && bytes[i] != b','
                && bytes[i] != b'}'
                && bytes[i] != b']'
                && bytes[i] != b'\n'
            {
                i += 1;
            }
            i
        }
    }
}

fn skip_after_value(bytes: &[u8], mut i: usize) -> usize {
    i = skip_ws_and_comments(bytes, i);
    if i < bytes.len() && bytes[i] == b',' {
        i += 1;
    }
    i
}

fn is_ident_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'0'..=b'9' | b'$' | b'.')
}

// -- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_one_top_level_object() {
        let src = "{ method: \"x\" }";
        let spans = top_level_objects(src);
        assert_eq!(spans, vec![(0, src.len())]);
    }

    #[test]
    fn finds_two_back_to_back_objects_with_comment() {
        let src = "// hi\n{ a: 1 }\n// gap\n{ b: 2 }\n";
        let spans = top_level_objects(src);
        assert_eq!(spans.len(), 2);
        assert_eq!(&src[spans[0].0..spans[0].1], "{ a: 1 }");
        assert_eq!(&src[spans[1].0..spans[1].1], "{ b: 2 }");
    }

    #[test]
    fn ignores_braces_inside_strings() {
        let src = r#"{ s: "}{}{}" }"#;
        let spans = top_level_objects(src);
        assert_eq!(spans, vec![(0, src.len())]);
    }

    #[test]
    fn finds_method_value() {
        let chunk = "{ method: \"assert.var\", params: {} }";
        assert_eq!(
            field_string_value(chunk, "method").as_deref(),
            Some("assert.var")
        );
    }

    #[test]
    fn finds_expect_span_in_assert_var() {
        let src = r#"{
  method: "assert.var",
  params: { name: "x", expect: { value_text: "i32(7)" }, hint: "h" }
}"#;
        let slots = slots_in(src);
        assert_eq!(slots.len(), 1);
        let slot = &slots[0];
        let (s, e) = slot.value_span.unwrap();
        assert_eq!(&src[s..e], r#"{ value_text: "i32(7)" }"#);
    }

    #[test]
    fn slots_skip_non_assert_methods() {
        let src = r#"{ method: "break.set", params: { at: "x" } }
{ method: "assert.var", params: { name: "x", expect: { v: 1 } } }
{ method: "run" }
{ method: "assert.frame", params: { expect: { line: 7 } } }"#;
        let slots = slots_in(src);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].assert_idx, 0);
        assert_eq!(slots[0].method, "assert.var");
        assert_eq!(slots[1].assert_idx, 1);
        assert_eq!(slots[1].method, "assert.frame");
    }

    #[test]
    fn apply_patches_replaces_one_expect() {
        let src = r#"{
  method: "assert.var",
  params: { name: "x", expect: { v: 1 } }
}"#;
        let slots = slots_in(src);
        let out = apply_patches(src, &slots, &[(0, json!({"v": 99}))], false).unwrap();
        // The render uses JSON5-style unquoted identifier keys.
        assert!(
            out.contains("expect: { v: 99 }"),
            "did not see the rendered expect block:\n{out}"
        );
        // Surrounding comments / formatting preserved.
        assert!(out.starts_with("{\n  method: \"assert.var\","));
    }

    #[test]
    fn apply_patches_is_idempotent() {
        let src = r#"{
  method: "assert.var",
  params: { name: "x", expect: { v: 42 } }
}"#;
        let slots = slots_in(src);
        let out1 = apply_patches(src, &slots, &[(0, json!({"v": 42}))], false).unwrap();
        let slots2 = slots_in(&out1);
        let out2 = apply_patches(&out1, &slots2, &[(0, json!({"v": 42}))], false).unwrap();
        assert_eq!(out1, out2);
    }

    #[test]
    fn mask_addresses_rewrites_inline_addr() {
        let v = mask_addresses(json!("0xDEADBEEF"));
        assert_eq!(v, json!({ "$regex": "^0x[0-9a-fA-F]+$" }));
    }

    #[test]
    fn mask_addresses_rewrites_pointer_render() {
        let v = mask_addresses(json!("&i32 [0x007FFFFFFFB494]"));
        // `regex::escape` is conservative — `&`, `[`, `]` all get
        // backslashed even though `&` isn't a metachar. That's still
        // a correct regex, just a touch noisier than necessary.
        assert_eq!(v, json!({ "$regex": r"^\&i32 \[0x[0-9a-fA-F]+\]$" }));
    }

    #[test]
    fn mask_addresses_preserves_existing_operators() {
        // Don't double-wrap a hand-written $regex / $contains / etc.
        let v = mask_addresses(json!({ "$regex": "0xDEAD" }));
        assert_eq!(v, json!({ "$regex": "0xDEAD" }));
    }

    #[test]
    fn mask_addresses_recurses_into_arrays_and_objects() {
        let v = mask_addresses(json!({
            "items": [{ "value_text": "0x123456" }],
            "literal": "not an addr"
        }));
        assert_eq!(
            v,
            json!({
                "items": [{ "value_text": { "$regex": "^0x[0-9a-fA-F]+$" } }],
                "literal": "not an addr"
            })
        );
    }

    #[test]
    fn missing_expect_block_gets_inserted() {
        let src = r#"{
  method: "assert.var",
  params: { name: "x" }
}"#;
        let slots = slots_in(src);
        assert_eq!(slots[0].value_span, None);
        let out = apply_patches(src, &slots, &[(0, json!({ "v": 1 }))], false).unwrap();
        assert!(out.contains("expect:"));
        // Original `name:` survives.
        assert!(out.contains(r#"name: "x""#));
    }
}

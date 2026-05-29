// SPDX-License-Identifier: MIT
use super::ThreadFocusByPid;
use crate::dap::yadap::protocol::DapRequest;
use crate::debugger;
use crate::debugger::variable::render::RenderValue;
use crate::ui::command::parser::expression as bs_expr;
use anyhow::{Context, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_ENGINE};
use chumsky::Parser as _;
use nix::unistd::Pid;
use serde_json::json;
use std::rc::Rc;
use std::time::Instant;

#[derive(Clone)]
pub enum WriteMeta {
    Scalar {
        addr: usize,
        kind: ScalarKind,
    },
    Composite {
        addr: usize,
        type_graph: Rc<debugger::ComplexType>,
    },
}

#[derive(Clone, Copy)]
pub enum ScalarKind {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    Bool,
    Char,
}

#[derive(Clone)]
pub struct VarItem {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    pub child: Option<Vec<VarItem>>,
    pub write: Option<WriteMeta>,
    pub source: Option<debugger::variable::value::Value>,
    /// Variables-view §5.2: serialised as the DAP custom field
    /// `bugstalker.mutability = "ro" | "rw" | "unknown"`. The
    /// vscode-extension uses this to pick the row-background hue
    /// (grey for `ro`, orange for `rw`, none for `unknown`).
    /// `"ro"` additionally emits `presentationHint.attributes =
    /// ["readOnly"]` so stock DAP clients (default VSCode pane)
    /// italicise the row even without our extension.
    pub mutability: Option<&'static str>,
    /// Variables-view §5.3: serialised as the DAP custom field
    /// `bugstalker.storage = "stack" | "register" | "static_ro" |
    /// "static_rw" | "tls" | "optimized" | "unknown"`. Drives the
    /// leading storage-class glyph in the vscode-extension.
    pub storage: Option<&'static str>,
    /// Variables-view §5.3 heap overlay: when `true`, this
    /// binding's value points into a heap-ish mapping (the `↗`
    /// glyph layers on top of the storage class). Serialised as
    /// `bugstalker.points_to_heap = true` and omitted when false
    /// to keep DAP JSON tight.
    pub points_to_heap: bool,
    /// Variables-view §5.5 — total byte size of this value's type
    /// from `DW_AT_byte_size`. Drives the trailing size column +
    /// the amber (>1 KB) / red (>16 KB) tinting. Serialised as
    /// `bugstalker.byte_size` (u64). `None` is omitted from the JSON.
    pub byte_size: Option<u64>,
    /// Variables-view §5.6 — shallow payload / padding breakdown
    /// (struct types only). Drives the HSL lightness split on the
    /// row background in the vscode-extension. Serialised as
    /// `bugstalker.layout = { totalBytes, payloadBytes, paddingBytes }`
    /// only when padding ≥ 5% of total (the
    /// `paddingSplit.showThresholdPct` from variables-view §4) —
    /// below that the split is visual noise.
    pub layout: Option<debugger::variable::execute::LayoutBreakdown>,
}

impl super::DebugSession {
    pub(super) fn handle_variables(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let variables_reference = req
            .arguments
            .get("variablesReference")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("variables: missing arguments.variablesReference"))?;

        let vars = self
            .vars
            .get(variables_reference)
            .cloned()
            .unwrap_or_default();

        let mut out = Vec::new();
        for (index, v) in vars.into_iter().enumerate() {
            // `variablesReference == 0` tells the DAP client there are
            // no expandable children for this row, so the IDE doesn't
            // render the disclosure triangle. We also short-circuit
            // when the child list is `Some(empty vec)` — an empty
            // children list otherwise produces a useless expand-arrow
            // that opens onto nothing (the symptom for unit structs
            // like `struct Marker;`).
            let child_ref = match v.child.as_ref() {
                Some(child) if !child.is_empty() => {
                    if let Some(r) = self.child_links.get(&(variables_reference, index)).copied() {
                        r
                    } else {
                        let r = self.vars.alloc(child.clone());
                        self.child_links.insert((variables_reference, index), r);
                        r
                    }
                }
                _ => 0,
            };
            // Append the type to the name as `name : type` so the
            // Variables panel shows `name : type = value` in IDEs
            // (notably VSCode) that don't honour the DAP `type`
            // field inline by default. The displayed type drops
            // module-namespace prefixes (`HashMap` instead of
            // `std::collections::HashMap`, recursive through generic
            // args) so the panel stays readable; the DAP `type`
            // field still ships the full namespaced path so hover-
            // tooltips show the unabbreviated name.
            //
            // For fixed-size arrays specifically we also re-attach the
            // length: DWARF gives us `[i32]` but the actual Rust type
            // is `[i32; 4]`. The length is `value.items.len()` (or
            // `byte_size / element_size` for empty arrays), so we
            // re-write the outermost `[…]` to `[…; N]`. Slices stay
            // as `&[T]` — their type genuinely doesn't carry length.
            let display_type = v.type_name.as_deref().map(|t| {
                let stripped = strip_type_namespace(t);
                attach_array_length(&stripped, v.source.as_ref())
            });
            let name_with_type = match display_type.as_deref() {
                Some(t) if !t.is_empty() => format!("{} : {t}", v.name),
                _ => v.name.clone(),
            };
            // Variables-view §5.2: emit the mutability hint as
            // (a) the standard `presentationHint.attributes =
            // ["readOnly"]` for `ro` rows (so stock DAP clients
            // italicise without needing our extension), and
            // (b) a custom `bugstalker.mutability` field carrying
            // the raw "ro"/"rw" string so the vscode-extension can
            // paint the row-background hue.
            let mut entry = json!({
                "name": name_with_type,
                "value": v.value,
                "type": v.type_name,
                "variablesReference": child_ref,
            });
            if let Some(m) = v.mutability {
                entry["bugstalker.mutability"] = json!(m);
                if m == "ro" {
                    entry["presentationHint"] = json!({
                        "attributes": ["readOnly"],
                    });
                }
            }
            // Variables-view §5.3 — storage class + heap overlay.
            // Both emitted as custom fields; no stock DAP equivalent.
            if let Some(s) = v.storage {
                entry["bugstalker.storage"] = json!(s);
            }
            if v.points_to_heap {
                entry["bugstalker.points_to_heap"] = json!(true);
            }
            // Variables-view §5.5 — per-local byte size for the
            // trailing size column + threshold tinting.
            if let Some(bytes) = v.byte_size {
                entry["bugstalker.byte_size"] = json!(bytes);
            }
            // Variables-view §5.6 — payload/padding split. Only
            // emit when padding ≥ 5% of total (the
            // showThresholdPct from §4) — below that the HSL
            // split is visual noise.
            if let Some(layout) = v.layout
                && layout.padding_pct().is_some_and(|pct| pct >= 5)
            {
                entry["bugstalker.layout"] = json!({
                    "totalBytes": layout.total,
                    "payloadBytes": layout.payload,
                    "paddingBytes": layout.padding,
                });
            }
            out.push(entry);
        }

        self.send_success_body(req, json!({"variables": out}))
    }

    pub(super) fn handle_set_variable(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let vars_ref = req
            .arguments
            .get("variablesReference")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("setVariable: missing arguments.variablesReference"))?;

        let name = req
            .arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("setVariable: missing arguments.name"))?
            .to_string();

        let new_value = req
            .arguments
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("setVariable: missing arguments.value"))?
            .to_string();

        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("setVariable: debugger not initialized"))?;

        let mut child_ref_to_remove = None;
        let (reply_value, reply_type) = {
            let vars = self
                .vars
                .get_mut(vars_ref)
                .ok_or_else(|| anyhow!("setVariable: unknown variablesReference={vars_ref}"))?;

            let (index, item) = vars
                .iter_mut()
                .enumerate()
                .find(|(_, v)| v.name == name)
                .ok_or_else(|| anyhow!("setVariable: variable '{name}' not found"))?;

            let Some(write) = item.write.clone() else {
                self.send_err(
                    req,
                    "setVariable: target variable is not writable".to_string(),
                )?;
                return Ok(());
            };

            match write {
                WriteMeta::Scalar { addr, kind } => {
                    let bytes = parse_set_value(kind, &new_value)?;
                    write_bytes(dbg, addr, &bytes)?;
                }
                WriteMeta::Composite { addr, type_graph } => {
                    let Some(source) = item.source.as_ref() else {
                        self.send_err(
                            req,
                            "setVariable: target variable is missing source value".to_string(),
                        )?;
                        return Ok(());
                    };
                    let Some(type_id) = source.type_id() else {
                        self.send_err(
                            req,
                            "setVariable: target variable has no type id".to_string(),
                        )?;
                        return Ok(());
                    };
                    let serialized = debugger::variable::value::serialize::serialize_dap_value(
                        &new_value,
                        &type_graph,
                        type_id,
                        Some(source),
                    )
                    .map_err(|err| anyhow!("setVariable: {err}"))?;
                    write_bytes(dbg, addr, &serialized.bytes)?;
                    item.child = None;
                    if let Some(child_ref) = self.child_links.remove(&(vars_ref, index)) {
                        child_ref_to_remove = Some(child_ref);
                    }
                }
            }

            // Update cached presentation value for this stop epoch.
            item.value = new_value.clone();
            (item.value.clone(), item.type_name.clone())
        };
        if let Some(child_ref) = child_ref_to_remove {
            self.vars.remove(child_ref);
        }

        self.send_success_body(
            req,
            json!({
                "value": reply_value,
                "type": reply_type,
                "variablesReference": 0,
            }),
        )?;
        self.enqueue_invalidated(vec![
            "variables".to_string(),
            "stack".to_string(),
            "memory".to_string(),
        ]);
        self.drain_events()
    }

    pub(super) fn handle_read_memory(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("readMemory: debugger not initialized"))?;
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("readMemory: arguments must be object"))?;

        let memory_reference = args
            .get("memoryReference")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("readMemory: missing arguments.memoryReference"))?;
        let count = args
            .get("count")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow!("readMemory: missing arguments.count"))?;
        if count < 0 {
            return self.send_err(req, "readMemory: count must be non-negative");
        }

        let offset = args
            .get("offset")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let addr = super::parse_memory_reference_with_offset(memory_reference, offset)
            .context("readMemory: invalid memoryReference")?;
        let start = Instant::now();
        let bytes = dbg
            .read_memory(addr, count as usize)
            .context("readMemory: read_memory")?;
        let elapsed = start.elapsed();
        if elapsed > super::MEMORY_READ_TIMEOUT {
            return self.send_err(
                req,
                format!(
                    "readMemory: read timed out after {}ms",
                    super::MEMORY_READ_TIMEOUT.as_millis()
                ),
            );
        }
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        let data = BASE64_ENGINE.encode(bytes);
        self.send_success_body(
            req,
            json!({
                "address": format!("0x{addr:x}"),
                "data": data,
            }),
        )
    }

    pub(super) fn handle_write_memory(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("writeMemory: debugger not initialized"))?;
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("writeMemory: arguments must be object"))?;

        let memory_reference = args
            .get("memoryReference")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("writeMemory: missing arguments.memoryReference"))?;
        let data = args
            .get("data")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("writeMemory: missing arguments.data"))?;
        let offset = args
            .get("offset")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let addr = super::parse_memory_reference_with_offset(memory_reference, offset)
            .context("writeMemory: invalid memoryReference")?;
        let bytes = BASE64_ENGINE
            .decode(data)
            .map_err(|err| anyhow!("writeMemory: base64 decode failed: {err}"))?;
        write_bytes(dbg, addr, &bytes).context("writeMemory: write_bytes")?;
        self.send_success_body(req, json!({ "bytesWritten": bytes.len() }))?;
        self.enqueue_invalidated(vec!["memory".to_string()]);
        self.drain_events()
    }

    pub(super) fn handle_set_expression(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_mut()
            .ok_or_else(|| anyhow!("setExpression: debugger not initialized"))?;

        let expression = req
            .arguments
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("setExpression: missing arguments.expression"))?;
        let new_value = req
            .arguments
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("setExpression: missing arguments.value"))?;

        if let Some(frame_value) = req.arguments.get("frameId") {
            let frame_id = frame_value
                .as_i64()
                .ok_or_else(|| anyhow!("setExpression: frameId must be an integer"))?;
            if frame_id < 0 {
                return self.send_err(req, "setExpression: frameId must be non-negative");
            }
            let (thread_id, frame_num) = Self::decode_frame_id(frame_id);
            let pid = self
                .thread_cache
                .get(&thread_id)
                .copied()
                .unwrap_or_else(|| Pid::from_raw(thread_id as i32));
            let _ = dbg.set_thread_into_focus_by_pid(pid);
            let _ = dbg.set_frame_into_focus(frame_num);
        }

        let dqe = bs_expr::parser()
            .parse(expression)
            .into_result()
            .map_err(|e| anyhow!("setExpression parse error: {e:?}"))?;
        let results = dbg
            .read_variable(dqe.clone())
            .context("setExpression read_variable")?;
        let result = results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("setExpression: expression produced no results"))?;
        let type_graph = Rc::new(result.type_graph().clone());
        let (_id, value) = result.into_identified_value();

        let Some(write_meta) = value_write_meta(&value, type_graph.clone()) else {
            return self.send_err(req, "setExpression: expression is not writable");
        };

        match write_meta {
            WriteMeta::Scalar { addr, kind } => {
                let bytes = parse_set_value(kind, new_value)?;
                write_bytes(dbg, addr, &bytes)?;
            }
            WriteMeta::Composite { addr, type_graph } => {
                let Some(type_id) = value.type_id() else {
                    return self.send_err(req, "setExpression: expression has no type id");
                };
                let serialized = debugger::variable::value::serialize::serialize_dap_value(
                    new_value,
                    &type_graph,
                    type_id,
                    Some(&value),
                )
                .map_err(|err| anyhow!("setExpression: {err}"))?;
                write_bytes(dbg, addr, &serialized.bytes)?;
            }
        }

        let refreshed = dbg
            .read_variable(dqe)
            .context("setExpression read_variable (refresh)")?;
        let response = if let Some(updated) = refreshed.into_iter().next() {
            let type_graph = Rc::new(updated.type_graph().clone());
            let viz = Some(dbg.view_registry());
            let child = value_children(&updated, type_graph, viz);
            let vars_ref = child.map(|c| self.vars.alloc(c)).unwrap_or(0);
            json!({
                "value": render_value_to_string_with_viz(updated.value(), viz),
                "type": updated.value().r#type().name_fmt(),
                "variablesReference": vars_ref,
            })
        } else {
            json!({
                "value": new_value,
                "variablesReference": 0,
            })
        };

        self.send_success_body(req, response)?;
        self.enqueue_invalidated(vec![
            "variables".to_string(),
            "stack".to_string(),
            "memory".to_string(),
        ]);
        self.drain_events()
    }

    pub(super) fn handle_evaluate(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        let expression = req
            .arguments
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("evaluate: missing arguments.expression"))?;

        let (body, elapsed) = {
            let dbg = self
                .debugger
                .as_mut()
                .ok_or_else(|| anyhow!("evaluate: debugger not initialized"))?;

            // Optional frameId: if provided, focus thread/frame so evaluation is stable.
            if let Some(frame_id) = req.arguments.get("frameId").and_then(|v| v.as_i64()) {
                let (thread_id, frame_num) = Self::decode_frame_id(frame_id);
                let pid = self
                    .thread_cache
                    .get(&thread_id)
                    .copied()
                    .unwrap_or_else(|| Pid::from_raw(thread_id as i32));
                let _ = dbg.set_thread_into_focus_by_pid(pid);
                let _ = dbg.set_frame_into_focus(frame_num);
            }

            let dqe = bs_expr::parser()
                .parse(expression)
                .into_result()
                .map_err(|e| anyhow!("evaluate parse error: {e:?}"))?;

            let start = Instant::now();
            let results = dbg.read_variable(dqe).context("evaluate read_variable")?;
            let elapsed = start.elapsed();
            let body = if results.is_empty() {
                json!({"result": "<no result>", "variablesReference": 0})
            } else {
                let result = results.into_iter().next().unwrap();
                let type_graph = Rc::new(result.type_graph().clone());
                let viz = Some(dbg.view_registry());
                let child = value_children(&result, type_graph, viz);
                let vars_ref = child.map(|c| self.vars.alloc(c)).unwrap_or(0);
                let result_str = render_value_to_string_with_viz(result.value(), viz);
                json!({"result": result_str, "variablesReference": vars_ref})
            };
            (body, elapsed)
        };
        if elapsed > super::DEBUGGER_RESPONSE_TIMEOUT {
            return self.send_err(
                req,
                format!(
                    "evaluate: debugger response timed out after {}ms",
                    super::DEBUGGER_RESPONSE_TIMEOUT.as_millis()
                ),
            );
        }
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        self.send_success_body(req, body)
    }
}

fn write_bytes(dbg: &debugger::Debugger, addr: usize, bytes: &[u8]) -> anyhow::Result<()> {
    let word = std::mem::size_of::<usize>();
    if bytes.is_empty() {
        return Ok(());
    }

    let start = addr;
    let end = addr + bytes.len();
    let mut cur = start;

    while cur < end {
        let word_start = (cur / word) * word;
        let word_end = word_start + word;
        let chunk_from = std::cmp::max(cur, word_start);
        let chunk_to = std::cmp::min(end, word_end);

        let mut existing = dbg.read_memory(word_start, word).context("read_memory")?;
        let src_off = chunk_from - start;
        let dst_off = chunk_from - word_start;
        existing[dst_off..dst_off + (chunk_to - chunk_from)]
            .copy_from_slice(&bytes[src_off..src_off + (chunk_to - chunk_from)]);

        let mut le = [0u8; std::mem::size_of::<usize>()];
        le.copy_from_slice(&existing[..word]);
        let value = usize::from_le_bytes(le);

        dbg.write_memory(word_start as _, value as _)
            .context("write_memory")?;

        cur = word_end;
    }

    Ok(())
}

pub fn render_value_to_string(v: &debugger::variable::value::Value) -> String {
    render_value_to_string_with_viz(v, None)
}

/// Walk a chain of eagerly-deref'd pointers and return
/// `(depth, leaf)` where `depth` is how many layers of indirection
/// were peeled through and `leaf` is the underlying non-pointer
/// value. Recognises the same family the parser eagerly-derefs
/// today: `alloc::boxed::Box<T>` only. Reference / raw-pointer /
/// Rc/Arc traversal was added earlier in this session but the Rc /
/// Arc payload-peel was bisected as the trigger for a debug-session
/// crash in `showcase`, so the walker is now Pointer-only.
///
/// Stops at the first link whose `dereffed` is `None`.
fn count_eager_derefs(
    v: &debugger::variable::value::Value,
) -> (usize, &debugger::variable::value::Value) {
    use debugger::variable::value::Value;
    let mut depth = 0usize;
    let mut cur = v;
    const MAX_DEREF_WALK: usize = 64;
    while depth < MAX_DEREF_WALK {
        let next: Option<&Value> = match cur {
            Value::Pointer(p) => p.dereffed.as_deref(),
            _ => None,
        };
        match next {
            Some(inner) => {
                depth += 1;
                cur = inner;
            }
            None => break,
        }
    }
    (depth, cur)
}

/// Format the deref-depth prefix for a value at `n` layers of
/// indirection: small counts use repeated `*` for visual scannability,
/// larger counts switch to `*{N}` so the value doesn't grow a comically
/// long star prefix.
fn render_deref_prefix(n: usize) -> String {
    const MAX_INLINE_STARS: usize = 5;
    if n <= MAX_INLINE_STARS {
        "*".repeat(n)
    } else {
        format!("*{{{n}}}")
    }
}

/// Strip module-namespace prefixes from every identifier path in a
/// Rust type-name string. `core::option::Option<alloc::string::String>`
/// becomes `Option<String>`; `&[std::path::PathBuf]` becomes
/// `&[PathBuf]`; primitives, references, tuples, slices, and array
/// punctuation pass through unchanged.
///
/// Used to compute the short "display" type that goes into the
/// Variables-panel `name : type = value` rendering. The full,
/// namespaced type still ships in the DAP `type` field on the same
/// response, so hover-tooltips and IDEs that show the full type
/// elsewhere keep working.
///
/// Algorithm is a single byte-by-byte walk: when we hit an ASCII
/// identifier start, we read identifier segments separated by `::`,
/// remember the start of the last segment, and emit only that. Any
/// punctuation (`<`, `>`, `&`, `*`, `[`, `]`, `(`, `)`, `,`, space,
/// `;`) is passed through verbatim, which is what makes the
/// recursion-through-generics work for free — the next identifier
/// inside `<…>` or `[…]` is reached by exactly the same loop.
/// Rewrite an outer `[T]` array-type display as `[T; N]` when the
/// underlying value is a fixed-size array we can count. DWARF emits
/// the array type without the length attribute string-form, but the
/// length is part of the Rust array type (`[T; N]`). For slices
/// (`&[T]`, `&mut [T]`, `*const [T]`, `*mut [T]`) we leave the type
/// alone — those are slice types whose length is runtime-only.
///
/// Only rewrites the outermost `[…]` pair. Nested arrays inside
/// (e.g. `[[i32; 2]; 3]`) would need recursive per-element type
/// rewriting using each item's own Value — a follow-up if it
/// matters in practice.
fn attach_array_length(
    type_str: &str,
    source: Option<&debugger::variable::value::Value>,
) -> String {
    use debugger::variable::value::Value;
    // Only fixed-size arrays carry a length in the type. References
    // / pointers / slices and any other shape pass through.
    let Some(Value::Array(arr)) = source else {
        return type_str.to_string();
    };
    let Some(items) = arr.items.as_ref() else {
        return type_str.to_string();
    };
    // The type must look like `[…]` with the closing bracket as the
    // final character — otherwise this is `&[T]` (slice), `&mut [T]`,
    // or something else we shouldn't rewrite.
    let trimmed = type_str.trim_end();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return type_str.to_string();
    }
    let len = items.len();
    // Insert `; N` just before the closing bracket.
    let inner = &trimmed[1..trimmed.len() - 1];
    format!("[{inner}; {len}]")
}

// `strip_type_namespace` lives in `debugger::variable::render` so
// both the DAP layer and the dyn renderer can call it. Re-exported
// here so existing call sites keep working with no churn.
pub use crate::debugger::variable::render::strip_type_namespace;

/// Cap on the inline string length for collection previews. Beyond
/// this, the value column collapses to `(len=N) [...]` rather than
/// the joined elements; the user can still expand into the children.
const COLLECTION_PREVIEW_BUDGET: usize = 80;

fn indexed_list_preview(
    items: &[debugger::variable::value::ArrayItem],
    viz: Option<&debugger::viz::VizRegistry>,
) -> String {
    let len = items.len();
    // Build a comma-separated inline preview, stopping early when we
    // exceed the budget. That way short collections like `[1, 2, 3]`
    // render in full while long ones don't dominate the panel.
    let mut joined = String::new();
    let mut over_budget = false;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            joined.push_str(", ");
        }
        if joined.len() >= COLLECTION_PREVIEW_BUDGET {
            over_budget = true;
            break;
        }
        joined.push_str(&render_value_to_string_with_viz(&item.value, viz));
    }
    if over_budget || joined.len() >= COLLECTION_PREVIEW_BUDGET {
        format!("[...] (len={len})")
    } else {
        format!("[{joined}] (len={len})")
    }
}

/// Variant of `indexed_list_preview` without the `(len=N)` postfix.
/// Used for `Value::Array` where the length already appears in the
/// displayed type as `[T; N]`, so prefixing the value would be
/// repetitive. Falls back to `[...]` when the joined elements
/// exceed the budget — the user can still expand into children.
fn indexed_list_inline(
    items: &[debugger::variable::value::ArrayItem],
    viz: Option<&debugger::viz::VizRegistry>,
) -> String {
    let mut joined = String::new();
    let mut over_budget = false;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            joined.push_str(", ");
        }
        if joined.len() >= COLLECTION_PREVIEW_BUDGET {
            over_budget = true;
            break;
        }
        joined.push_str(&render_value_to_string_with_viz(&item.value, viz));
    }
    if over_budget || joined.len() >= COLLECTION_PREVIEW_BUDGET {
        "[...]".to_string()
    } else {
        format!("[{joined}]")
    }
}

fn non_indexed_list_preview(
    items: &[debugger::variable::value::Value],
    viz: Option<&debugger::viz::VizRegistry>,
) -> String {
    let len = items.len();
    let mut joined = String::new();
    let mut over_budget = false;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            joined.push_str(", ");
        }
        if joined.len() >= COLLECTION_PREVIEW_BUDGET {
            over_budget = true;
            break;
        }
        joined.push_str(&render_value_to_string_with_viz(item, viz));
    }
    if over_budget || joined.len() >= COLLECTION_PREVIEW_BUDGET {
        format!("{{...}} (len={len})")
    } else {
        format!("{{{joined}}} (len={len})")
    }
}

fn map_preview(
    entries: &[(
        debugger::variable::value::Value,
        debugger::variable::value::Value,
    )],
    viz: Option<&debugger::viz::VizRegistry>,
) -> String {
    let len = entries.len();
    let mut joined = String::new();
    let mut over_budget = false;
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            joined.push_str(", ");
        }
        if joined.len() >= COLLECTION_PREVIEW_BUDGET {
            over_budget = true;
            break;
        }
        joined.push_str(&render_value_to_string_with_viz(k, viz));
        joined.push_str(": ");
        joined.push_str(&render_value_to_string_with_viz(v, viz));
    }
    if over_budget || joined.len() >= COLLECTION_PREVIEW_BUDGET {
        format!("{{...}} (len={len})")
    } else {
        format!("{{{joined}}} (len={len})")
    }
}

/// A `Structure` value whose members are the positional fields of a
/// Rust tuple (or a tuple-struct rendered like a tuple). DWARF stores
/// these as a struct whose fields are named `__0`, `__1`, … and we
/// want to recognise them so the Variables panel renders them with
/// the source-level conventions: round brackets around the value,
/// `.N` for the child names.
///
/// Predicate: non-empty, and every member's field name is exactly
/// `__<i>` matching its positional index. Out-of-order or
/// mixed-naming structs (e.g. a one-field struct with a `__0` member
/// among other named fields) are NOT classified as tuples — those
/// are odd but real and we'd rather under-detect than mislabel.
fn is_tuple_structure(members: &[debugger::variable::value::Member]) -> bool {
    is_tuple_field_set(members.iter().map(|m| m.field_name.as_deref()))
}

/// Predicate-by-name version of [`is_tuple_structure`]. Splits out
/// the pure logic so it can be unit-tested without constructing
/// fake `Member` values (the `Member` struct's fields require
/// `TypeIdentity`, which lives in a private debugger sub-module).
fn is_tuple_field_set<'a, I: IntoIterator<Item = Option<&'a str>>>(names: I) -> bool {
    let mut count = 0usize;
    for (idx, name) in names.into_iter().enumerate() {
        count = idx + 1;
        match name {
            Some(n) if n == format!("__{idx}") => continue,
            _ => return false,
        }
    }
    count > 0
}

/// Wrap a comma-joined tuple body in parens for inline display,
/// adding the trailing comma Rust uses to disambiguate a 1-tuple
/// (`(x,)`) from a merely parenthesised value (`(x)`). Only *bare*
/// tuples get the comma — a single-field tuple struct or enum variant
/// (`Wrap(x)`, `Ok(x)`) reads without one. `bare_tuple` is true when
/// the value's own type name is parens-shaped (e.g. `(i32,)`).
fn wrap_tuple_body(joined: &str, member_count: usize, bare_tuple: bool) -> String {
    if bare_tuple && member_count == 1 {
        format!("({joined},)")
    } else {
        format!("({joined})")
    }
}

/// Phase 4 Tier-A — DAP renderer that consults the
/// [`debugger::viz::VizRegistry`]. When the value is a struct
/// whose type has a registered `summary` template, the rendered
/// summary appears in the `value` field of the DAP `Variable`
/// instead of the placeholder `{...}`. Children continue to be
/// served via the normal `variables` request walk; that walk
/// also threads the registry, so nested structs render the same
/// way.
pub fn render_value_to_string_with_viz(
    v: &debugger::variable::value::Value,
    viz: Option<&debugger::viz::VizRegistry>,
) -> String {
    use debugger::variable::render::RenderValue;
    match v.value_layout() {
        Some(debugger::variable::render::ValueLayout::PreRendered(s)) => s.to_string(),
        Some(debugger::variable::render::ValueLayout::Referential(ptr)) => {
            format!("{ptr:p}")
        }
        Some(debugger::variable::render::ValueLayout::Wrapped(inner)) => {
            // Phase 4 steps 6 + 8 — enum summary template. Step
            // 8 prefers a variant-level summary over the type-
            // level one, applies variant-scoped field overrides,
            // and prepends a `[tag]` chip when the variant
            // carries a `tag = "..."` attribute.
            if let debugger::variable::value::Value::RustEnum(re) = v
                && let Some(spec) = viz.and_then(|r| {
                    let outer = RenderValue::r#type(v).name_fmt();
                    r.find(outer)
                })
            {
                let active_variant_name = re.value.as_ref().and_then(|m| m.field_name.as_deref());
                let variant_spec = active_variant_name
                    .and_then(|n| spec.variants.iter().find(|var| var.name == n));
                let tmpl = variant_spec
                    .and_then(|v| v.summary.as_deref())
                    .or(spec.summary.as_deref());
                if let (Some(tmpl), debugger::variable::value::Value::Struct(variant)) =
                    (tmpl, inner)
                {
                    let outer_type = RenderValue::r#type(v).name_fmt();
                    let fields_for_lookup: &[bs_viz_spec::FieldSpec] = match variant_spec {
                        Some(vs) => &vs.fields,
                        None => &spec.fields,
                    };
                    let body = debugger::viz::substitute_template(tmpl, &variant.members, |m| {
                        let fmt = m
                            .field_name
                            .as_deref()
                            .and_then(|name| fields_for_lookup.iter().find(|f| f.name == name))
                            .map(|f| f.format)
                            .filter(|f| *f != bs_viz_spec::Format::Default);
                        if let Some(fmt) = fmt {
                            if let Some(s) =
                                crate::ui::generic::variable::format_scalar_for_dap(&m.value, fmt)
                            {
                                return s;
                            }
                            if let Some(s) =
                                crate::ui::generic::variable::format_bytes_for_dap(&m.value, fmt)
                            {
                                return s;
                            }
                        }
                        if let Some(raw) =
                            debugger::variable::render::unwrap_string_for_template(&m.value)
                        {
                            return raw;
                        }
                        render_value_to_string_with_viz(&m.value, viz)
                    });
                    let tag_prefix = variant_spec
                        .and_then(|vs| vs.tag.as_deref())
                        .map(|t| format!(" [{t}]"))
                        .unwrap_or_default();
                    return format!("{outer_type}{tag_prefix} {body}");
                }
            }
            // Wrapped layouts come from two sources:
            //   - Rust enum variants (`v` is `Value::RustEnum`,
            //     `inner` is the variant's struct data).
            //   - Eagerly-deref'd pointers (`v` is `Value::Pointer`,
            //     `inner` is the pointee). References, raw pointers,
            //     and Box<T> all go through this path now.
            //
            // Render each in the shape the user would expect to see
            // in Rust source.
            // Rc/Arc were briefly included in this gate but the
            // accompanying payload-peel triggered a debug-session
            // crash on showcase. Reverted to Pointer-only. Rc/Arc
            // fall through to the enum-variant logic, which renders
            // them as `RcInner<T> {...}` — uglier but stable.
            let is_pointer_like = matches!(v, debugger::variable::value::Value::Pointer(_));
            if is_pointer_like {
                // Count how many layers of indirection we had to peel
                // through to reach a non-pointer value, and render as
                // `*N final_value` (or `*final` / `**final` / `***final`
                // for small N) — the count makes the indirection depth
                // visible at a glance without forcing the reader to
                // mentally parse `Box<&Box<&i32>>` or `Rc<Box<T>>` from
                // the type column.
                //
                // The KIND of indirection (Box vs `&` vs `*const` vs
                // Rc/Arc) is still in the type column for cases where it
                // matters; the value column's job is "what's behind the
                // pointers, and how deep".
                let (depth, leaf) = count_eager_derefs(v);
                let body = render_value_to_string_with_viz(leaf, viz);
                if depth == 0 {
                    return body;
                }
                return format!("{} {body}", render_deref_prefix(depth));
            }
            // Rust enum variants: render the way Rust source spells them:
            //   `None`               for empty variants
            //   `Some(42)`           for tuple variants
            //   `Foo { x: 1, y: 2 }` for struct variants
            // The old `Variant::body` form (e.g. `None::{...}`,
            // `Some::(42)`) wasn't valid Rust syntax and made the
            // Variables panel read strangely.
            let variant_type = RenderValue::r#type(inner).name_fmt();
            if let debugger::variable::value::Value::Struct(s) = inner {
                if s.members.is_empty() {
                    return variant_type.to_string();
                }
                let body = render_value_to_string_with_viz(inner, viz);
                if is_tuple_structure(&s.members) {
                    // Tuple variants: body already starts with `(`
                    // courtesy of the tuple branch of Structure
                    // rendering. Concatenate directly.
                    return format!("{variant_type}{body}");
                }
                // Struct variants: separate the type from the
                // `{ … }` body with a space, matching Rust's
                // `Variant { field: value }` source form.
                return format!("{variant_type} {body}");
            }
            format!(
                "{variant_type}({})",
                render_value_to_string_with_viz(inner, viz)
            )
        }
        Some(debugger::variable::render::ValueLayout::Structure(members)) => {
            let type_name = RenderValue::r#type(v).name_fmt();
            if let Some(spec) = viz.and_then(|r| r.find(type_name))
                && let Some(tmpl) = spec.summary.as_deref()
            {
                return debugger::viz::substitute_template(tmpl, members, |m| {
                    // Honour per-field format overrides
                    // inside the template too, mirroring the
                    // TUI/console path. The DAP renderer
                    // doesn't have a `format_scalar` of its
                    // own — for now reach into the TUI helper
                    // via a thin re-export, since the formats
                    // are pure functions of (Value, Format).
                    let fmt = m
                        .field_name
                        .as_deref()
                        .and_then(|name| spec.fields.iter().find(|f| f.name == name))
                        .map(|f| f.format)
                        .filter(|f| *f != bs_viz_spec::Format::Default);
                    if let Some(fmt) = fmt {
                        if let Some(s) =
                            crate::ui::generic::variable::format_scalar_for_dap(&m.value, fmt)
                        {
                            return s;
                        }
                        if let Some(s) =
                            crate::ui::generic::variable::format_bytes_for_dap(&m.value, fmt)
                        {
                            return s;
                        }
                    }
                    if let Some(raw) =
                        debugger::variable::render::unwrap_string_for_template(&m.value)
                    {
                        return raw;
                    }
                    render_value_to_string_with_viz(&m.value, viz)
                });
            }
            if is_tuple_structure(members) {
                // Render tuples inline with the bracket shape Rust
                // source uses: `(1, "one")` rather than `{...}`. Cap
                // the joined string to avoid blowing the Variables
                // panel up on deeply-nested tuples; the caller can
                // still expand into the child entries for the full
                // view.
                let rendered: Vec<String> = members
                    .iter()
                    .map(|m| render_value_to_string_with_viz(&m.value, viz))
                    .collect();
                let joined = rendered.join(", ");
                const MAX_INLINE_LEN: usize = 120;
                if joined.len() <= MAX_INLINE_LEN {
                    let bare_tuple = type_name.starts_with('(');
                    return wrap_tuple_body(&joined, members.len(), bare_tuple);
                }
                return "(…)".to_string();
            }
            // Unit struct (`struct Marker;`) — zero fields. The `…`
            // in `{...}` implies hidden content; `{}` matches Rust's
            // explicit empty-struct literal (`Marker {}`) and reads
            // honestly. The expand-arrow is also suppressed at the
            // `variablesReference` level above so the user doesn't
            // see a triangle that opens onto nothing.
            if members.is_empty() {
                return "{}".to_string();
            }
            "{...}".to_string()
        }
        Some(debugger::variable::render::ValueLayout::IndexedList(items)) => {
            // For fixed-size arrays the length is in the type
            // (`[T; N]`) — adding `(len=N)` to the value would
            // duplicate it. For everything else (slices, Vec items
            // when surfaced as IndexedList) keep the prefix; the
            // type doesn't carry the length there.
            let array_shaped = matches!(v, debugger::variable::value::Value::Array(_));
            if array_shaped {
                indexed_list_inline(items, viz)
            } else {
                indexed_list_preview(items, viz)
            }
        }
        Some(debugger::variable::render::ValueLayout::NonIndexedList(items)) => {
            // HashSet / BTreeSet: same `len=` treatment, just no
            // positional indices on the items themselves.
            non_indexed_list_preview(items, viz)
        }
        Some(debugger::variable::render::ValueLayout::Map(entries)) => {
            // HashMap / BTreeMap likewise benefit from a `len=N` hint
            // so the user knows the map size before expanding.
            map_preview(entries, viz)
        }
        None => "<unavailable>".to_string(),
    }
}

fn value_write_meta(
    v: &debugger::variable::value::Value,
    type_graph: Rc<debugger::ComplexType>,
) -> Option<WriteMeta> {
    use debugger::variable::value::{SupportedScalar, Value as BsValue};

    let addr = v.in_memory_location()?;
    match v {
        BsValue::Scalar(s) => match s.value.as_ref()? {
            SupportedScalar::I8(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::I8,
            }),
            SupportedScalar::I16(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::I16,
            }),
            SupportedScalar::I32(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::I32,
            }),
            SupportedScalar::I64(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::I64,
            }),
            SupportedScalar::I128(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::I128,
            }),
            SupportedScalar::Isize(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::Isize,
            }),
            SupportedScalar::U8(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::U8,
            }),
            SupportedScalar::U16(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::U16,
            }),
            SupportedScalar::U32(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::U32,
            }),
            SupportedScalar::U64(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::U64,
            }),
            SupportedScalar::U128(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::U128,
            }),
            SupportedScalar::Usize(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::Usize,
            }),
            SupportedScalar::F32(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::F32,
            }),
            SupportedScalar::F64(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::F64,
            }),
            SupportedScalar::Bool(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::Bool,
            }),
            SupportedScalar::Char(_) => Some(WriteMeta::Scalar {
                addr,
                kind: ScalarKind::Char,
            }),
            SupportedScalar::Empty() => None,
        },
        _ => v
            .type_id()
            .map(|_| WriteMeta::Composite { addr, type_graph }),
    }
}

fn parse_set_value(kind: ScalarKind, input: &str) -> anyhow::Result<Vec<u8>> {
    let s = input.trim();

    fn parse_int_i128(s: &str) -> anyhow::Result<i128> {
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i128::from_str_radix(hex, 16).context("hex i128 parse")
        } else {
            s.parse::<i128>().context("dec i128 parse")
        }
    }

    fn parse_int_u128(s: &str) -> anyhow::Result<u128> {
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u128::from_str_radix(hex, 16).context("hex u128 parse")
        } else {
            s.parse::<u128>().context("dec u128 parse")
        }
    }

    match kind {
        ScalarKind::I8 => Ok(vec![(parse_int_i128(s)? as i8) as u8]),
        ScalarKind::U8 => Ok(vec![parse_int_u128(s)? as u8]),
        ScalarKind::I16 => Ok((parse_int_i128(s)? as i16).to_le_bytes().to_vec()),
        ScalarKind::U16 => Ok((parse_int_u128(s)? as u16).to_le_bytes().to_vec()),
        ScalarKind::I32 => Ok((parse_int_i128(s)? as i32).to_le_bytes().to_vec()),
        ScalarKind::U32 => Ok((parse_int_u128(s)? as u32).to_le_bytes().to_vec()),
        ScalarKind::I64 => Ok((parse_int_i128(s)? as i64).to_le_bytes().to_vec()),
        ScalarKind::U64 => Ok((parse_int_u128(s)? as u64).to_le_bytes().to_vec()),
        ScalarKind::I128 => Ok(parse_int_i128(s)?.to_le_bytes().to_vec()),
        ScalarKind::U128 => Ok(parse_int_u128(s)?.to_le_bytes().to_vec()),
        ScalarKind::Isize => Ok((parse_int_i128(s)? as isize).to_le_bytes().to_vec()),
        ScalarKind::Usize => Ok((parse_int_u128(s)? as usize).to_le_bytes().to_vec()),
        ScalarKind::F32 => Ok(s
            .parse::<f32>()
            .context("f32 parse")?
            .to_le_bytes()
            .to_vec()),
        ScalarKind::F64 => Ok(s
            .parse::<f64>()
            .context("f64 parse")?
            .to_le_bytes()
            .to_vec()),
        ScalarKind::Bool => {
            let b = match s {
                "true" | "True" | "TRUE" => true,
                "false" | "False" | "FALSE" => false,
                "1" => true,
                "0" => false,
                _ => anyhow::bail!("bool parse: expected true/false/0/1, got '{s}'"),
            };
            Ok(vec![if b { 1 } else { 0 }])
        }
        ScalarKind::Char => {
            // Accept: 'a' or 97 or 0x61. Stored as Rust char (u32).
            if let Some(stripped) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
                let mut it = stripped.chars();
                let ch = it.next().context("char parse: empty literal")?;
                if it.next().is_some() {
                    anyhow::bail!("char parse: expected single char literal");
                }
                let u = ch as u32;
                Ok(u.to_le_bytes().to_vec())
            } else if s.chars().count() == 1 {
                let u = s.chars().next().unwrap() as u32;
                Ok(u.to_le_bytes().to_vec())
            } else {
                let u = parse_int_u128(s)? as u32;
                Ok(u.to_le_bytes().to_vec())
            }
        }
    }
}

fn value_children(
    qr: &debugger::variable::execute::QueryResult,
    type_graph: Rc<debugger::ComplexType>,
    viz: Option<&debugger::viz::VizRegistry>,
) -> Option<Vec<VarItem>> {
    use debugger::variable::render::{RenderValue, ValueLayout};
    let layout = qr.value().value_layout()?;
    match layout {
        ValueLayout::Structure(members) => {
            let mut out = Vec::new();
            let tuple = is_tuple_structure(members);
            for (idx, m) in members.iter().enumerate() {
                // Rust source spells tuple field access as `.0`, `.1`,
                // ... — but DWARF stores those members as named fields
                // `__0`, `__1`, ... (the convention rustc emits for
                // tuple-struct lowering). Rewrite to match the source
                // syntax so the Variables panel reads like Rust code,
                // not like DWARF.
                let field_name = if tuple {
                    format!(".{idx}")
                } else {
                    m.field_name.as_deref().unwrap_or("<unnamed>").to_string()
                };
                let qr = qr
                    .clone()
                    .modify_value(|_, _| Some(m.value.clone()))
                    .expect("should be `Some`");

                out.push(VarItem {
                    name: field_name,
                    value: render_value_to_string_with_viz(qr.value(), viz),
                    type_name: Some(qr.value().r#type().to_string()),
                    child: value_children(&qr, type_graph.clone(), viz),
                    write: value_write_meta(qr.value(), type_graph.clone()),
                    source: Some(qr.value().clone()),
                    // Variables-view §5.2 / §5.3: child rows skip
                    // mutability / storage / heap hints in v0 —
                    // top-level row already shows them. Per-field
                    // inheritance from parent is a documented §7
                    // follow-up.
                    mutability: None,
                    storage: None,
                    points_to_heap: false,
                    byte_size: None,
                    layout: None,
                });
            }
            Some(out)
        }
        ValueLayout::IndexedList(items) => {
            let mut out = Vec::new();
            for it in items {
                let qr = qr
                    .clone()
                    .modify_value(|_, _| Some(it.value.clone()))
                    .expect("should be `Some`");

                out.push(VarItem {
                    name: format!("[{}]", it.index),
                    value: render_value_to_string_with_viz(qr.value(), viz),
                    type_name: Some(qr.value().r#type().to_string()),
                    child: value_children(&qr, type_graph.clone(), viz),
                    write: value_write_meta(qr.value(), type_graph.clone()),
                    source: Some(qr.value().clone()),
                    mutability: None,
                    storage: None,
                    points_to_heap: false,
                    byte_size: None,
                    layout: None,
                });
            }
            Some(out)
        }
        ValueLayout::NonIndexedList(items) => {
            let mut out = Vec::new();
            for (i, val) in items.iter().enumerate() {
                let qr = qr
                    .clone()
                    .modify_value(|_, _| Some(val.clone()))
                    .expect("should be `Some`");

                out.push(VarItem {
                    name: format!("[{i}]"),
                    value: render_value_to_string_with_viz(qr.value(), viz),
                    type_name: Some(qr.value().r#type().to_string()),
                    child: value_children(&qr, type_graph.clone(), viz),
                    write: value_write_meta(qr.value(), type_graph.clone()),
                    source: Some(qr.value().clone()),
                    mutability: None,
                    storage: None,
                    points_to_heap: false,
                    byte_size: None,
                    layout: None,
                });
            }
            Some(out)
        }
        ValueLayout::Map(kvs) => {
            let mut out = Vec::new();
            for (i, (k, val)) in kvs.iter().enumerate() {
                let cell_qr = qr.clone().modify_value(|_, _| Some(val.clone()));

                out.push(VarItem {
                    name: format!("[{i}]"),
                    value: format!(
                        "{} => {}",
                        render_value_to_string_with_viz(k, viz),
                        render_value_to_string_with_viz(val, viz)
                    ),
                    type_name: None,
                    child: cell_qr
                        .as_ref()
                        .and_then(|qr| value_children(qr, type_graph.clone(), viz)),
                    write: None,
                    source: cell_qr.map(|qr| qr.value().clone()),
                    mutability: None,
                    storage: None,
                    points_to_heap: false,
                    byte_size: None,
                    layout: None,
                });
            }
            Some(out)
        }
        ValueLayout::Wrapped(v) => {
            let qr = qr
                .clone()
                .modify_value(|_, _| Some(v.clone()))
                .expect("should be `Some`");

            value_children(&qr, type_graph, viz)
        }
        ValueLayout::Referential(_r) => {
            let qr = qr.clone().modify_value(|pcx, v| v.deref(pcx));

            if let Some(deref_qr) = qr {
                let out = vec![VarItem {
                    name: "deref".to_string(),
                    value: render_value_to_string_with_viz(deref_qr.value(), viz),
                    type_name: Some(deref_qr.value().r#type().to_string()),
                    child: value_children(&deref_qr, type_graph.clone(), viz),
                    write: value_write_meta(deref_qr.value(), type_graph.clone()),
                    source: Some(deref_qr.value().clone()),
                    mutability: None,
                    storage: None,
                    points_to_heap: false,
                    byte_size: None,
                    layout: None,
                }];
                Some(out)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn read_locals(dbg: &debugger::Debugger) -> anyhow::Result<Vec<VarItem>> {
    use debugger::variable::render::RenderValue;
    let locals = dbg.read_local_variables()?;
    let viz = Some(dbg.view_registry());
    let mut out = Vec::new();
    for r in locals {
        let type_graph = Rc::new(r.type_graph().clone());
        let name = r.identity().to_string();
        let mutability = mutability_hint(&r, dbg);
        let storage = storage_hint(&r);
        let points_to_heap = points_to_heap_hint(&r, dbg);
        let byte_size = r.byte_size();
        let layout = r.layout();
        out.push(VarItem {
            name,
            value: render_value_to_string_with_viz(r.value(), viz),
            type_name: Some(r.value().r#type().to_string()),
            child: value_children(&r, type_graph.clone(), viz),
            write: value_write_meta(r.value(), type_graph.clone()),
            source: Some(r.value().clone()),
            mutability,
            storage,
            points_to_heap,
            byte_size,
            layout,
        });
    }
    Ok(out)
}

/// Variables-view §5.2: classify the variable's mutability and
/// stringify it for the DAP custom field. Returns `None` when the
/// classifier reports `Unknown` so we omit the field rather than
/// emit an unhelpful `"unknown"` in the JSON.
fn mutability_hint(
    qr: &debugger::variable::execute::QueryResult<'_>,
    dbg: &debugger::Debugger,
) -> Option<&'static str> {
    let m = debugger::variable::mutability::classify(qr, dbg);
    match m {
        debugger::variable::mutability::Mutability::Unknown => None,
        other => Some(other.as_dap_str()),
    }
}

/// Variables-view §5.3: stringify the storage class from a
/// QueryResult. Returns the result already cached on the
/// QueryResult by `DqeExecutor::root_from_die` — we don't redo
/// the DWARF walk here.
fn storage_hint(qr: &debugger::variable::execute::QueryResult<'_>) -> Option<&'static str> {
    qr.storage().map(|s| s.as_dap_str())
}

/// Variables-view §5.3 heap overlay — `true` when the variable's
/// value points into a heap-ish mapping (`[heap]` or anon RW).
fn points_to_heap_hint(
    qr: &debugger::variable::execute::QueryResult<'_>,
    dbg: &debugger::Debugger,
) -> bool {
    debugger::variable::storage::value_points_to_heap(qr.value(), dbg)
}

pub fn read_args(dbg: &debugger::Debugger) -> anyhow::Result<Vec<VarItem>> {
    use debugger::variable::dqe::{Dqe, Selector};
    use debugger::variable::render::RenderValue;
    let args = dbg.read_argument(Dqe::Variable(Selector::Any))?;
    let viz = Some(dbg.view_registry());
    let mut out = Vec::new();
    for r in args {
        let type_graph = Rc::new(r.type_graph().clone());
        let name = r.identity().to_string();
        let mutability = mutability_hint(&r, dbg);
        let storage = storage_hint(&r);
        let points_to_heap = points_to_heap_hint(&r, dbg);
        let byte_size = r.byte_size();
        let layout = r.layout();
        out.push(VarItem {
            name,
            value: render_value_to_string_with_viz(r.value(), viz),
            type_name: Some(r.value().r#type().to_string()),
            child: value_children(&r, type_graph.clone(), viz),
            write: value_write_meta(r.value(), type_graph.clone()),
            source: Some(r.value().clone()),
            mutability,
            storage,
            points_to_heap,
            byte_size,
            layout,
        });
    }
    Ok(out)
}

/// Variables-view §5.4 — populate the `Statics` DAP scope.
/// Defaults to the user's crate (filtering out std / dep statics)
/// so the pane stays useful.
pub fn read_statics(dbg: &debugger::Debugger) -> anyhow::Result<Vec<VarItem>> {
    file_scope_var_items(
        dbg,
        debugger::variable::execute::FileScopeKind::Statics,
        debugger::variable::execute::FileScopeFilter::CurrentCrate,
    )
}

/// Variables-view §5.4 — populate the `Thread-locals` DAP scope.
/// Same filter rationale as [`read_statics`].
pub fn read_thread_locals(dbg: &debugger::Debugger) -> anyhow::Result<Vec<VarItem>> {
    file_scope_var_items(
        dbg,
        debugger::variable::execute::FileScopeKind::ThreadLocals,
        debugger::variable::execute::FileScopeFilter::CurrentCrate,
    )
}

fn file_scope_var_items(
    dbg: &debugger::Debugger,
    kind: debugger::variable::execute::FileScopeKind,
    filter: debugger::variable::execute::FileScopeFilter,
) -> anyhow::Result<Vec<VarItem>> {
    use debugger::variable::execute::FileScopeKind;
    use debugger::variable::render::RenderValue;
    let entries = match kind {
        FileScopeKind::Statics => dbg.read_static_variables(filter)?,
        FileScopeKind::ThreadLocals => dbg.read_thread_local_variables(filter)?,
    };
    let viz = Some(dbg.view_registry());
    let mut out = Vec::new();
    for r in entries {
        let type_graph = Rc::new(r.type_graph().clone());
        let name = r.identity().to_string();
        let mutability = mutability_hint(&r, dbg);
        let storage = storage_hint(&r);
        let points_to_heap = points_to_heap_hint(&r, dbg);
        let byte_size = r.byte_size();
        let layout = r.layout();
        out.push(VarItem {
            name,
            value: render_value_to_string_with_viz(r.value(), viz),
            type_name: Some(r.value().r#type().to_string()),
            child: value_children(&r, type_graph.clone(), viz),
            write: value_write_meta(r.value(), type_graph.clone()),
            source: Some(r.value().clone()),
            mutability,
            storage,
            points_to_heap,
            byte_size,
            layout,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tuple_rendering_tests {
    use super::*;

    #[test]
    fn empty_struct_is_not_a_tuple() {
        assert!(!is_tuple_field_set(std::iter::empty()));
    }

    #[test]
    fn bare_one_tuple_gets_trailing_comma() {
        // A 1-tuple must read as `("a",)`, not `("a")`, so it's
        // clearly a tuple and not a parenthesised value.
        assert_eq!(wrap_tuple_body("\"a\"", 1, true), "(\"a\",)");
    }

    #[test]
    fn bare_multi_tuple_has_no_trailing_comma() {
        assert_eq!(wrap_tuple_body("1, \"one\"", 2, true), "(1, \"one\")");
    }

    #[test]
    fn one_field_tuple_struct_or_variant_has_no_comma() {
        // `bare_tuple == false` for tuple structs / enum variants
        // (`Wrap(x)`, `Ok(x)`) — the prefix is added by the caller and
        // Rust writes those without a trailing comma.
        assert_eq!(wrap_tuple_body("7", 1, false), "(7)");
    }

    #[test]
    fn two_field_underscore_underscore_indices_is_a_tuple() {
        assert!(is_tuple_field_set([Some("__0"), Some("__1")]));
    }

    #[test]
    fn five_field_in_order_is_a_tuple() {
        assert!(is_tuple_field_set([
            Some("__0"),
            Some("__1"),
            Some("__2"),
            Some("__3"),
            Some("__4"),
        ]));
    }

    #[test]
    fn out_of_order_indices_are_not_a_tuple() {
        // A struct with members literally named __1 then __0 is
        // not a Rust tuple — we'd be wrong to relabel them as
        // `.0`, `.1` in source order.
        assert!(!is_tuple_field_set([Some("__1"), Some("__0")]));
    }

    #[test]
    fn struct_with_named_fields_is_not_a_tuple() {
        assert!(!is_tuple_field_set([Some("x"), Some("y")]));
    }

    #[test]
    fn mixed_named_and_indexed_fields_are_not_a_tuple() {
        assert!(!is_tuple_field_set([Some("__0"), Some("name")]));
    }

    #[test]
    fn missing_field_name_disqualifies() {
        assert!(!is_tuple_field_set([Some("__0"), None]));
    }

    #[test]
    fn single_field_underscore_underscore_zero_is_a_tuple() {
        assert!(is_tuple_field_set([Some("__0")]));
    }
}

#[cfg(test)]
mod collection_preview_tests {
    use super::*;

    /// `non_indexed_list_preview` is the only collection helper
    /// whose input shape (`&[Value]`) we can construct without
    /// pulling in TypeIdentity, so we keep its tests here. The
    /// indexed-list and map helpers share the same length-budget
    /// machinery; a smoke check on this one guards the format.
    #[test]
    fn non_indexed_collapses_when_over_budget() {
        // Build a NonIndexedList payload long enough to exceed
        // COLLECTION_PREVIEW_BUDGET. The renderer should fall back
        // to `{...} (len=N)` rather than emit the whole thing.
        let big_strings: Vec<debugger::variable::value::Value> = (0..50)
            .map(|i| debugger::variable::value::Value::Specialized {
                value: Some(debugger::variable::value::SpecializedValue::Str(
                    debugger::variable::value::specialization::StrVariable {
                        value: format!("item-{i:03}"),
                        elided: None,
                    },
                )),
                original: debugger::variable::value::StructValue::default(),
            })
            .collect();
        let rendered = non_indexed_list_preview(&big_strings, None);
        assert!(
            rendered.ends_with("(len=50)"),
            "preview must end with the length tag, got: {rendered}"
        );
        assert!(
            rendered.starts_with("{...}"),
            "over-budget preview must collapse to {{...}}, got: {rendered}"
        );
    }

    #[test]
    fn non_indexed_inlines_when_short() {
        // An empty NonIndexedList is the lowest-budget case and
        // exercises the formatting path without needing to
        // synthesise items.
        let rendered = non_indexed_list_preview(&[], None);
        assert_eq!(rendered, "{} (len=0)");
    }
}

// `strip_type_namespace_tests` moved to
// `debugger::variable::render` alongside the function itself.

#[cfg(test)]
mod deref_prefix_tests {
    use super::render_deref_prefix;

    #[test]
    fn zero_depth_renders_empty() {
        assert_eq!(render_deref_prefix(0), "");
    }

    #[test]
    fn single_deref_is_one_star() {
        assert_eq!(render_deref_prefix(1), "*");
    }

    #[test]
    fn small_counts_use_repeated_stars() {
        assert_eq!(render_deref_prefix(2), "**");
        assert_eq!(render_deref_prefix(3), "***");
        assert_eq!(render_deref_prefix(5), "*****");
    }

    #[test]
    fn large_counts_collapse_to_braced_form() {
        // 6+ collapses to `*{N}` so the prefix doesn't grow
        // comically. Tested at the boundary and one over it.
        assert_eq!(render_deref_prefix(6), "*{6}");
        assert_eq!(render_deref_prefix(42), "*{42}");
    }
}

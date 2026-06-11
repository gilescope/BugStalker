// SPDX-License-Identifier: MIT
use crate::dap::yadap::protocol::DapRequest;
use crate::dap::yadap::sourcemap::SourceMap;
use crate::debugger;
use anyhow::{Context, anyhow};
use capstone::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};

#[cfg(target_arch = "x86_64")]
fn new_capstone() -> Result<Capstone, capstone::Error> {
    Capstone::new()
        .x86()
        .mode(arch::x86::ArchMode::Mode64)
        .syntax(arch::x86::ArchSyntax::Att)
        .build()
}

#[cfg(target_arch = "aarch64")]
fn new_capstone() -> Result<Capstone, capstone::Error> {
    Capstone::new()
        .arm64()
        .mode(arch::arm64::ArchMode::Arm)
        .build()
}

#[derive(Debug, Clone)]
pub struct DisasmSource {
    pub reference: i64,
    pub name: String,
    pub content: String,
}

pub struct DisasmInstruction {
    pub address: u64,
    pub bytes_hex: String,
    pub text: String,
}

impl super::DebugSession {
    pub(super) fn handle_disassemble(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        if self.consume_cancellation(req, None)? {
            return Ok(());
        }
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("disassemble: arguments must be object"))?;

        let memory_reference = args
            .get("memoryReference")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("disassemble: missing arguments.memoryReference"))?;

        let instruction_count = args
            .get("instructionCount")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow!("disassemble: missing arguments.instructionCount"))?;
        if instruction_count <= 0 {
            return self.send_err(
                req,
                "disassemble: instructionCount must be positive".to_string(),
            );
        }

        let offset = args
            .get("offset")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let instruction_offset = args
            .get("instructionOffset")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let base_addr = super::parse_memory_reference(memory_reference)
            .context("disassemble: invalid memoryReference")?;
        let start = offset
            .checked_add(base_addr as i64)
            .ok_or_else(|| anyhow!("disassemble: address overflow"))?;
        if start < 0 {
            return self.send_err(req, "disassemble: start address is negative".to_string());
        }
        let anchor_addr = start as usize;
        let back_instructions = instruction_offset.unsigned_abs() as usize;
        let max_len = 16usize;
        let start_addr = anchor_addr.saturating_sub(back_instructions.saturating_mul(max_len));
        let disasm_count = instruction_count as usize + back_instructions + 16;
        let progress_id = self.enqueue_progress_start(
            "Disassembling",
            Some(format!("Reading {disasm_count} instruction(s)")),
            Some(0),
        );
        self.drain_events()?;
        if self.consume_cancellation(req, Some(&progress_id))? {
            return Ok(());
        }
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("disassemble: debugger not initialized"))?;
        let start = Instant::now();
        let instructions = match disassemble_from_address(
            dbg,
            start_addr,
            disasm_count,
            super::MEMORY_READ_TIMEOUT,
        ) {
            Ok(instructions) => instructions,
            Err(err) => {
                self.enqueue_progress_end(
                    progress_id.clone(),
                    Some("Disassembly failed".to_string()),
                );
                self.drain_events()?;
                return self.send_err(req, format!("disassemble: {err}"));
            }
        };
        let elapsed = start.elapsed();
        if elapsed > super::DEBUGGER_RESPONSE_TIMEOUT {
            self.enqueue_progress_end(
                progress_id.clone(),
                Some("Disassembly timed out".to_string()),
            );
            self.drain_events()?;
            return self.send_err(
                req,
                format!(
                    "disassemble: debugger response timed out after {}ms",
                    super::DEBUGGER_RESPONSE_TIMEOUT.as_millis()
                ),
            );
        }
        if self.consume_cancellation(req, Some(&progress_id))? {
            return Ok(());
        }
        let anchor_index = instructions
            .iter()
            .position(|ins| ins.address as usize >= anchor_addr)
            .unwrap_or(instructions.len());
        let start_index = if instruction_offset >= 0 {
            anchor_index.saturating_add(instruction_offset as usize)
        } else {
            anchor_index.saturating_sub(back_instructions)
        };

        // Interleave source: resolve each instruction's address to (file, line)
        // and attach a `location`+`line` to the *first* instruction of each
        // source-line run (DAP carries it forward to the rest) — this is what
        // makes VS Code show Rust on the left, asm on the right.
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("disassemble: debugger not initialized"))?;
        let mut prev: Option<(String, u64)> = None;
        let mut instructions = instructions
            .into_iter()
            .skip(start_index)
            .take(instruction_count as usize)
            .map(|ins| {
                let mut obj = json!({
                    "address": format!("0x{:x}", ins.address),
                    "instructionBytes": ins.bytes_hex,
                    "instruction": ins.text,
                });
                if let Some((file, line, column)) = dbg.source_location_at(ins.address as usize) {
                    let client = self
                        .source_map
                        .map_target_to_client(&file.to_string_lossy());
                    if prev.as_ref() != Some(&(client.clone(), line)) {
                        let name = file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| client.clone());
                        obj["location"] = json!({ "path": client, "name": name });
                        obj["line"] = json!(line);
                        obj["column"] = json!(column);
                        prev = Some((client, line));
                    }
                }
                obj
            })
            .collect::<Vec<_>>();

        // DAP contract: return EXACTLY instructionCount entries. On x86-64 a
        // context-before request starts disassembling `back × max_len` bytes
        // early, and capstone can desync on the variable-length stream and
        // stop short — arm64's fixed 4-byte instructions always fill the
        // budget, which is why this only shows up on x86 CI. Pad the tail
        // with explicit invalid entries (VS Code renders them greyed) rather
        // than under-deliver, which desyncs the Disassembly View's scrolling.
        let mut next_addr = instructions
            .last()
            .and_then(|i| i["address"].as_str())
            .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
            .map_or(anchor_addr as u64, |a| a + 1);
        while instructions.len() < instruction_count as usize {
            instructions.push(json!({
                "address": format!("0x{next_addr:x}"),
                "instruction": "??",
                "instructionBytes": "",
            }));
            next_addr += 1;
        }

        self.enqueue_progress_update(
            progress_id.clone(),
            Some(format!("Prepared {} instruction(s)", instructions.len())),
            Some(100),
        );
        self.enqueue_progress_end(progress_id, Some("Disassembly complete".to_string()));
        self.drain_events()?;
        self.send_success_body(req, json!({ "instructions": instructions }))
    }

    /// `bs/currentFunctionName` — returns the demangled name of the function
    /// containing the current PC, for display in the Source+ASM view header.
    pub(super) fn handle_current_function_name(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let Some(dbg) = self.debugger.as_ref() else {
            return self.send_err(req, "bs/currentFunctionName: no active session");
        };
        let name = dbg.current_function_name().unwrap_or_default();
        self.send_success_body(req, json!({ "name": name }))
    }

    /// `bs/registers` — GPR values for the focus thread at the current stop,
    /// as a `{ name: "0x…" }` map. The Source+ASM view uses these to show an
    /// instruction's operand registers' live values in its hover tooltip.
    pub(super) fn handle_registers(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let Some(dbg) = self.debugger.as_ref() else {
            return self.send_err(req, "bs/registers: no active session");
        };
        let map: serde_json::Map<String, serde_json::Value> = dbg
            .current_registers()
            .into_iter()
            .map(|(name, value)| (name.to_string(), json!(format!("0x{value:x}"))))
            .collect();
        self.send_success_body(req, json!({ "registers": map }))
    }

    /// `bs/setAsmFocus` — notified by the extension when the Source+ASM webview
    /// panel gains or loses focus. While focused, `handle_next`/`handle_step_in`
    /// treat every step as `granularity: "instruction"` so F10/F11 step one
    /// machine instruction without any keybinding overrides.
    pub(super) fn handle_set_asm_focus(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let focused = req
            .arguments
            .get("focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.asm_view_focused = focused;
        self.send_success(req)
    }

    /// `bs/functionBounds` — returns the relocated start/end addresses of the
    /// function containing the current PC. The extension uses this to request a
    /// full-function disassembly rather than a fixed instruction window.
    pub(super) fn handle_function_bounds(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let Some(dbg) = self.debugger.as_ref() else {
            return self.send_err(req, "bs/functionBounds: no active session");
        };
        match dbg.current_function_address_range() {
            Some((start, end)) => self.send_success_body(
                req,
                json!({
                    "startAddress": format!("0x{start:x}"),
                    "endAddress":   format!("0x{end:x}"),
                }),
            ),
            None => self.send_success_body(req, json!({ "unavailable": true })),
        }
    }

    pub fn disasm_source_for_address(
        &mut self,
        req: &DapRequest,
        addr: usize,
    ) -> anyhow::Result<Option<DisasmSource>> {
        if let Some(existing) = self.disasm_cache_by_addr.get(&addr) {
            return Ok(Some(existing.clone()));
        }

        let progress_id = self.enqueue_progress_start(
            "Disassembling",
            Some(format!("Generating source for 0x{addr:x}")),
            Some(0),
        );
        self.drain_events()?;
        if self.consume_cancellation(req, Some(&progress_id))? {
            return Ok(None);
        }
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("disassemble: debugger not initialized"))?;
        let start = Instant::now();
        let instructions = match disassemble_from_address(dbg, addr, 64, super::MEMORY_READ_TIMEOUT)
        {
            Ok(instructions) => instructions,
            Err(err) => {
                self.enqueue_progress_end(
                    progress_id.clone(),
                    Some("Disassembly failed".to_string()),
                );
                self.drain_events()?;
                return Err(err);
            }
        };
        let elapsed = start.elapsed();
        if elapsed > super::DEBUGGER_RESPONSE_TIMEOUT {
            self.enqueue_progress_end(
                progress_id.clone(),
                Some("Disassembly timed out".to_string()),
            );
            self.drain_events()?;
            anyhow::bail!(
                "disassemble: debugger response timed out after {}ms",
                super::DEBUGGER_RESPONSE_TIMEOUT.as_millis()
            );
        }
        if self.consume_cancellation(req, Some(&progress_id))? {
            return Ok(None);
        }
        let content = if instructions.is_empty() {
            format!("No disassembly available at 0x{addr:x}.")
        } else {
            instructions
                .iter()
                .map(|ins| format!("0x{:x}: {}", ins.address, ins.text))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let reference = self.next_source_reference;
        self.next_source_reference += 1;
        let name = format!("disasm @ 0x{addr:x}");
        let entry = DisasmSource {
            reference,
            name,
            content,
        };
        self.enqueue_progress_update(
            progress_id.clone(),
            Some("Disassembly ready".to_string()),
            Some(100),
        );
        self.enqueue_progress_end(progress_id, Some("Disassembly complete".to_string()));
        self.drain_events()?;
        self.disasm_cache_by_addr.insert(addr, entry.clone());
        self.disasm_cache_by_reference
            .insert(reference, entry.clone());
        Ok(Some(entry))
    }

    pub(super) fn handle_loaded_sources(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let sources = self
            .module_info
            .as_ref()
            .map(|info| vec![info.source.clone()])
            .unwrap_or_default();
        self.send_success_body(req, json!({ "sources": sources }))
    }

    pub(super) fn handle_modules(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let progress_id = self.enqueue_progress_start(
            "Loading modules",
            Some("Collecting module list".to_string()),
            Some(0),
        );
        self.drain_events()?;
        let modules = self
            .module_info
            .as_ref()
            .map(|info| vec![info.module.clone()])
            .unwrap_or_default();
        let total = modules.len();
        self.enqueue_progress_update(
            progress_id.clone(),
            Some(format!("Loaded {} module(s)", total)),
            Some(100),
        );
        self.enqueue_progress_end(progress_id, Some("Module list ready".to_string()));
        self.drain_events()?;
        self.send_success_body(req, json!({ "modules": modules, "totalModules": total }))
    }

    pub(super) fn handle_source(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| anyhow!("source: arguments must be object"))?;

        let source_obj = args.get("source").and_then(serde_json::Value::as_object);

        let source_reference = args
            .get("sourceReference")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                source_obj
                    .and_then(|obj| obj.get("sourceReference"))
                    .and_then(serde_json::Value::as_i64)
            })
            .filter(|value| *value > 0);

        if let Some(source_reference) = source_reference {
            if let Some(disasm) = self.disasm_cache_by_reference.get(&source_reference) {
                return self.send_success_body(
                    req,
                    serde_json::json!({
                        "content": disasm.content,
                        "mimeType": "text/x-asm"
                    }),
                );
            }
            return self.send_success_body(
                req,
                serde_json::json!({
                    "content": format!(
                        "No cached disassembly found for sourceReference {source_reference}."
                    ),
                    "mimeType": "text/plain"
                }),
            );
        }

        let source_obj = source_obj.ok_or_else(|| anyhow!("source: missing source object"))?;
        let path = source_obj
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("source: missing source.path"))?;

        let read_source = |candidate: &str| -> Option<String> {
            let normalized = SourceMap::norm_path(candidate);
            // VSCode sometimes sends relative glibc paths like "./nptl/pthread_kill.c"
            let try_paths = [
                std::path::PathBuf::from(&normalized),
                std::path::PathBuf::from(normalized.trim_start_matches("./")),
            ];

            for p in &try_paths {
                if let Ok(data) = std::fs::read_to_string(p) {
                    return Some(data);
                }
            }

            None
        };

        let mapped_path = self.source_map.map_client_to_target(path);
        let mut content = read_source(&mapped_path);
        if content.is_none() {
            let fallback_path = self.source_map.map_target_to_client(path);
            content = read_source(&fallback_path);
        }

        let Some(content) = content else {
            return self.send_err(
                req,
                format!(
                    "Could not load source '{}': file not found on adapter host",
                    path
                ),
            );
        };

        self.send_success_body(
            req,
            serde_json::json!({
                "content": content,
                "mimeType": "text/x-c"
            }),
        )
    }
}

pub fn disassemble_from_address(
    dbg: &debugger::Debugger,
    addr: usize,
    instruction_count: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<DisasmInstruction>> {
    let cs = new_capstone().map_err(|err| anyhow!("disassemble: init capstone: {err}"))?;
    let max_len = 16usize;
    let read_len = instruction_count.saturating_mul(max_len).max(max_len);
    let start = Instant::now();
    let mut bytes = dbg
        .read_memory(addr, read_len)
        .context("disassemble: read_memory")?;
    let elapsed = start.elapsed();
    if elapsed > timeout {
        anyhow::bail!(
            "disassemble: read_memory timed out after {}ms",
            timeout.as_millis()
        );
    }
    // Restore original bytes at software-breakpoint addresses so the
    // disassembler sees the real instructions, not the trap opcodes.
    dbg.patch_disasm_buf(addr, &mut bytes);
    let insns = cs
        .disasm_all(&bytes, addr as u64)
        .map_err(|err| anyhow!("disassemble: disasm_all: {err}"))?;

    let mut out = Vec::new();
    for insn in insns.iter().take(instruction_count) {
        let mnemonic: &str = insn.mnemonic().unwrap_or("<unknown>");
        let op_str = insn.op_str().unwrap_or("");
        let text = if op_str.is_empty() {
            mnemonic.to_string()
        } else {
            format!("{mnemonic} {op_str}")
        };
        let bytes_hex = insn
            .bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("");
        out.push(DisasmInstruction {
            address: insn.address(),
            bytes_hex,
            text,
        });
    }
    Ok(out)
}

pub fn disassemble_from_range(
    dbg: &debugger::Debugger,
    start_addr: usize,
    end_addr: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<DisasmInstruction>> {
    if end_addr <= start_addr {
        return Ok(Vec::new());
    }
    let len = end_addr - start_addr;
    let max_len = 0x10000usize;
    let read_len = len.min(max_len);
    let cs = new_capstone().map_err(|err| anyhow!("disassemble: init capstone: {err}"))?;
    let start = Instant::now();
    let mut bytes = dbg
        .read_memory(start_addr, read_len)
        .context("disassemble: read_memory")?;
    let elapsed = start.elapsed();
    if elapsed > timeout {
        anyhow::bail!(
            "disassemble: read_memory timed out after {}ms",
            timeout.as_millis()
        );
    }
    dbg.patch_disasm_buf(start_addr, &mut bytes);
    let insns = cs
        .disasm_all(&bytes, start_addr as u64)
        .map_err(|err| anyhow!("disassemble: disasm_all: {err}"))?;

    let mut out = Vec::new();
    for insn in insns.iter() {
        let mnemonic: &str = insn.mnemonic().unwrap_or("<unknown>");
        let op_str = insn.op_str().unwrap_or("");
        let text = if op_str.is_empty() {
            mnemonic.to_string()
        } else {
            format!("{mnemonic} {op_str}")
        };
        let bytes_hex = insn
            .bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("");
        out.push(DisasmInstruction {
            address: insn.address(),
            bytes_hex,
            text,
        });
    }
    Ok(out)
}

// SPDX-License-Identifier: MIT
use crate::debugger::address::GlobalAddress;
use object::{Object, ObjectSymbol, ObjectSymbolTable, SymbolKind};
use regex::Regex;
use std::collections::HashMap;

/// Strip the Rust symbol hash suffix (e.g. `17h<16 hex chars>E`) from a mangled name.
fn strip_rust_hash(name: &str) -> &str {
    // Rust hash suffix: "17h" + 16 hex chars + "E" = 20 chars total
    if name.len() >= 20 && name.ends_with('E') {
        let suffix_start = name.len() - 20;
        let suffix = &name[suffix_start..];
        if suffix.starts_with("17h") && suffix[3..19].bytes().all(|b| b.is_ascii_hexdigit()) {
            return &name[..suffix_start];
        }
    }
    name
}

/// Maps mangled ELF symbol names to TLS segment offsets for STT_TLS symbols.
/// Supports both exact matching and hash-stripped fallback for const-init TLS.
#[derive(Debug, Clone)]
pub(super) struct TlsSymbolTab {
    exact: HashMap<String, u64>,
    stripped: HashMap<String, u64>,
}

impl TlsSymbolTab {
    pub(super) fn new<'data, 'file, OBJ>(object_file: &'data OBJ) -> Option<Self>
    where
        'data: 'file,
        OBJ: Object<'data, 'file>,
    {
        object_file.symbol_table().as_ref().map(|sym_table| {
            let mut exact = HashMap::new();
            let mut stripped = HashMap::new();
            for s in sym_table.symbols().filter(|s| s.kind() == SymbolKind::Tls) {
                if let Ok(name) = s.name() {
                    exact.insert(name.to_string(), s.address());
                    stripped.insert(strip_rust_hash(name).to_string(), s.address());
                }
            }
            TlsSymbolTab { exact, stripped }
        })
    }

    /// Exact match by full mangled name.
    pub fn get_offset(&self, mangled_name: &str) -> Option<u64> {
        self.exact.get(mangled_name).copied()
    }

    /// Hash-stripped match, used as fallback for const-init TLS closures
    /// where the init closure's hash differs from the ELF symbol's hash.
    pub fn get_offset_stripped(&self, mangled_name: &str) -> Option<u64> {
        self.stripped.get(strip_rust_hash(mangled_name)).copied()
    }
}

#[derive(Debug, Clone)]
pub struct Symbol<'a> {
    pub name: &'a str,
    pub kind: SymbolKind,
    pub addr: GlobalAddress,
}

#[derive(Debug, Clone)]
struct SymbolVal {
    pub kind: SymbolKind,
    pub addr: GlobalAddress,
}

type Name = String;

#[derive(Debug, Clone)]
pub(super) struct SymbolTab {
    by_name: HashMap<Name, SymbolVal>,
    /// Phase 3 Feature A — reverse index for vtable resolution.
    /// Keys are object-file (link-time, pre-relocation) addresses,
    /// values are the original *mangled* symbol names so the
    /// rust-mangle-tree consumer can re-parse and walk to
    /// `impl_self_type()`.
    by_address: HashMap<u64, String>,
    /// Sorted list of `(symbol_address, mangled_name)`, ordered by
    /// address. Used to look up the *containing* function for an
    /// arbitrary PC: `addresses.binary_search_by_key(probe, |p| p.0)`
    /// then take the previous entry. Built from text-section symbol
    /// kinds only so we don't include data symbols that would skew
    /// range computations.
    sorted_text: Vec<(u64, String)>,
}

impl SymbolTab {
    pub(super) fn new<'data, 'file, OBJ>(object_file: &'data OBJ) -> Option<Self>
    where
        'data: 'file,
        OBJ: Object<'data, 'file>,
    {
        object_file.symbol_table().as_ref().map(|sym_table| {
            let mut by_name: HashMap<Name, SymbolVal> = HashMap::new();
            let mut by_address: HashMap<u64, String> = HashMap::new();
            let mut sorted_text: Vec<(u64, String)> = Vec::new();
            for symbol in sym_table.symbols() {
                let raw = symbol.name().unwrap_or_default();
                // Mach-O nlist names carry a leading underscore that
                // ELF doesn't — `__R...` for a Rust v0 symbol, `__ZN`
                // for legacy. `rust-mangle-tree` expects the stripped
                // form (`_R...` / `_ZN...`); peel one leading
                // underscore if the symbol looks like a mangled name.
                let demangle_input = if raw.starts_with("__R") || raw.starts_with("__Z") {
                    &raw[1..]
                } else {
                    raw
                };
                // Phase 2 batch H: drive demangling through
                // `rust-mangle-tree`. Falls back to the raw
                // mangled string on parse error so a single
                // bad symbol can't poison the whole table.
                let demangled = match rust_mangle_tree::parse(demangle_input) {
                    Ok(sym) => sym.to_string(),
                    Err(_) => raw.to_string(),
                };
                by_name.insert(
                    demangled,
                    SymbolVal {
                        kind: symbol.kind(),
                        addr: symbol.address().into(),
                    },
                );
                // Keep the raw mangled name in the address index —
                // vtable resolution re-parses it via rust-mangle-tree
                // and walks to `impl_self_type()`.
                by_address.insert(symbol.address(), raw.to_string());
                if symbol.kind() == object::SymbolKind::Text {
                    sorted_text.push((symbol.address(), raw.to_string()));
                }
            }
            sorted_text.sort_unstable_by_key(|p| p.0);
            sorted_text.dedup_by_key(|p| p.0);
            SymbolTab {
                by_name,
                by_address,
                sorted_text,
            }
        })
    }

    /// Phase 3 Feature A — exact-address lookup for vtable
    /// resolution. Returns the *mangled* symbol name at `addr`,
    /// or `None` if the address has no symbol (typical for
    /// stripped binaries or compiler-internal anonymous globals).
    pub fn mangled_at(&self, addr: u64) -> Option<&str> {
        self.by_address.get(&addr).map(String::as_str)
    }

    /// Reverse lookup — given a demangled symbol name (the form
    /// `rust-mangle-tree` produces, e.g. `showcase::main`), return
    /// the address the linker placed it at. Used by the line-
    /// resolution filter to validate that a candidate PC is
    /// physically in the expected function, not in foreign code
    /// that DWARF mis-claims as part of the same subprogram.
    pub fn address_of(
        &self,
        demangled_name: &str,
    ) -> Option<crate::debugger::address::GlobalAddress> {
        self.by_name.get(demangled_name).map(|v| v.addr)
    }

    /// Find the text symbol whose address range `[addr, next_addr)`
    /// contains `pc`. Returns `(start, end, mangled_name)`. Used to
    /// answer "what function does this PC physically belong to" —
    /// the linker's view, which is more reliable than DWARF
    /// subprogram ranges (those can claim impossibly-wide ranges
    /// after LTO).
    ///
    /// `next_addr` is the next text symbol's address (or `u64::MAX`
    /// past the last symbol). Caller treats `pc >= end` as "outside
    /// any known function".
    pub fn containing_text_symbol(&self, pc: u64) -> Option<(u64, u64, &str)> {
        let idx = match self.sorted_text.binary_search_by_key(&pc, |p| p.0) {
            Ok(i) => i,
            Err(i) if i > 0 => i - 1,
            Err(_) => return None,
        };
        let (start, ref name) = self.sorted_text[idx];
        let end = self
            .sorted_text
            .get(idx + 1)
            .map(|p| p.0)
            .unwrap_or(u64::MAX);
        Some((start, end, name.as_str()))
    }

    pub fn find(&'_ self, regex: &Regex) -> Vec<Symbol<'_>> {
        let keys = self.by_name.keys().filter(|key| {
            let s = key.as_str();
            if regex.find(s).is_some() {
                return true;
            }
            // Mach-O symbol names carry a leading `_` that ELF doesn't.
            // Match the user's regex against the underscore-stripped
            // form too so that `^main$` finds `_main`, `^foo$` finds
            // `_foo`, etc. (Rust-mangled names like `__ZN…` don't have
            // a single leading underscore — they start with `__Z` —
            // so this only affects C-level symbols.)
            #[cfg(target_os = "macos")]
            if let Some(stripped) = s.strip_prefix('_')
                && !stripped.starts_with('_')
            {
                return regex.find(stripped).is_some();
            }
            false
        });
        keys.map(|k| {
            let s = &self.by_name[k];
            Symbol {
                name: k.as_str(),
                kind: s.kind,
                addr: s.addr,
            }
        })
        .collect()
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod showcase_lookup_tests {
    //! Static smoke tests: build a `SymbolTab` straight from
    //! `examples/target/debug/showcase` on disk and assert the
    //! address-keyed lookups bs's trait-object resolver depends
    //! on. No debuggee, no ptrace, no mach syscalls — purely
    //! exercises the object-file parse path. Helps tell apart
    //! "resolver logic broken" from "lookup misses at runtime
    //! because of slide" when investigating the `&dyn` rendering
    //! gap on darwin.
    use super::SymbolTab;
    use object::{Object as _, ObjectSymbol as _};
    use std::fs;
    use std::path::PathBuf;

    fn binary_path() -> PathBuf {
        let manifest = env!("CARGO_MANIFEST_DIR");
        PathBuf::from(manifest).join("examples/target/debug/showcase")
    }

    #[test]
    fn greet_method_resolves_by_address() {
        let path = binary_path();
        if !path.exists() {
            eprintln!("skipping — showcase debug binary not built at {path:?}");
            return;
        }
        let data = fs::read(&path).expect("read showcase");
        let obj = object::File::parse(&*data).expect("parse showcase");
        let tab = SymbolTab::new(&obj).expect("symbol table");

        // The greet method address is rustc/wild dependent. Find it
        // by name, then round-trip through the address lookup.
        // Legacy mangling renders `<X as Y>` inside a single
        // length-prefixed segment using `..` for `::`, so we match on
        // the readable forms rather than the `8showcase4main` shape
        // that only appears for top-level paths.
        let greet_name = obj
            .symbols()
            .filter_map(|s| s.name().ok().map(|n| (n.to_string(), s.address())))
            .find(|(n, _)| {
                (n.contains("showcase..main..Point")
                    || n.contains("8showcase4main"))
                    && n.contains("Greeter")
                    && n.contains("5greet")
            });
        let Some((expected_name, addr)) = greet_name else {
            panic!("no greet method symbol in showcase — has the binary been rebuilt?");
        };
        eprintln!("[lookup] found {expected_name} @ {addr:#x}");

        let got = tab.mangled_at(addr);
        assert!(
            got.is_some(),
            "SymbolTab.mangled_at({addr:#x}) returned None — \
             this is bs's runtime resolution path; if it misses statically \
             nothing else can save us"
        );
        let got = got.unwrap();
        assert!(
            got.contains("Greeter") && got.contains("5greet"),
            "mangled_at returned {got:?}, expected the greet method"
        );
        eprintln!("[lookup] mangled_at({addr:#x}) = {got}");
    }

    /// Strategy 1 end-to-end sim: locate the `Point as Greeter`
    /// vtable in `__DATA_CONST,__const` by hunting for slot 3 (the
    /// `greet` fn ptr), read the 16-byte vtable layout straight
    /// from the binary's file contents, then for each non-zero slot
    /// look the address up in `SymbolTab` and demangle through
    /// `concrete_from_vtable_symbol`. This is exactly what bs does
    /// at runtime — but on static bytes, so no debuggee, no
    /// inferior, no kernel risk. If this passes, the resolver
    /// chain works for the showcase fixture; if it fails, the bug
    /// is between the renderer hook and what we sim here.
    #[test]
    fn strategy1_sim_resolves_point() {
        use object::ObjectSection as _;
        let path = binary_path();
        if !path.exists() {
            eprintln!("skipping — showcase debug binary not built at {path:?}");
            return;
        }
        let data = fs::read(&path).expect("read showcase");
        let obj = object::File::parse(&*data).expect("parse showcase");
        let tab = SymbolTab::new(&obj).expect("symbol table");

        // Find `greet`'s address.
        let greet_addr: u64 = obj.symbols()
            .filter_map(|s| s.name().ok().map(|n| (n, s.address())))
            .find(|(n, _)| (n.contains("showcase..main..Point")
                || n.contains("8showcase4mainNtB2_5Point"))
                && n.contains("Greeter")
                && n.contains("5greet"))
            .map(|(_, a)| a)
            .expect("greet symbol present");

        // Find a vtable by scanning __DATA_CONST,__const for a u64
        // equal to greet_addr (slot 3 of the vtable). The vtable
        // base is 24 bytes earlier. The `object` crate exposes
        // Mach-O sections by their bare `sectname` (with collisions
        // across segments resolved by iteration order); the safer
        // probe is to iterate all sections and filter by segment.
        let section = obj.sections()
            .find(|s| {
                let seg = s.segment_name_bytes().ok().flatten();
                let name = s.name_bytes().ok();
                seg == Some(b"__DATA_CONST" as &[u8]) && name == Some(b"__const" as &[u8])
            })
            .expect("__DATA_CONST,__const section");
        let bytes = section.data().expect("section data");
        let base_addr = section.address();
        let mut vtable_base: Option<u64> = None;
        for off in (0..bytes.len().saturating_sub(8)).step_by(8) {
            let v = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            if v == greet_addr && off >= 24 {
                vtable_base = Some(base_addr + (off - 24) as u64);
                break;
            }
        }
        let vtable_base = vtable_base.expect("greet appears in __const exactly once");
        eprintln!("[sim] vtable base @ {vtable_base:#x}");

        // Read 16 u64 slots starting at vtable_base from the file.
        let vtable_off = (vtable_base - base_addr) as usize;
        let slots: Vec<u64> = (0..16)
            .filter_map(|i| {
                let o = vtable_off + i * 8;
                if o + 8 > bytes.len() {
                    return None;
                }
                Some(u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()))
            })
            .collect();
        eprintln!("[sim] slots: {:?}", slots.iter().map(|s| format!("{s:#x}")).collect::<Vec<_>>());

        // Replay Strategy 1 — return the first slot whose mangled
        // name extracts a concrete type. (Pulling the helper out of
        // parser.rs would make this cleaner; for now we re-implement
        // the legacy + v0 surgery inline so this test stays in
        // symbol.rs and avoids a public re-export of the parser
        // internals.)
        let mut resolved: Option<String> = None;
        for slot in &slots {
            if *slot == 0 {
                continue;
            }
            let Some(raw) = tab.mangled_at(*slot) else {
                continue;
            };
            // Apply the same Mach-O underscore peel we fixed in
            // parser.rs.
            let peeled = if raw.starts_with("__R") || raw.starts_with("__Z") {
                &raw[1..]
            } else {
                raw
            };
            let parsed = match rust_mangle_tree::parse(peeled) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let s = match parsed {
                rust_mangle_tree::Symbol::V0(path) => {
                    // Walk for an impl_self_type.
                    fn walk<'a>(p: &'a rust_mangle_tree::Path<'a>) -> Option<rust_mangle_tree::Type<'a>> {
                        if let Some(t) = p.impl_self_type() { return Some(t.clone()); }
                        match p {
                            rust_mangle_tree::Path::Nested { parent, .. }
                            | rust_mangle_tree::Path::Generic { parent, .. } => walk(parent),
                            _ => None,
                        }
                    }
                    walk(&path).and_then(|t| match t {
                        rust_mangle_tree::Type::Path(p) => Some(p.to_string()),
                        _ => None,
                    })
                }
                rust_mangle_tree::Symbol::Legacy(_) => {
                    let demangled = format!("{parsed:#}");
                    (|| {
                        let lt = demangled.find('<')?;
                        let as_kw = demangled[lt..].find(" as ")?;
                        Some(demangled[lt + 1..lt + as_kw].trim().to_string())
                    })()
                }
                _ => None,
            };
            if let Some(name) = s {
                resolved = Some(name);
                break;
            }
        }
        assert_eq!(
            resolved.as_deref(),
            Some("showcase::main::Point"),
            "Strategy 1 simulation should reproduce the user's expected concrete type"
        );
    }

    #[test]
    fn vtable_base_has_no_symbol() {
        // The `<Point as Greeter>` vtable lives in `__DATA_CONST,__const`
        // and rustc does NOT export it as a named symbol on darwin.
        // bs's Strategy 2 (exact-address lookup of the vtable's own
        // symbol) therefore always misses for showcase — Strategy 1
        // (probe slots, look up the fn ptrs) is the only path that
        // can succeed.
        let path = binary_path();
        if !path.exists() {
            eprintln!("skipping — showcase debug binary not built at {path:?}");
            return;
        }
        let data = fs::read(&path).expect("read showcase");
        let obj = object::File::parse(&*data).expect("parse showcase");
        let tab = SymbolTab::new(&obj).expect("symbol table");

        // No symbol should sit at any plausibly-vtable address inside
        // __DATA_CONST,__const. Sweep a small window around our
        // hypothesised base to be tolerant to layout drift.
        for probe_addr in (0x100068000u64..0x100069000u64).step_by(8) {
            if let Some(name) = tab.mangled_at(probe_addr) {
                // Only fail if any name we found mentions Greeter —
                // unrelated data symbols are fine.
                assert!(
                    !name.contains("Greeter"),
                    "unexpected Greeter-named symbol at {probe_addr:#x}: {name}"
                );
            }
        }
    }
}

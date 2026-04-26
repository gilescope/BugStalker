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
        if suffix.starts_with("17h")
            && suffix[3..19].bytes().all(|b| b.is_ascii_hexdigit())
        {
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
pub(super) struct SymbolTab(HashMap<Name, SymbolVal>);

impl SymbolTab {
    pub(super) fn new<'data, 'file, OBJ>(object_file: &'data OBJ) -> Option<Self>
    where
        'data: 'file,
        OBJ: Object<'data, 'file>,
    {
        object_file.symbol_table().as_ref().map(|sym_table| {
            SymbolTab(
                sym_table
                    .symbols()
                    .map(|symbol| {
                        let name = symbol.name().unwrap_or_default();
                        let name = rustc_demangle::demangle(name).to_string();
                        (
                            name,
                            SymbolVal {
                                kind: symbol.kind(),
                                addr: symbol.address().into(),
                            },
                        )
                    })
                    .collect::<HashMap<_, _>>(),
            )
        })
    }

    pub fn find(&'_ self, regex: &Regex) -> Vec<Symbol<'_>> {
        let keys = self.0.keys().filter(|key| {
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
            let s = &self.0[k];
            Symbol {
                name: k.as_str(),
                kind: s.kind,
                addr: s.addr,
            }
        })
        .collect()
    }
}

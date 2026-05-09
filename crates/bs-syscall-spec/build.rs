// SPDX-License-Identifier: MIT
//! Build-time codegen for the long-tail syscall tables.
//!
//! Reads `data/syscall_64.tbl` and `data/syscall_aarch64.tbl`,
//! parses `<nr> <name>` lines, and emits two const slices:
//!
//! - `LONG_TAIL_X86_64: &[GenericSyscall]`
//! - `LONG_TAIL_AARCH64: &[GenericSyscall]`
//!
//! The lib's `include!`s pull both in. Anyone editing either
//! table:
//!
//! - Numbers strictly increasing.
//! - No duplicate names *within* a table.
//! - Identifiers contain only `[A-Za-z0-9_]`.
//!
//! Errors print with file/line so the build failure points to
//! the offending row (rustc-level kindness).

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

const X86_64_TABLE: &str = "data/syscall_64.tbl";
const AARCH64_TABLE: &str = "data/syscall_aarch64.tbl";

fn main() {
    println!("cargo:rerun-if-changed={X86_64_TABLE}");
    println!("cargo:rerun-if-changed={AARCH64_TABLE}");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set by cargo");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let out = PathBuf::from(out_dir);

    emit_table(
        &PathBuf::from(&manifest_dir).join(X86_64_TABLE),
        &out.join("long_tail_x86_64.rs"),
        "LONG_TAIL_X86_64",
        "x86-64",
    );
    emit_table(
        &PathBuf::from(&manifest_dir).join(AARCH64_TABLE),
        &out.join("long_tail_aarch64.rs"),
        "LONG_TAIL_AARCH64",
        "aarch64",
    );
}

fn emit_table(src: &std::path::Path, dst: &std::path::Path, const_name: &str, arch_label: &str) {
    let raw = fs::read_to_string(src).unwrap_or_else(|e| {
        panic!(
            "couldn't read {arch_label} syscall table at {}: {e}\n\
             — verify the file exists and is committed to the repo.",
            src.display(),
        )
    });
    let entries = parse_table(&raw, src).unwrap_or_else(|e| panic!("{e}"));
    let body = render(&entries, const_name, arch_label, src);
    fs::write(dst, body)
        .unwrap_or_else(|e| panic!("couldn't write generated table to {}: {e}", dst.display()));
}

#[derive(Debug)]
struct Entry {
    nr: u32,
    name: String,
    src_line: usize,
}

fn parse_table(raw: &str, path: &std::path::Path) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut last_nr: Option<u32> = None;

    for (idx0, line) in raw.lines().enumerate() {
        let line_no = idx0 + 1;
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut cols = trimmed.split_whitespace();
        let nr_str = cols.next().ok_or_else(|| {
            format!(
                "{}:{line_no}: empty row after stripping comment",
                path.display()
            )
        })?;
        let name = cols.next().ok_or_else(|| {
            format!(
                "{}:{line_no}: row `{trimmed}` is missing a name (expected `<nr> <name>`)",
                path.display(),
            )
        })?;
        if cols.next().is_some() {
            return Err(format!(
                "{}:{line_no}: row `{trimmed}` has more than two columns",
                path.display(),
            ));
        }
        let nr: u32 = nr_str.parse().map_err(|e| {
            format!(
                "{}:{line_no}: column 1 `{nr_str}` is not a valid u32 ({e})",
                path.display(),
            )
        })?;
        if !is_valid_ident(name) {
            return Err(format!(
                "{}:{line_no}: name `{name}` contains characters that aren't [A-Za-z0-9_]",
                path.display(),
            ));
        }
        if !seen_names.insert(name.to_owned()) {
            return Err(format!(
                "{}:{line_no}: duplicate syscall name `{name}`",
                path.display(),
            ));
        }
        if let Some(prev) = last_nr {
            if nr <= prev {
                return Err(format!(
                    "{}:{line_no}: row `{trimmed}` is out of order \
                     (nr {nr} not strictly greater than previous {prev}); \
                     keep the table sorted by number",
                    path.display(),
                ));
            }
        }
        last_nr = Some(nr);
        entries.push(Entry {
            nr,
            name: name.to_owned(),
            src_line: line_no,
        });
    }
    if entries.is_empty() {
        return Err(format!("{}: no entries parsed", path.display()));
    }
    Ok(entries)
}

fn is_valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn render(entries: &[Entry], const_name: &str, arch_label: &str, src: &std::path::Path) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "// Auto-generated from {} by build.rs.\n\
         // Do not edit by hand; edit the source table instead.",
        src.display(),
    );
    let _ = writeln!(
        s,
        "/// Long-tail {arch_label} syscalls — name + number only."
    );
    let _ = writeln!(
        s,
        "/// The recorder uses this for syscalls outside the curated\n\
         /// table. Sorted by `nr`."
    );
    let _ = writeln!(s, "pub const {const_name}: &[GenericSyscall] = &[",);
    for e in entries {
        let _ = writeln!(
            s,
            "    GenericSyscall {{ nr: {}, name: {:?} }}, // line {}",
            e.nr, e.name, e.src_line,
        );
    }
    let _ = writeln!(s, "];");
    s
}

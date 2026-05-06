// SPDX-License-Identifier: MIT
//! Build-time codegen for the long-tail x86-64 syscall table.
//!
//! Reads `data/syscall_64.tbl`, parses `<nr> <name>` lines, and
//! emits a `LONG_TAIL_X86_64: &[GenericSyscall]` const slice
//! into `$OUT_DIR/long_tail_x86_64.rs`. The lib's `include!`
//! pulls it in.
//!
//! Validations performed at build time:
//!
//! - Numbers strictly increasing — anyone editing the table out
//!   of order finds out immediately rather than hitting a
//!   silent linear-scan bug at runtime.
//! - No duplicate names.
//! - Identifiers contain only `[A-Za-z0-9_]`.
//!
//! Errors print with file/line so the build failure points to
//! the offending row (rustc-level kindness — see CLAUDE.md
//! "Detailed diagnostics are good").

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

const TABLE_RELATIVE: &str = "data/syscall_64.tbl";

fn main() {
    println!("cargo:rerun-if-changed={TABLE_RELATIVE}");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set by cargo");
    let table_path = PathBuf::from(&manifest_dir).join(TABLE_RELATIVE);
    let raw = fs::read_to_string(&table_path).unwrap_or_else(|e| {
        panic!(
            "couldn't read syscall table at {}: {e}\n\
             — verify the file exists and is committed to the repo.",
            table_path.display(),
        )
    });

    let entries = parse_table(&raw, &table_path).unwrap_or_else(|e| {
        // Fail the build with file:line:reason — rustc-level
        // kindness lets the editor jump straight to the offender.
        panic!("{e}");
    });

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let out_path = PathBuf::from(out_dir).join("long_tail_x86_64.rs");
    let body = render(&entries);
    fs::write(&out_path, body).unwrap_or_else(|e| {
        panic!("couldn't write generated table to {}: {e}", out_path.display())
    });
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
            format!("{}:{line_no}: empty row after stripping comment", path.display())
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
        entries.push(Entry { nr, name: name.to_owned(), src_line: line_no });
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

fn render(entries: &[Entry]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "// Auto-generated from data/syscall_64.tbl by build.rs.\n\
         // Do not edit by hand; edit the source table instead."
    );
    let _ = writeln!(s, "/// Long-tail x86-64 syscalls — name + number only.");
    let _ = writeln!(
        s,
        "/// The recorder uses this for syscalls outside the curated\n\
         /// [`KNOWN_X86_64`] table. Sorted by `nr`."
    );
    let _ = writeln!(
        s,
        "pub const LONG_TAIL_X86_64: &[GenericSyscall] = &[",
    );
    for e in entries {
        let _ = writeln!(
            s,
            "    GenericSyscall {{ nr: {}, name: {:?} }}, // line {} of data/syscall_64.tbl",
            e.nr, e.name, e.src_line,
        );
    }
    let _ = writeln!(s, "];");
    s
}

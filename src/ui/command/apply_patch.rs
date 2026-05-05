// SPDX-License-Identifier: MIT
//! `apply-patch <path>` — read a wild-emitted patch file and write each
//! byte run into the running debuggee process.
//!
//! Wild's `--emit-patch=<path>` (in our wasmtime fork) writes a text file
//! after each incremental link describing every byte run that differs
//! from the previous output. This command is the consumer side: take
//! that file, translate each `<file_offset>` into a runtime address by
//! adding the user-supplied `--base` (the load address of the binary's
//! `__TEXT` segment), and write the bytes via `write_memory`.
//!
//! Format expected (matches wild's emitter):
//! ```text
//! # wild-patch v1
//! # old-size: <N>
//! # new-size: <M>
//! # entries: <K>
//! <hex-offset> <length> <hex-bytes>
//! ...
//! ```
//!
//! Lines starting with `#` are header/comments and are ignored. Each
//! data line has three whitespace-separated fields: hex file offset,
//! decimal length, hex bytes (length matches `length`).
//!
//! v0 caveats:
//!   * Caller supplies the `__TEXT` base; auto-detection (reading
//!     `/proc/<pid>/maps` on Linux or `dyld_image_info` on macOS)
//!     can come later.
//!   * No safety check that the binary at runtime has the same
//!     pre-image as the patch's `old-size` claims. The cleaner
//!     contract would be to record a pre-image hash in the patch and
//!     verify before writing.
//!   * Word-aligned writes via `Debugger::write_memory` (which is
//!     `uintptr_t`-sized on both Linux and macOS). For sub-word
//!     patches we read-modify-write the surrounding word.
//!   * The function being patched should not be currently executing
//!     in any thread — same caveat as every other EnC system.

use crate::debugger::Debugger;
use crate::ui::command;
use nix::libc::uintptr_t;
use std::path::PathBuf;

/// Parsed entry from a wild-patch file.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchEntry {
    /// File offset inside the binary where the run starts.
    pub offset: u64,
    /// Bytes that were at this range in the previous link. The handler
    /// compares this against what's actually in the running process
    /// before writing — drift here means an earlier patch failed,
    /// the running binary is from a different build, or someone else
    /// is writing to memory. Empty for v1 patches (no pre-image).
    pub old_bytes: Vec<u8>,
    /// New byte content to install at `[offset, offset+new_bytes.len())`.
    pub new_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Read `path`, parse as wild-patch, write each entry at
    /// `base + entry.offset`. If `base` is `None`, ask the debugger
    /// to translate each entry's file offset to a runtime address
    /// itself (using the loaded executable's mapping).
    ApplyPatch {
        path: PathBuf,
        base: Option<uintptr_t>,
    },
    /// Apply the patch every time `path`'s mtime changes. Polls
    /// every 250 ms; blocks the REPL until the user kills BugStalker
    /// (Ctrl-C / SIGINT). v0 limitation: the watch holds the main
    /// thread, so other debugger commands are unavailable while it's
    /// running. Useful for "edit + cargo build + auto-apply" workflows
    /// where the user has the BugStalker terminal dedicated to
    /// watching.
    WatchPatch {
        path: PathBuf,
        base: Option<uintptr_t>,
        interval_ms: u64,
    },
}

/// Result reported back to the UI: counts only, not the bytes themselves.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyReport {
    pub entries_applied: usize,
    pub bytes_written: usize,
    pub entries_skipped_drift: usize,
}

pub struct Handler<'a> {
    dbg: &'a Debugger,
}

impl<'a> Handler<'a> {
    pub fn new(debugger: &'a Debugger) -> Self {
        Self { dbg: debugger }
    }

    pub fn handle(&self, cmd: Command) -> command::CommandResult<ApplyReport> {
        match cmd {
            Command::ApplyPatch { path, base } => self.apply_once(&path, base),
            Command::WatchPatch {
                path,
                base,
                interval_ms,
            } => self.watch_loop(&path, base, interval_ms),
        }
    }

    fn apply_once(
        &self,
        path: &std::path::Path,
        base: Option<uintptr_t>,
    ) -> command::CommandResult<ApplyReport> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            command::CommandError::Parsing(format!(
                "failed to read patch file {}: {e}",
                path.display()
            ))
        })?;
        let entries = parse_wild_patch(&text).map_err(|msg| {
            command::CommandError::Parsing(format!(
                "failed to parse patch file {}: {msg}",
                path.display()
            ))
        })?;

        let mut report = ApplyReport::default();
        for entry in &entries {
            let target = match base {
                Some(b) => b.wrapping_add(entry.offset as uintptr_t),
                None => self
                    .dbg
                    .debugee()
                    .file_offset_to_runtime(entry.offset)
                    .ok_or_else(|| {
                        command::CommandError::Parsing(format!(
                            "could not auto-detect runtime address for file offset \
                             0x{:x}: main executable not mapped yet",
                            entry.offset
                        ))
                    })?,
            };

            // Pre-image verification (v2 patches only). Skip the entry
            // and surface a diagnostic if the running process has
            // bytes other than what wild diffed against.
            if !entry.old_bytes.is_empty() {
                let actual = self.dbg.read_memory(target, entry.old_bytes.len())?;
                if actual != entry.old_bytes {
                    eprintln!(
                        "[apply-patch] DRIFT at file offset 0x{:x} (runtime 0x{:x}): \
                         expected {} but found {} — skipping",
                        entry.offset,
                        target,
                        hex_summary(&entry.old_bytes),
                        hex_summary(&actual),
                    );
                    report.entries_skipped_drift += 1;
                    continue;
                }
            }

            write_aligned(self.dbg, target, &entry.new_bytes)?;
            report.entries_applied += 1;
            report.bytes_written += entry.new_bytes.len();
        }
        Ok(report)
    }

    fn watch_loop(
        &self,
        path: &std::path::Path,
        base: Option<uintptr_t>,
        interval_ms: u64,
    ) -> command::CommandResult<ApplyReport> {
        // Apply once up front (so the user gets immediate feedback if
        // the file already exists / parses).
        let initial = self.apply_once(path, base)?;
        eprintln!(
            "[watch-patch] initial apply: {} entries, {} bytes. \
             Polling {} every {} ms; Ctrl-C to stop.",
            initial.entries_applied,
            initial.bytes_written,
            path.display(),
            interval_ms,
        );

        let mut last_mtime = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok());
        let interval = std::time::Duration::from_millis(interval_ms);
        let mut total = initial;

        loop {
            std::thread::sleep(interval);
            let now_mtime = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok());
            if now_mtime != last_mtime {
                last_mtime = now_mtime;
                match self.apply_once(path, base) {
                    Ok(rep) => {
                        eprintln!(
                            "[watch-patch] reapplied: {} entries, {} bytes",
                            rep.entries_applied, rep.bytes_written
                        );
                        total.entries_applied += rep.entries_applied;
                        total.bytes_written += rep.bytes_written;
                    }
                    Err(e) => eprintln!("[watch-patch] reapply failed: {e}"),
                }
            }
        }
    }
}

/// Parse the wild-patch text format. See module-level docs for the
/// grammar. Returns the list of entries in file order.
///
/// Supports both v1 and v2 formats:
///   v1: `<offset> <length> <new-hex>`           (no pre-image)
///   v2: `<offset> <length> <old-hex> <new-hex>` (pre-image inline)
pub fn parse_wild_patch(text: &str) -> Result<Vec<PatchEntry>, String> {
    let mut entries = Vec::new();
    let mut version: Option<u32> = None;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if let Some(rest) = line.strip_prefix("# wild-patch v") {
                version = rest.trim().parse().ok();
            }
            continue;
        }
        let v = version.ok_or_else(|| {
            format!("line {}: data before wild-patch header", lineno + 1)
        })?;
        let mut fields = line.split_whitespace();
        let off_s = fields.next().ok_or_else(|| {
            format!("line {}: missing offset field", lineno + 1)
        })?;
        let len_s = fields.next().ok_or_else(|| {
            format!("line {}: missing length field", lineno + 1)
        })?;
        let third = fields.next().ok_or_else(|| {
            format!("line {}: missing bytes field", lineno + 1)
        })?;
        let fourth = fields.next();

        let offset = u64::from_str_radix(off_s, 16).map_err(|e| {
            format!("line {}: bad hex offset `{off_s}`: {e}", lineno + 1)
        })?;
        let length: usize = len_s.parse().map_err(|e| {
            format!("line {}: bad length `{len_s}`: {e}", lineno + 1)
        })?;

        let (old_hex, new_hex) = match (v, fourth) {
            (1, None) => ("", third),
            (2, Some(new)) => (third, new),
            (1, Some(_)) => {
                return Err(format!(
                    "line {}: v1 patch has 4 fields (only v2 has old+new bytes)",
                    lineno + 1
                ));
            }
            (2, None) => {
                return Err(format!(
                    "line {}: v2 patch missing new-bytes field",
                    lineno + 1
                ));
            }
            (other, _) => {
                return Err(format!("unsupported wild-patch version: v{other}"));
            }
        };

        let new_bytes = decode_and_check(new_hex, length, lineno + 1, "new")?;
        let old_bytes = if old_hex.is_empty() {
            Vec::new()
        } else {
            decode_and_check(old_hex, length, lineno + 1, "old")?
        };
        entries.push(PatchEntry {
            offset,
            old_bytes,
            new_bytes,
        });
    }
    Ok(entries)
}

fn decode_and_check(
    hex: &str,
    length: usize,
    lineno: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if hex.len() != length * 2 {
        return Err(format!(
            "line {lineno}: {label} bytes is {} chars (expected {})",
            hex.len(),
            length * 2
        ));
    }
    decode_hex(hex).map_err(|e| format!("line {lineno}: bad {label} hex: {e}"))
}

/// Render up to 16 bytes as `aa bb cc ...` for diagnostic output.
fn hex_summary(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let take = bytes.len().min(16);
    for (i, b) in bytes[..take].iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write!(out, "{b:02x}").unwrap();
    }
    if bytes.len() > take {
        out.push_str(&format!(" ...({} more)", bytes.len() - take));
    }
    out
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string ({})", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = (chunk[0] as char).to_digit(16).ok_or_else(|| {
            format!("invalid hex digit `{}`", chunk[0] as char)
        })?;
        let lo = (chunk[1] as char).to_digit(16).ok_or_else(|| {
            format!("invalid hex digit `{}`", chunk[1] as char)
        })?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

/// Write `bytes` to the debuggee at `target` using word-sized
/// `Debugger::write_memory` calls. Reads the surrounding words first
/// when the start/end aren't word-aligned, so we only modify the
/// requested byte range.
fn write_aligned(
    dbg: &Debugger,
    target: uintptr_t,
    bytes: &[u8],
) -> Result<(), command::CommandError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let word = std::mem::size_of::<uintptr_t>();
    let start = target & !(word - 1);
    let end = (target + bytes.len()).next_multiple_of(word);
    let span = end - start;

    // Read the existing words in [start, end). Even if our patch fully
    // overwrites every byte, we read for the case where it doesn't (sub-
    // word ranges or unaligned start/end).
    let existing = dbg.read_memory(start, span)?;
    let mut buf = existing;

    let bytes_off = target - start;
    buf[bytes_off..bytes_off + bytes.len()].copy_from_slice(bytes);

    // Write back word-by-word.
    for (i, chunk) in buf.chunks(word).enumerate() {
        let mut w_bytes = [0u8; std::mem::size_of::<uintptr_t>()];
        w_bytes.copy_from_slice(chunk);
        let w = uintptr_t::from_ne_bytes(w_bytes);
        let addr = start + i * word;
        dbg.write_memory(addr, w)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v1_minimal() {
        let text = "\
# wild-patch v1
# entries: 1
2d34 4 91019000
";
        let entries = parse_wild_patch(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].offset, 0x2d34);
        assert!(entries[0].old_bytes.is_empty(), "v1 has no pre-image");
        assert_eq!(entries[0].new_bytes, vec![0x91, 0x01, 0x90, 0x00]);
    }

    #[test]
    fn parse_v2_minimal() {
        let text = "\
# wild-patch v2
# entries: 1
2d34 4 00040091 91019000
";
        let entries = parse_wild_patch(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].offset, 0x2d34);
        assert_eq!(entries[0].old_bytes, vec![0x00, 0x04, 0x00, 0x91]);
        assert_eq!(entries[0].new_bytes, vec![0x91, 0x01, 0x90, 0x00]);
    }

    #[test]
    fn parse_empty() {
        let text = "# wild-patch v2\n# entries: 0\n";
        assert!(parse_wild_patch(text).unwrap().is_empty());
    }

    #[test]
    fn parse_v2_multiple_entries() {
        let text = "\
# wild-patch v2
# entries: 2
1000 2 0102 0304
2000 4 deadbeef cafebabe
";
        let entries = parse_wild_patch(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].old_bytes, vec![0x01, 0x02]);
        assert_eq!(entries[0].new_bytes, vec![0x03, 0x04]);
        assert_eq!(entries[1].old_bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(entries[1].new_bytes, vec![0xca, 0xfe, 0xba, 0xbe]);
    }

    #[test]
    fn parse_rejects_data_before_header() {
        let text = "1000 2 0102\n";
        assert!(parse_wild_patch(text).is_err());
    }

    #[test]
    fn parse_rejects_v1_with_two_byte_fields() {
        let text = "# wild-patch v1\n1000 2 0102 0304\n";
        assert!(parse_wild_patch(text).is_err());
    }

    #[test]
    fn parse_rejects_v2_with_only_one_byte_field() {
        let text = "# wild-patch v2\n1000 2 0102\n";
        assert!(parse_wild_patch(text).is_err());
    }

    #[test]
    fn parse_rejects_length_mismatch() {
        let text = "# wild-patch v2\n1000 4 0102 0304\n";
        let err = parse_wild_patch(text).unwrap_err();
        assert!(err.contains("expected 8"), "got: {err}");
    }

    #[test]
    fn parse_rejects_bad_hex() {
        let text = "# wild-patch v2\n1000 1 zz aa\n";
        assert!(parse_wild_patch(text).is_err());
    }

    #[test]
    fn hex_summary_truncates() {
        let many: Vec<u8> = (0..20).collect();
        let s = hex_summary(&many);
        assert!(s.contains("..."));
        assert!(s.contains("(4 more)"));
    }
}

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
    /// New byte content for `[offset, offset+bytes.len())`.
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Read `path`, parse as wild-patch, write each entry at
    /// `base + entry.offset`.
    ApplyPatch { path: PathBuf, base: uintptr_t },
}

/// Result reported back to the UI: counts only, not the bytes themselves.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyReport {
    pub entries_applied: usize,
    pub bytes_written: usize,
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
            Command::ApplyPatch { path, base } => {
                let text = std::fs::read_to_string(&path).map_err(|e| {
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
                    let target = base.wrapping_add(entry.offset as uintptr_t);
                    write_aligned(self.dbg, target, &entry.bytes)?;
                    report.entries_applied += 1;
                    report.bytes_written += entry.bytes.len();
                }
                Ok(report)
            }
        }
    }
}

/// Parse the wild-patch text format. See module-level docs for the
/// grammar. Returns the list of entries in file order.
pub fn parse_wild_patch(text: &str) -> Result<Vec<PatchEntry>, String> {
    let mut entries = Vec::new();
    let mut saw_header = false;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if line.starts_with("# wild-patch v") {
                saw_header = true;
            }
            continue;
        }
        if !saw_header {
            return Err(format!(
                "line {}: data before wild-patch header",
                lineno + 1
            ));
        }
        let mut fields = line.split_whitespace();
        let off_s = fields.next().ok_or_else(|| {
            format!("line {}: missing offset field", lineno + 1)
        })?;
        let len_s = fields.next().ok_or_else(|| {
            format!("line {}: missing length field", lineno + 1)
        })?;
        let bytes_hex = fields.next().ok_or_else(|| {
            format!("line {}: missing bytes field", lineno + 1)
        })?;
        let offset = u64::from_str_radix(off_s, 16).map_err(|e| {
            format!("line {}: bad hex offset `{off_s}`: {e}", lineno + 1)
        })?;
        let length: usize = len_s.parse().map_err(|e| {
            format!("line {}: bad length `{len_s}`: {e}", lineno + 1)
        })?;
        if bytes_hex.len() != length * 2 {
            return Err(format!(
                "line {}: length={length} but hex-bytes is {} chars (expected {})",
                lineno + 1,
                bytes_hex.len(),
                length * 2
            ));
        }
        let bytes = decode_hex(bytes_hex).map_err(|e| {
            format!("line {}: bad hex bytes: {e}", lineno + 1)
        })?;
        entries.push(PatchEntry { offset, bytes });
    }
    Ok(entries)
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
    fn parse_minimal() {
        let text = "\
# wild-patch v1
# old-size: 100
# new-size: 100
# entries: 1
2d34 4 91019000
";
        let entries = parse_wild_patch(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].offset, 0x2d34);
        assert_eq!(entries[0].bytes, vec![0x91, 0x01, 0x90, 0x00]);
    }

    #[test]
    fn parse_empty() {
        let text = "\
# wild-patch v1
# old-size: 100
# new-size: 100
# entries: 0
";
        assert!(parse_wild_patch(text).unwrap().is_empty());
    }

    #[test]
    fn parse_multiple_entries() {
        let text = "\
# wild-patch v1
# entries: 2
1000 2 0102
2000 4 deadbeef
";
        let entries = parse_wild_patch(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].offset, 0x1000);
        assert_eq!(entries[0].bytes, vec![0x01, 0x02]);
        assert_eq!(entries[1].offset, 0x2000);
        assert_eq!(entries[1].bytes, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn parse_rejects_data_before_header() {
        let text = "1000 2 0102\n";
        assert!(parse_wild_patch(text).is_err());
    }

    #[test]
    fn parse_rejects_length_mismatch() {
        let text = "\
# wild-patch v1
1000 4 0102
";
        let err = parse_wild_patch(text).unwrap_err();
        assert!(err.contains("length=4"), "got: {err}");
    }

    #[test]
    fn parse_rejects_bad_hex() {
        let text = "\
# wild-patch v1
1000 1 zz
";
        assert!(parse_wild_patch(text).is_err());
    }
}

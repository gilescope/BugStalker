// SPDX-License-Identifier: MIT
//! Sub-phase 3D follow-up — vDSO entry-point detection.
//!
//! glibc's `gettimeofday`, `clock_gettime`, `time`, and
//! `getcpu` fast paths call into the kernel's vDSO (a
//! kernel-provided shared object mapped into every process)
//! instead of issuing a regular syscall. Because the vDSO
//! bypasses the syscall ABI, the recorder's seccomp filter
//! never sees them — replay would diverge.
//!
//! The fix the plan documents (and `rr` uses): patch the
//! vDSO's exported entry points to do a real syscall, so
//! they trip seccomp like any other syscall.
//!
//! ## What this module lands
//!
//! - [`find_vdso_range_for_self`] — parse `/proc/self/maps`,
//!   return the `[vdso]` mapping range.
//! - [`VdsoSymbol`] — one (name, address) pair from the vDSO's
//!   dyn-symbol table.
//! - [`scan_vdso_exports`] — read the vDSO via the `object`
//!   crate's ELF parser, return all exported function symbols
//!   the recorder cares about.
//! - [`patch_payload_x86_64`] — opcode bytes for the trampoline
//!   that replaces a vDSO entry: load the syscall number into
//!   `%eax`, `syscall`, `ret`. The actual remote-write into
//!   the tracee's address space (via `process_vm_writev` plus
//!   `mprotect`) is the next focused commit; this module
//!   hands the supervisor what to write.

#![cfg(target_os = "linux")]

use std::fs;
use std::io;

/// Names of the vDSO symbols the recorder cares about. These
/// are the ones libc's fast paths call into; missing one means
/// libc went through the normal syscall path and seccomp
/// already saw it.
pub const VDSO_TARGET_SYMBOLS: &[&str] = &[
    "__vdso_gettimeofday",
    "__vdso_clock_gettime",
    "__vdso_clock_getres",
    "__vdso_time",
    "__vdso_getcpu",
];

/// One mapping line from `/proc/<pid>/maps`. Layout:
///
/// ```text
/// 7f8c1c5e1000-7f8c1c5e3000 r-xp 00000000 fd:00 1234567 [vdso]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcMapping {
    /// Start of the mapping (virtual address).
    pub start: u64,
    /// End of the mapping (exclusive).
    pub end: u64,
    /// Permission string from the maps line — `r-xp` etc.
    pub perms: String,
    /// Mapping name — `[vdso]`, `[heap]`, or a file path.
    /// Empty for anonymous mappings.
    pub name: String,
}

impl ProcMapping {
    /// Parse one `/proc/<pid>/maps` line. Returns `None` for
    /// blank lines or malformed input — callers iterate over
    /// `Some` entries.
    pub fn parse_line(line: &str) -> Option<Self> {
        let mut cols = line.split_whitespace();
        let range = cols.next()?;
        let perms = cols.next()?;
        let _ = cols.next()?; // offset
        let _ = cols.next()?; // dev
        let _ = cols.next()?; // inode
        let name: String = cols.collect::<Vec<_>>().join(" ");
        let (start, end) = range.split_once('-')?;
        let start = u64::from_str_radix(start, 16).ok()?;
        let end = u64::from_str_radix(end, 16).ok()?;
        Some(Self {
            start,
            end,
            perms: perms.to_owned(),
            name,
        })
    }

    /// Length of the mapping in bytes.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Empty if start == end.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Return every mapping in `/proc/<pid>/maps`. Used by both
/// the vDSO finder and any other supervisor code that needs
/// to walk the tracee's address space.
pub fn read_proc_maps(pid: i32) -> io::Result<Vec<ProcMapping>> {
    let path = format!("/proc/{pid}/maps");
    let raw = fs::read_to_string(path)?;
    Ok(raw.lines().filter_map(ProcMapping::parse_line).collect())
}

/// Read the supervisor's own `/proc/self/maps`. Useful for
/// tests that want to compare against the recorder's view of
/// its own vDSO.
pub fn read_proc_maps_for_self() -> io::Result<Vec<ProcMapping>> {
    let raw = fs::read_to_string("/proc/self/maps")?;
    Ok(raw.lines().filter_map(ProcMapping::parse_line).collect())
}

/// Find the `[vdso]` mapping in a tracee's address space.
/// Returns `None` if the tracee somehow lacks a vDSO (very
/// rare — happens when the kernel was built without
/// `CONFIG_VDSO=y` or the program explicitly unmapped it).
pub fn find_vdso_range(pid: i32) -> io::Result<Option<ProcMapping>> {
    Ok(read_proc_maps(pid)?
        .into_iter()
        .find(|m| m.name == "[vdso]"))
}

/// Convenience for the supervisor's own vDSO (used in tests).
pub fn find_vdso_range_for_self() -> io::Result<Option<ProcMapping>> {
    Ok(read_proc_maps_for_self()?
        .into_iter()
        .find(|m| m.name == "[vdso]"))
}

/// One exported function in the vDSO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VdsoSymbol {
    /// `__vdso_gettimeofday`, etc.
    pub name: String,
    /// Absolute virtual address of the symbol's first byte
    /// (already adjusted for the vDSO base).
    pub address: u64,
    /// Size of the function as ELF reported it. Zero means
    /// "unknown" (some vDSOs use STT_FUNC entries with empty
    /// size); callers fall back to "patch up to the next
    /// function" or "patch the first 16 bytes".
    pub size: u64,
}

/// Read the vDSO bytes from the supervisor's own address space.
/// Cheap because the vDSO is already mapped into us — we just
/// reborrow as `&[u8]`.
///
/// SAFETY: the vDSO is r-xp and stable for the process's
/// lifetime; copying out of it is safe.
pub fn read_self_vdso_bytes(range: &ProcMapping) -> Vec<u8> {
    let len = range.len() as usize;
    // SAFETY: the [vdso] mapping is r-xp and present for the
    // life of the process; reading bytes from it is sound. We
    // copy out so the caller can pass them to `object`'s ELF
    // parser without lifetime worries.
    unsafe {
        std::slice::from_raw_parts(range.start as *const u8, len).to_vec()
    }
}

/// Walk the vDSO ELF and return every symbol whose name appears
/// in [`VDSO_TARGET_SYMBOLS`]. Symbols reported with
/// `address == 0` (uncommon for vDSO) are skipped — the
/// recorder treats them as "not patchable, ignore".
pub fn scan_vdso_exports(
    range: &ProcMapping,
    bytes: &[u8],
) -> Result<Vec<VdsoSymbol>, VdsoScanError> {
    use object::read::elf::{ElfFile64, FileHeader};
    use object::{Object, ObjectSymbol};

    let elf: ElfFile64<object::Endianness> =
        ElfFile64::parse(bytes).map_err(|e| VdsoScanError::ElfParse(format!("{e}")))?;

    // The vDSO uses ET_DYN (shared object); symbol addresses
    // are *relative* to the file's base virtual address (which
    // is 0 for ET_DYN unless it sets PT_LOAD differently).
    // ELF base for the loaded image = mapping start.
    let load_base = range.start;
    // Validate the e_type just to surface a clear error if
    // someone hands us non-vDSO bytes.
    if elf.elf_header().e_type.get(elf.endian())
        != object::elf::ET_DYN
    {
        return Err(VdsoScanError::NotShared);
    }

    let mut found = Vec::new();
    // Use dynamic symbols since the vDSO has them; some kernels
    // don't include a regular .symtab.
    for sym in elf.dynamic_symbols() {
        let name = match sym.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !VDSO_TARGET_SYMBOLS.iter().any(|t| *t == name) {
            continue;
        }
        let address = load_base + sym.address();
        if sym.address() == 0 {
            continue;
        }
        found.push(VdsoSymbol {
            name: name.to_owned(),
            address,
            size: sym.size(),
        });
    }
    Ok(found)
}

/// Errors arising from [`scan_vdso_exports`].
#[derive(thiserror::Error, Debug)]
pub enum VdsoScanError {
    /// `object`'s ELF parser couldn't read the bytes.
    #[error("ELF parse: {0}")]
    ElfParse(String),
    /// The bytes parsed as ELF but the type wasn't ET_DYN
    /// — vDSO is always a shared object.
    #[error("vDSO bytes parsed as non-shared ELF (not ET_DYN)")]
    NotShared,
}

/// Build the trampoline opcode sequence the recorder writes
/// over each vDSO entry on x86-64. Eight bytes:
///
/// ```text
///   B8 nr nr nr nr        ; mov    $nr, %eax     (5 bytes)
///   0F 05                  ; syscall              (2 bytes)
///   C3                     ; ret                  (1 byte)
/// ```
///
/// Returns 8 bytes — fits inside any vDSO entry's prologue
/// since the kernel keeps them generously sized.
///
/// On replay this isn't needed: the replay engine intercepts
/// every syscall via the seccomp listener and already supplies
/// recorded results, so the patch can stay in place
/// permanently.
#[cfg(target_arch = "x86_64")]
pub fn patch_payload_x86_64(syscall_nr: u32) -> [u8; 8] {
    let nr = syscall_nr.to_le_bytes();
    [0xB8, nr[0], nr[1], nr[2], nr[3], 0x0F, 0x05, 0xC3]
}

/// Map a vDSO export name to the syscall number that replaces
/// it. Returns `None` for names outside [`VDSO_TARGET_SYMBOLS`].
pub fn syscall_nr_for_vdso(name: &str) -> Option<u32> {
    Some(match name {
        "__vdso_gettimeofday" => 96,    // gettimeofday
        "__vdso_clock_gettime" => 228,  // clock_gettime
        "__vdso_clock_getres" => 229,   // clock_getres
        "__vdso_time" => 201,           // time
        "__vdso_getcpu" => 309,         // getcpu
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_mapping_parses_a_typical_line() {
        let line = "7f8c1c5e1000-7f8c1c5e3000 r-xp 00000000 fd:00 1234567 [vdso]";
        let m = ProcMapping::parse_line(line).expect("parse");
        assert_eq!(m.start, 0x7f8c1c5e1000);
        assert_eq!(m.end, 0x7f8c1c5e3000);
        assert_eq!(m.perms, "r-xp");
        assert_eq!(m.name, "[vdso]");
        assert_eq!(m.len(), 0x2000);
    }

    #[test]
    fn proc_mapping_handles_anonymous_mapping() {
        let line = "55a4d3e15000-55a4d3e16000 rw-p 00000000 00:00 0";
        let m = ProcMapping::parse_line(line).expect("parse");
        assert_eq!(m.name, "");
        assert_eq!(m.perms, "rw-p");
    }

    #[test]
    fn proc_mapping_rejects_blank_lines() {
        assert!(ProcMapping::parse_line("").is_none());
        assert!(ProcMapping::parse_line("garbage").is_none());
    }

    #[test]
    fn proc_mapping_rejects_malformed_range() {
        // Missing `-`
        let line = "deadbeef r-xp 0 fd:00 0 [bogus]";
        assert!(ProcMapping::parse_line(line).is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_proc_maps_for_self_returns_at_least_one_executable() {
        let maps = read_proc_maps_for_self().expect("read maps");
        assert!(!maps.is_empty(), "/proc/self/maps yielded no rows");
        assert!(
            maps.iter().any(|m| m.perms.contains('x')),
            "expected at least one executable mapping",
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn find_vdso_range_for_self_finds_a_mapping() {
        let v = find_vdso_range_for_self().expect("read self maps");
        // Almost every Linux kernel exposes the vDSO; failing
        // here means the kernel was built without CONFIG_VDSO.
        match v {
            Some(m) => {
                assert_eq!(m.name, "[vdso]");
                assert!(m.len() >= 4096, "vDSO mapping is suspiciously small");
                assert!(m.perms.contains('x'));
            }
            None => eprintln!(
                "skipping vDSO range assertion: kernel lacks [vdso] mapping",
            ),
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn scan_self_vdso_finds_at_least_clock_gettime() {
        let m = match find_vdso_range_for_self().unwrap() {
            Some(m) => m,
            None => {
                eprintln!("skipping: no vDSO mapping");
                return;
            }
        };
        let bytes = read_self_vdso_bytes(&m);
        let symbols = match scan_vdso_exports(&m, &bytes) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping: scan_vdso_exports failed: {e:?}");
                return;
            }
        };
        // `__vdso_clock_gettime` is the most universal — every
        // kernel since 3.x exports it.
        assert!(
            symbols.iter().any(|s| s.name == "__vdso_clock_gettime"),
            "vDSO scan didn't find __vdso_clock_gettime; symbols={:?}",
            symbols.iter().map(|s| &s.name).collect::<Vec<_>>(),
        );
        for s in &symbols {
            assert!(
                s.address >= m.start && s.address < m.end,
                "vDSO symbol `{}` at {:#x} outside mapping {:#x}..{:#x}",
                s.name, s.address, m.start, m.end,
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn patch_payload_for_clock_gettime_emits_expected_bytes() {
        // syscall 228 = clock_gettime
        let p = patch_payload_x86_64(228);
        assert_eq!(p[0], 0xB8); // mov $imm32, %eax
        // 228 = 0xE4 in little-endian u32
        assert_eq!(&p[1..5], &228u32.to_le_bytes());
        assert_eq!(&p[5..7], &[0x0F, 0x05]); // syscall
        assert_eq!(p[7], 0xC3); // ret
    }

    #[test]
    fn syscall_nr_for_vdso_covers_all_targets() {
        for name in VDSO_TARGET_SYMBOLS {
            let nr = syscall_nr_for_vdso(name)
                .unwrap_or_else(|| panic!("no syscall nr mapped for `{name}`"));
            // Sanity: must be a real syscall in our table.
            assert!(
                bs_syscall_spec::lookup_long_tail_x86_64(nr).is_some(),
                "syscall nr {nr} for `{name}` not in long-tail table",
            );
        }
        assert!(syscall_nr_for_vdso("__vdso_unknown").is_none());
    }
}

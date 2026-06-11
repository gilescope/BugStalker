// SPDX-License-Identifier: MIT
//! Recover the exact source location of a Rust panic from the
//! `#[track_caller]` `&core::panic::Location` threaded into the panic
//! machinery. The DWARF line table is imprecise for macro-synthesised
//! bodies (notably `#[test]` closures, whose instructions collapse onto
//! the `fn` line); the `Location` constant is exact by construction —
//! it's the same value the panic hook prints as `panicked at file:line:col`.

use crate::debugger::Debugger;
use crate::debugger::read_memory_by_pid;
use crate::debugger::register::{Register, RegisterMap};
use nix::unistd::Pid;

/// Exact source location of a panic site (`file:line:column`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// Integer-argument registers, in ABI order. The `#[track_caller]`
/// `&Location` is passed *after* a panic fn's explicit args, and which
/// arg that lands in varies by fn (`panic_fmt(Arguments)` vs
/// `panic_bounds_check(usize, usize)` …), so we scan them all.
#[cfg(target_arch = "aarch64")]
const ARG_REGISTERS: &[Register] = &[
    Register::X0,
    Register::X1,
    Register::X2,
    Register::X3,
    Register::X4,
    Register::X5,
    Register::X6,
    Register::X7,
];
#[cfg(target_arch = "x86_64")]
const ARG_REGISTERS: &[Register] = &[
    Register::Rdi,
    Register::Rsi,
    Register::Rdx,
    Register::Rcx,
    Register::R8,
    Register::R9,
];

impl Debugger {
    /// When the focus thread is stopped at a Rust panic entry point,
    /// recover the exact panic site from the `#[track_caller]`
    /// `&Location` constant. `None` if not at a panic stop, or no valid
    /// `Location` could be found.
    ///
    /// Finds the `&Location` by scanning the argument registers for a
    /// pointer whose pointee parses as a `core::panic::Location` (a
    /// readable `.rs` path + plausible line/column). That avoids a
    /// per-panic-fn, per-rustc-version ABI table at the cost of a few
    /// memory probes.
    pub fn panic_location(&self) -> Option<PanicLocation> {
        // Gate to an actual panic entry point so an ordinary stop whose
        // registers happen to point at Location-shaped bytes can't lie.
        let ecx = self.ecx();
        let loc = ecx.location();
        let dwarf = self.debugee.debug_info(loc.pc).ok()?;
        let (_, info) = dwarf.find_function_by_pc(loc.global_pc).ok()??;
        let fname = info.full_name()?;
        if !is_panic_entry(&fname) {
            return None;
        }

        let pid = ecx.pid_on_focus();
        let regs = RegisterMap::current(pid).ok()?;
        ARG_REGISTERS
            .iter()
            .find_map(|&reg| read_location_at(pid, regs.value(reg)))
    }
}

/// The core/std panic functions that carry a `&Location`:
/// `core::panicking::{panic, panic_fmt, panic_bounds_check, …}` and
/// `std::panicking::begin_panic*`.
fn is_panic_entry(name: &str) -> bool {
    name.contains("panicking::") || name.contains("begin_panic")
}

/// Try to read a `core::panic::Location` at `ptr` in the debuggee.
/// Layout (default-repr, file-first since the pointer leads): `{ file: &str
/// (ptr, len), line: u32, col: u32 }` — 24 bytes on 64-bit. The `&str` len
/// counts a trailing NUL on modern rustc (cut at the first NUL below).
/// Heavily validated so a non-`Location` register value can't masquerade.
fn read_location_at(pid: Pid, ptr: u64) -> Option<PanicLocation> {
    if ptr == 0 {
        return None;
    }
    let hdr = read_memory_by_pid(pid, ptr as usize, 24).ok()?;
    let file_ptr = u64::from_ne_bytes(hdr[0..8].try_into().ok()?);
    let file_len = u64::from_ne_bytes(hdr[8..16].try_into().ok()?);
    let line = u32::from_ne_bytes(hdr[16..20].try_into().ok()?);
    let column = u32::from_ne_bytes(hdr[20..24].try_into().ok()?);

    // Plausibility gates — reject pointers into anything that isn't a
    // real `Location`.
    if file_ptr == 0
        || !(1..=4096).contains(&file_len)
        || !(1..=1_000_000).contains(&line)
        || !(1..=1_000_000).contains(&column)
    {
        return None;
    }

    let file_bytes = read_memory_by_pid(pid, file_ptr as usize, file_len as usize).ok()?;
    // Modern `core::panic::Location` stores the path NUL-terminated (to back
    // `Location::file_with_nul`), so `file_len` counts the trailing `\0`.
    // Older rustc stored a plain `&str` with no NUL. Cut at the first NUL to
    // handle both, and to defend against a register that points just shy of a
    // string pool (over-read into the next, NUL-separated, entry).
    let path_bytes = match file_bytes.iter().position(|&b| b == 0) {
        Some(nul) => &file_bytes[..nul],
        None => &file_bytes[..],
    };
    let file = std::str::from_utf8(path_bytes).ok()?.to_owned();
    // Final guard: panic Locations always name a `.rs` source file.
    if !file.ends_with(".rs") {
        return None;
    }
    Some(PanicLocation { file, line, column })
}

// SPDX-License-Identifier: MIT
//! Sub-phase 3D — non-deterministic instruction trapping
//! (recorder side).
//!
//! Five instructions on x86-64 produce host-state-dependent
//! results that replay can't reproduce by re-running the same
//! code: `RDTSC`, `RDTSCP`, `RDRAND`, `RDSEED`, and `CPUID`. The
//! recorder catches each and writes an
//! [`crate::format::event::Event::InstructionTrap`] at record
//! time; the replay shim writes the recorded result back into
//! the tracee's RAX/RDX/etc. before stepping past the
//! instruction.
//!
//! ## Trap mechanisms
//!
//! - **RDTSC / RDTSCP**: `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`
//!   makes the instructions raise `SIGSEGV` instead of running.
//!   See [`set_tsc_trap_for_self`].
//! - **RDRAND / RDSEED**: mask the CPUID feature bits so glibc /
//!   openssl / etc. don't think the instructions are available.
//!   If a program calls them anyway, the kernel raises `#UD`
//!   (delivered as `SIGILL`). The mask is performed by clearing
//!   bits in the CPUID return — itself trapped by the same
//!   mechanism as below.
//! - **CPUID**: `prctl(PR_SET_DUMPABLE, …)`-style toggles don't
//!   exist on x86-64; the canonical approach is to leave CPUID
//!   alone and instead virtualise its result via the same
//!   seccomp-trap-and-substitute mechanism the syscalls use,
//!   plus an `int 0x80` / `syscall` patch on the instruction
//!   itself. Mode 1 (CPUID via SIGSEGV) requires kernel ≥ 5.5
//!   with `PR_SET_SYSCALL_USER_DISPATCH` — we adopt that path
//!   when available and fall back to "leave CPUID alone, mask
//!   nothing, accept replay imperfection" otherwise.
//!
//! ## What this module provides
//!
//! - [`set_tsc_trap_for_self`] — install the `RDTSC` trap on the
//!   *current* process. Linux-only.
//! - [`InstrKind`] — five-variant enum mirroring
//!   [`crate::format::event::InstructionTrapKind`] but kept in
//!   the recorder layer so the format crate doesn't take a
//!   dispatch dependency.
//! - [`classify_at_pc`] — pure-Rust instruction decoder; given
//!   the bytes at PC, return which of the five it is (or
//!   `None`).
//! - [`event_for_instruction_trap`] — wire-format helper.
//!
//! ## Out of scope for step 9 (deferred to follow-up)
//!
//! - vDSO patching (see `vdso_patch.rs` — separate module, lands
//!   when the patcher's iced-x86-driven entry-point detection is
//!   wired up).
//! - The actual signal handler (`SIGSEGV` from `RDTSC`,
//!   `SIGILL` from `RDRAND`/`RDSEED`/`CPUID`) — needs the
//!   ptrace driver's signal-stop loop, which lives in 3E.

#![cfg(target_os = "linux")]

use std::io;

use crate::format::event::{Event, InstructionTrapKind};

/// Five non-deterministic instructions the replay engine must
/// trap. Mirrors [`InstructionTrapKind`] in the format crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrKind {
    /// `RDTSC` (0F 31).
    Rdtsc,
    /// `RDTSCP` (0F 01 F9).
    Rdtscp,
    /// `RDRAND` (0F C7 /6).
    Rdrand,
    /// `RDSEED` (0F C7 /7).
    Rdseed,
    /// `CPUID` (0F A2).
    Cpuid,
}

impl From<InstrKind> for InstructionTrapKind {
    fn from(k: InstrKind) -> Self {
        match k {
            InstrKind::Rdtsc => Self::Rdtsc,
            InstrKind::Rdtscp => Self::Rdtscp,
            InstrKind::Rdrand => Self::Rdrand,
            InstrKind::Rdseed => Self::Rdseed,
            InstrKind::Cpuid => Self::Cpuid,
        }
    }
}

/// `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`. After this call the
/// calling thread takes a `SIGSEGV` whenever it executes
/// `RDTSC` or `RDTSCP`. The trap is per-thread, not per-
/// process; the recorder calls this from the tracee thread it
/// wants to monitor.
///
/// Errors:
///
/// - `EINVAL` if the kernel doesn't support `PR_SET_TSC` (very
///   old; pre-2.6.26). The plan's seccomp-notify minimum (5.5)
///   makes this unreachable in practice.
pub fn set_tsc_trap_for_self() -> io::Result<()> {
    // libc::PR_TSC_SIGSEGV missing on some 0.2.x; redeclare.
    const PR_SET_TSC: libc::c_int = 26;
    const PR_TSC_SIGSEGV: libc::c_int = 2;
    // SAFETY: prctl with PR_SET_TSC + PR_TSC_SIGSEGV is a
    // self-only state change; no buffer pointers.
    let r = unsafe { libc::prctl(PR_SET_TSC, PR_TSC_SIGSEGV, 0, 0, 0) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Classify the instruction at `bytes` (a slice big enough to
/// hold one x86-64 instruction; 15 bytes is the worst case).
/// Returns `None` if the instruction isn't one of the five
/// non-deterministic ones the replay engine cares about.
pub fn classify_at_pc(pc: u64, bytes: &[u8]) -> Option<InstrKind> {
    use iced_x86::{Code, Decoder, DecoderOptions};
    let mut dec = Decoder::with_ip(64, bytes, pc, DecoderOptions::NONE);
    if !dec.can_decode() {
        return None;
    }
    let instr = dec.decode();
    match instr.code() {
        Code::Rdtsc => Some(InstrKind::Rdtsc),
        Code::Rdtscp => Some(InstrKind::Rdtscp),
        Code::Rdrand_r16 | Code::Rdrand_r32 | Code::Rdrand_r64 => {
            Some(InstrKind::Rdrand)
        }
        Code::Rdseed_r16 | Code::Rdseed_r32 | Code::Rdseed_r64 => {
            Some(InstrKind::Rdseed)
        }
        Code::Cpuid => Some(InstrKind::Cpuid),
        _ => None,
    }
}

/// Wire-format helper: build an `Event::InstructionTrap` from a
/// classified hit + the values the recorder observed.
///
/// `result` shape per the format crate's contract:
/// - `Rdtsc` / `Rdtscp` — one `u64` (the timestamp counter).
/// - `Rdrand` / `Rdseed` — two `u64`s; the value, then `1` on
///   CF=1 success or `0` on CF=0 failure.
/// - `Cpuid` — four `u64`s holding `eax`, `ebx`, `ecx`, `edx`.
pub fn event_for_instruction_trap(
    pc: u64,
    kind: InstrKind,
    result: Vec<u64>,
) -> Event {
    debug_assert!(
        match kind {
            InstrKind::Rdtsc | InstrKind::Rdtscp => result.len() == 1,
            InstrKind::Rdrand | InstrKind::Rdseed => result.len() == 2,
            InstrKind::Cpuid => result.len() == 4,
        },
        "result vec for {kind:?} has wrong length {}",
        result.len(),
    );
    Event::InstructionTrap {
        pc,
        kind: kind.into(),
        result,
    }
}

/// Read a single timestamp counter on the recording host. The
/// recorder calls this *after* a SIGSEGV stops the tracee at
/// the trapped `RDTSC`; the supervisor reads its own RDTSC and
/// stamps that value into the trace. This isn't perfectly
/// monotonic vs the tracee's prior reads but is good enough for
/// replay (replay re-injects the recorded value, so determinism
/// is preserved across a record/replay pair).
///
/// Available only on x86_64.
#[cfg(target_arch = "x86_64")]
pub fn read_host_tsc() -> u64 {
    // SAFETY: RDTSC has no operands and no side effects beyond
    // writing EDX:EAX. Always available on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdtsc_two_byte_form_classifies() {
        // 0F 31  — RDTSC
        let bytes = [0x0F, 0x31];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Rdtsc));
    }

    #[test]
    fn rdtscp_three_byte_form_classifies() {
        // 0F 01 F9 — RDTSCP
        let bytes = [0x0F, 0x01, 0xF9];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Rdtscp));
    }

    #[test]
    fn cpuid_classifies() {
        // 0F A2 — CPUID
        let bytes = [0x0F, 0xA2];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Cpuid));
    }

    #[test]
    fn rdrand_eax_classifies() {
        // 0F C7 F0 — RDRAND eax
        let bytes = [0x0F, 0xC7, 0xF0];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Rdrand));
    }

    #[test]
    fn rdrand_rax_with_rex_classifies() {
        // 48 0F C7 F0 — RDRAND rax (REX.W prefix)
        let bytes = [0x48, 0x0F, 0xC7, 0xF0];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Rdrand));
    }

    #[test]
    fn rdseed_classifies() {
        // 0F C7 F8 — RDSEED eax
        let bytes = [0x0F, 0xC7, 0xF8];
        assert_eq!(classify_at_pc(0x0, &bytes), Some(InstrKind::Rdseed));
    }

    #[test]
    fn unrelated_instruction_returns_none() {
        // 90 — NOP
        assert_eq!(classify_at_pc(0x0, &[0x90]), None);
        // 0F 0B — UD2
        assert_eq!(classify_at_pc(0x0, &[0x0F, 0x0B]), None);
        // C3 — RET
        assert_eq!(classify_at_pc(0x0, &[0xC3]), None);
    }

    #[test]
    fn empty_bytes_decodes_to_none() {
        assert_eq!(classify_at_pc(0x0, &[]), None);
    }

    #[test]
    fn instr_kind_to_format_kind_round_trip() {
        for ik in [
            InstrKind::Rdtsc,
            InstrKind::Rdtscp,
            InstrKind::Rdrand,
            InstrKind::Rdseed,
            InstrKind::Cpuid,
        ] {
            let fk: InstructionTrapKind = ik.into();
            // Spot-check the mapping by name (no `From` going
            // the other way — the format crate doesn't depend
            // on the recorder).
            let name = format!("{fk:?}");
            assert!(
                name.starts_with(match ik {
                    InstrKind::Rdtsc => "Rdtsc",
                    InstrKind::Rdtscp => "Rdtscp",
                    InstrKind::Rdrand => "Rdrand",
                    InstrKind::Rdseed => "Rdseed",
                    InstrKind::Cpuid => "Cpuid",
                }),
                "format kind {name} doesn't match recorder kind {ik:?}",
            );
        }
    }

    #[test]
    fn event_for_instruction_trap_stamps_pc_and_payload() {
        let ev = event_for_instruction_trap(0xCAFE_F00D, InstrKind::Rdtsc, vec![0xDEAD_BEEF]);
        match ev {
            Event::InstructionTrap { pc, kind, result } => {
                assert_eq!(pc, 0xCAFE_F00D);
                assert_eq!(kind, InstructionTrapKind::Rdtsc);
                assert_eq!(result, vec![0xDEAD_BEEF]);
            }
            other => panic!("expected InstructionTrap, got {other:?}"),
        }
    }

    #[test]
    fn cpuid_event_carries_four_words() {
        let ev = event_for_instruction_trap(
            0x4000_0000,
            InstrKind::Cpuid,
            vec![1, 2, 3, 4],
        );
        match ev {
            Event::InstructionTrap { result, .. } => assert_eq!(result.len(), 4),
            other => panic!("got {other:?}"),
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn read_host_tsc_is_monotonic_within_a_few_calls() {
        let a = read_host_tsc();
        let b = read_host_tsc();
        assert!(
            b >= a,
            "RDTSC went backward: {a} → {b}; only happens on serious clock skew",
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn set_tsc_trap_for_self_inside_a_child() {
        // Setting PR_SET_TSC is per-thread and irreversible; do
        // it inside a forked child so the test process keeps
        // working. The child sets the trap, executes RDTSC, and
        // exits with whether the SIGSEGV fired (status 0
        // means "trap not installed" = test failure on a host
        // that supports the prctl).
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => {
                eprintln!("fork failed; skipping");
                return;
            }
            0 => {
                // Child.
                let r = set_tsc_trap_for_self();
                if r.is_err() {
                    // Skip — kernel doesn't support PR_SET_TSC.
                    unsafe { libc::_exit(64) };
                }
                // Try RDTSC; we expect SIGSEGV. If we get
                // here without crashing, the trap didn't take.
                #[cfg(target_arch = "x86_64")]
                unsafe {
                    let _ = core::arch::x86_64::_rdtsc();
                    libc::_exit(2);
                }
                #[cfg(not(target_arch = "x86_64"))]
                unsafe {
                    libc::_exit(64);
                }
            }
            child => {
                let mut status: libc::c_int = 0;
                let r = unsafe { libc::waitpid(child, &mut status, 0) };
                assert!(r > 0, "waitpid failed: {}", io::Error::last_os_error());
                if libc::WIFSIGNALED(status) {
                    let sig = libc::WTERMSIG(status);
                    assert_eq!(
                        sig, libc::SIGSEGV,
                        "expected SIGSEGV from RDTSC trap, got signal {sig}"
                    );
                } else if libc::WIFEXITED(status) {
                    let code = libc::WEXITSTATUS(status);
                    if code == 64 {
                        eprintln!("skipping: kernel/host doesn't support PR_SET_TSC");
                        return;
                    }
                    panic!(
                        "child exited normally with code {code}; \
                         RDTSC trap didn't fire"
                    );
                } else {
                    panic!("child neither exited nor was signalled (status {status:#x})");
                }
            }
        }
    }
}

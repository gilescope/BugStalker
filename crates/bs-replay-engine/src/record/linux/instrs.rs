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

/// `arch_prctl(ARCH_SET_CPUID, 0)`. After this call the
/// calling thread takes a `SIGSEGV` whenever it executes
/// `CPUID`. The trap is per-thread; the recorder calls this
/// from the tracee thread it wants to monitor.
///
/// Implication: libc / openssl / etc. will see the trap on
/// startup CPUID probes and either crash or fall back to
/// non-CPUID-dependent code paths. A future enhancement
/// would have the recorder respond to each trap with a
/// synthetic CPUID return that masks RDRAND/RDSEED feature
/// bits while preserving the rest. For now it's an opt-in
/// flag; users with libcs that probe CPUID at startup
/// shouldn't enable it.
///
/// Linux ≥ 4.12, x86-64 only — `arch_prctl` is an x86-specific
/// syscall. On non-x86 builds the function is a no-op that
/// returns `Ok(())` so callers don't need their own arch
/// branches.
#[cfg(target_arch = "x86_64")]
pub fn set_cpuid_disabled_for_self() -> io::Result<()> {
    // libc 0.2 doesn't always export ARCH_SET_CPUID; spell
    // out the constant.
    const ARCH_SET_CPUID: libc::c_int = 0x1012;
    // SAFETY: arch_prctl with ARCH_SET_CPUID + arg=0 is a
    // self-only state change; no buffer pointers.
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-x86 stub — CPUID doesn't exist on aarch64; the flag is
/// silently no-op. Returns `Ok(())` so cross-arch callers
/// don't need their own branches.
#[cfg(not(target_arch = "x86_64"))]
pub fn set_cpuid_disabled_for_self() -> io::Result<()> {
    Ok(())
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
    classify_at_pc_full(pc, bytes).map(|(k, _)| k)
}

/// Like [`classify_at_pc`] but also returns the destination
/// register's stable id for `RDRAND` / `RDSEED`. Other kinds
/// return `id == 0` (their dest registers are fixed by the
/// instruction encoding).
///
/// The register id is the value from [`x86_64_register_id`]
/// — a stable mapping from the iced-x86 register name (any
/// width) to a 0..=15 family slot (`RAX`..=`R15`). Wire
/// format: encoded as the third `u64` in
/// [`crate::format::event::Event::InstructionTrap::result`]
/// for RDRAND/RDSEED. Older traces (two-element result) decode
/// to id 0 (RAX) — backwards-compatible.
pub fn classify_at_pc_full(pc: u64, bytes: &[u8]) -> Option<(InstrKind, u64)> {
    use iced_x86::{Code, Decoder, DecoderOptions};
    let mut dec = Decoder::with_ip(64, bytes, pc, DecoderOptions::NONE);
    if !dec.can_decode() {
        return None;
    }
    let instr = dec.decode();
    let kind = match instr.code() {
        Code::Rdtsc => InstrKind::Rdtsc,
        Code::Rdtscp => InstrKind::Rdtscp,
        Code::Rdrand_r16 | Code::Rdrand_r32 | Code::Rdrand_r64 => InstrKind::Rdrand,
        Code::Rdseed_r16 | Code::Rdseed_r32 | Code::Rdseed_r64 => InstrKind::Rdseed,
        Code::Cpuid => InstrKind::Cpuid,
        _ => return None,
    };
    let dest_id = match kind {
        InstrKind::Rdrand | InstrKind::Rdseed => x86_64_register_id(instr.op0_register()),
        _ => 0,
    };
    Some((kind, dest_id))
}

/// Map an `iced_x86::Register` to a stable 0..=15 family id.
/// All width-aliases of the same physical register map to the
/// same id (`RAX`/`EAX`/`AX`/`AL`/`AH` → 0).
///
/// Anything outside the 16-register x86-64 GP set returns
/// `RAX`'s id (0) as a fall-back so wire format never carries
/// an out-of-range id.
pub fn x86_64_register_id(r: iced_x86::Register) -> u64 {
    use iced_x86::Register as R;
    match r {
        R::RAX | R::EAX | R::AX | R::AL | R::AH => 0,
        R::RCX | R::ECX | R::CX | R::CL | R::CH => 1,
        R::RDX | R::EDX | R::DX | R::DL | R::DH => 2,
        R::RBX | R::EBX | R::BX | R::BL | R::BH => 3,
        R::RSP | R::ESP | R::SP | R::SPL => 4,
        R::RBP | R::EBP | R::BP | R::BPL => 5,
        R::RSI | R::ESI | R::SI | R::SIL => 6,
        R::RDI | R::EDI | R::DI | R::DIL => 7,
        R::R8 | R::R8D | R::R8W | R::R8L => 8,
        R::R9 | R::R9D | R::R9W | R::R9L => 9,
        R::R10 | R::R10D | R::R10W | R::R10L => 10,
        R::R11 | R::R11D | R::R11W | R::R11L => 11,
        R::R12 | R::R12D | R::R12W | R::R12L => 12,
        R::R13 | R::R13D | R::R13W | R::R13L => 13,
        R::R14 | R::R14D | R::R14W | R::R14L => 14,
        R::R15 | R::R15D | R::R15W | R::R15L => 15,
        _ => 0,
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
    fn classify_at_pc_full_returns_dest_id_for_rdrand() {
        // 0F C7 F0 — RDRAND eax → dest id 0
        assert_eq!(
            classify_at_pc_full(0x0, &[0x0F, 0xC7, 0xF0]),
            Some((InstrKind::Rdrand, 0)),
        );
        // 48 0F C7 F3 — RDRAND rbx → dest id 3
        assert_eq!(
            classify_at_pc_full(0x0, &[0x48, 0x0F, 0xC7, 0xF3]),
            Some((InstrKind::Rdrand, 3)),
        );
        // 49 0F C7 F1 — RDRAND r9 → dest id 9
        assert_eq!(
            classify_at_pc_full(0x0, &[0x49, 0x0F, 0xC7, 0xF1]),
            Some((InstrKind::Rdrand, 9)),
        );
    }

    #[test]
    fn classify_at_pc_full_returns_zero_dest_for_rdtsc() {
        // RDTSC has no operand register; dest id is 0.
        assert_eq!(
            classify_at_pc_full(0x0, &[0x0F, 0x31]),
            Some((InstrKind::Rdtsc, 0)),
        );
    }

    #[test]
    fn x86_64_register_id_aliases_match() {
        use iced_x86::Register;
        // Width aliases of the same register family map to the same id.
        assert_eq!(x86_64_register_id(Register::RAX), 0);
        assert_eq!(x86_64_register_id(Register::EAX), 0);
        assert_eq!(x86_64_register_id(Register::AX), 0);
        assert_eq!(x86_64_register_id(Register::AL), 0);
        // R8..R15 family.
        assert_eq!(x86_64_register_id(Register::R8), 8);
        assert_eq!(x86_64_register_id(Register::R8D), 8);
        assert_eq!(x86_64_register_id(Register::R15), 15);
        // SIMD or non-GP regs fall back to 0.
        assert_eq!(x86_64_register_id(Register::XMM0), 0);
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

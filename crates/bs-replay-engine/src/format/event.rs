// SPDX-License-Identifier: MIT
//! Trace event vocabulary.
//!
//! Sub-phase 3A ships the *frame* — the codec, segment file format,
//! manifest. Real syscall, signal, and instruction-trap event
//! variants land in 3B / 3D / 3E.
//!
//! ## Variant-ordering rule
//!
//! `rkyv` encodes enums by variant *index*, so the on-disk
//! representation of `Marker` is the byte `0`. Adding a new variant
//! at the **end** of this enum is forward-compatible: old traces
//! that only wrote `Marker` still parse against new code because
//! the discriminant `0` still maps to `Marker`. Conversely:
//!
//! - Inserting a variant in the middle renumbers every later
//!   variant — old traces silently misinterpret as the wrong
//!   variant. This requires a `MAX_SUPPORTED_FORMAT_VERSION` bump
//!   and a reader that maps old discriminants explicitly.
//! - Removing a variant is the same — old traces would fail.
//! - Changing a variant's *fields* is a wire-format change —
//!   same bump rule.
//!
//! The defensive policy: **always append, never insert or remove**,
//! at v1 of the format. The version-rejection path (covered in
//! `version::tests`) ensures a future trace written by a newer
//! BugStalker fails fast against an older one.

use rkyv::{Archive, Deserialize, Serialize};

/// One recorded event. Public-API stability is **not** promised at
/// version 0; the variant set grows additively as the recorder
/// gains coverage.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum Event {
    /// Diagnostic placeholder. Carries an opaque tag + payload so
    /// integration tests can write a known sequence and read it
    /// back. Keep at index 0; new variants append below.
    Marker {
        /// Caller-defined tag. No semantics at this layer.
        tag: u32,
        /// Caller-defined payload word.
        data: u64,
    },
    /// One Linux syscall observation. Recorded by sub-phase 3B
    /// once seccomp-bpf user-notify is wired up; for the moment
    /// only the wire format is defined here so the rest of the
    /// pipeline can be exercised.
    ///
    /// Field meanings mirror the kernel's syscall ABI: `nr` is
    /// `__NR_*`, `args` is the six general-purpose argument
    /// registers (RDI, RSI, RDX, R10, R8, R9 on x86-64; X0–X5 on
    /// aarch64), `result` is the return value sign-extended into
    /// 64 bits (errors are negative `errno`s on the kernel ABI),
    /// `output` is any pointed-to data the kernel wrote that we
    /// have to replay back into the tracee's address space (e.g.
    /// the bytes returned by `read(fd, buf, n)`).
    Syscall {
        /// Syscall number (`__NR_*`).
        nr: u32,
        /// Six argument registers in ABI order.
        args: [u64; 6],
        /// Sign-extended return value; negative is `-errno`.
        result: i64,
        /// Bytes the kernel wrote into pointed-to buffers, in the
        /// order they appear in the call's output buffer list.
        /// Empty when the syscall has no out-pointer side effects.
        output: Vec<u8>,
    },
    /// One signal-delivery observation. Sub-phase 3E records these
    /// from `PTRACE_O_TRACESYSGOOD` + signal-delivery-stops; the
    /// wire format is here today so the cursor/replayer pipeline
    /// can be exercised against signal-bearing traces.
    ///
    /// Async signals carry the exact PC where the kernel delivered
    /// them (recovered via PMU instruction counters or PT). Sync
    /// faults (SIGSEGV from a bad load, etc.) replay implicitly
    /// because the same instructions run and re-produce the fault;
    /// for those the PC is advisory.
    ///
    /// `siginfo` is opaque platform-layout bytes — typically
    /// `siginfo_t` (128 bytes on Linux x86-64) so replay can use
    /// `PTRACE_SETSIGINFO` to deliver an identical signal. The
    /// format crate doesn't decode it; that's the recorder /
    /// replayer's contract.
    Signal {
        /// Signal number (`SIGINT`, `SIGSEGV`, etc.).
        sig_no: u32,
        /// PC at delivery time.
        pc: u64,
        /// Opaque `siginfo_t` bytes for `PTRACE_SETSIGINFO`.
        siginfo: Vec<u8>,
    },
    /// One non-deterministic-instruction trap. Sub-phase 3D
    /// captures these via `PR_SET_TSC = PR_TSC_SIGSEGV` (RDTSC*),
    /// CPUID emulation, and `#UD` traps for `RDRAND`/`RDSEED`
    /// when their CPUID feature bit is masked off at fork. Wire
    /// format only at this layer; recorder is sub-phase 3D.
    ///
    /// Single variant for all five instruction kinds — the
    /// replayer dispatches on `kind` and the format crate stays
    /// neutral. The `result` vector's per-kind shape is a
    /// recorder/replayer contract:
    ///
    /// - `Rdtsc` / `Rdtscp` — one `u64` (the timestamp counter).
    /// - `Rdrand` / `Rdseed` — *three* `u64`s; the value, then
    ///   `1` on success or `0` on the kernel's CF=0 path, then
    ///   the destination-register id (0..=15 for RAX..R15;
    ///   see `bs_replay_engine::record::linux::instrs::x86_64_register_id`).
    ///   Traces predating step 101 have only the first two
    ///   `u64`s; replay defaults the id to 0 (RAX) when the
    ///   third element is absent (backwards compat).
    /// - `Cpuid` — four `u64`s holding `eax`, `ebx`, `ecx`, `edx`.
    InstructionTrap {
        /// PC where the trap fired.
        pc: u64,
        /// Which instruction was trapped.
        kind: InstructionTrapKind,
        /// Result words the replay engine must reproduce.
        /// Per-kind shape is the recorder's contract.
        result: Vec<u64>,
    },
    /// PC marker — "the recorder observed the tracee at this PC
    /// here in the event stream". Lets Tier 1 reverse-step
    /// display `now at <file>:<line>` after the consumer
    /// resolves PC → source via DWARF (BugStalker's existing
    /// infrastructure). Variant index 4 — appended per the
    /// additive-forever rule.
    ///
    /// File / line / column resolution intentionally lives
    /// outside the trace format. The trace stores PCs (compact,
    /// fixed size, no string table needed); the debugger's
    /// existing DWARF tooling does the lookup at display time.
    /// This keeps the trace small and avoids embedding source-
    /// path strings that would bloat with every basic-block
    /// crossing.
    ///
    /// Recorder cadence: the syscall recorder emits one marker at
    /// each syscall boundary. Once Phase 6's PT trace is available
    /// on real hardware, decode can add a finer-grained PC sequence
    /// that compresses naturally.
    PcMarker {
        /// Program counter at this point in the recorded stream.
        pc: u64,
    },
}

/// Which non-deterministic instruction triggered an
/// [`Event::InstructionTrap`]. Recorders shouldn't expose any
/// other instruction here without a format-version bump — the
/// variant set is part of the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum InstructionTrapKind {
    /// `RDTSC`. Reads the host TSC into `EDX:EAX`.
    Rdtsc,
    /// `RDTSCP`. Like `RDTSC` plus reads the IA32_TSC_AUX MSR
    /// into `ECX`.
    Rdtscp,
    /// `RDRAND`. Reads a hardware-RNG word into a GP register.
    Rdrand,
    /// `RDSEED`. Like `RDRAND` but reads from the seed pool.
    Rdseed,
    /// `CPUID`. Reads CPU-feature info into `EAX:EBX:ECX:EDX`.
    Cpuid,
}

impl Event {
    /// Upper-bound estimate of this event's contribution to the
    /// segment archive in bytes. Used by the writer to decide when
    /// to rotate before compression. Slight over-estimation is
    /// safe; under-estimation is not (could overshoot the segment
    /// size cap).
    pub fn approx_archive_size(&self) -> usize {
        // rkyv enum tag overhead + alignment slack. Conservative.
        const VARIANT_OVERHEAD: usize = 16;
        match self {
            // 4-byte tag + 8-byte data + alignment.
            Self::Marker { .. } => VARIANT_OVERHEAD + 4 + 8,
            // 4-byte nr + 6×8-byte args + 8-byte result + the
            // archived Vec layout (16-byte rkyv RelPtr + len) +
            // the payload bytes themselves.
            Self::Syscall { output, .. } => VARIANT_OVERHEAD + 4 + 6 * 8 + 8 + 16 + output.len(),
            // 4-byte sig_no + 8-byte pc + Vec<u8> overhead + payload.
            Self::Signal { siginfo, .. } => VARIANT_OVERHEAD + 4 + 8 + 16 + siginfo.len(),
            // 8-byte pc + 1-byte kind discriminant + Vec<u64> overhead
            // + 8 × words.
            Self::InstructionTrap { result, .. } => {
                VARIANT_OVERHEAD + 8 + 1 + 16 + 8 * result.len()
            }
            // Single u64 + variant overhead.
            Self::PcMarker { .. } => VARIANT_OVERHEAD + 8,
        }
    }
}

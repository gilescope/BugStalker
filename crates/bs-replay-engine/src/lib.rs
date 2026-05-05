// SPDX-License-Identifier: MIT
//! BugStalker time-travel — Tier 3 deterministic record-and-replay.
//!
//! See `doc/plans/phase-5-time-travel.md` § "Tier 3 — Clean-room
//! deterministic record-and-replay" for the full architecture.
//!
//! ## How we hit near-native record speed
//!
//! gdb's `record full` single-steps every instruction; that is the
//! source of its 50–1000× slowdown. We refuse that approach. Instead
//! we let the program run at hardware speed and intercept *only* at
//! non-determinism boundaries:
//!
//! - **`seccomp-bpf` user-notify** — every syscall traps to us; the
//!   instructions between syscalls run unmodified.
//! - **`PR_SET_TSC = SIGSEGV`** — `RDTSC`/`RDTSCP` raise a fault we
//!   catch; no per-instruction trap needed.
//! - **`CPUID` mask + signal trap** — `RDRAND`/`RDSEED` advertised
//!   as unsupported so glibc/openssl/etc. don't use them; if they
//!   do, we trap.
//! - **vDSO entry-point patching** — `gettimeofday` and friends go
//!   through the kernel and trip seccomp like any other syscall.
//! - **`userfaultfd` on `io_uring` SQ/CQ pages** — only those pages
//!   page-fault into us; the rest of the address space runs free.
//! - **single-CPU pinning** — multi-thread context switches happen
//!   only at syscalls and timer interrupts, both already recorded.
//! - **PT-assisted instruction counts** (sub-phase 3H) — Phase 6's
//!   PT trace replaces software single-step counters between context
//!   switches, dropping multi-thread overhead from ~5× to ~2×.
//!
//! Single-thread overhead targets ~1.5–2×, in line with `rr`. The
//! defining principle: never single-step the program.
//!
//! This crate has zero C dependencies. Compression uses pure-Rust
//! `ruzstd` at `CompressionLevel::Fastest`; the on-disk format is
//! RFC 8478-compliant zstd, so future encoder swaps are drop-in.

// Lint at `warn` rather than `deny`: rkyv's `Archive` derive emits
// generated structs whose fields we can't ourselves document, and
// the codebase has a lot of stubs while the sub-phases mature. The
// signal stays loud without blocking builds.
#![warn(missing_docs)]

pub mod driver;
pub mod format;
pub mod record;
pub mod replay;

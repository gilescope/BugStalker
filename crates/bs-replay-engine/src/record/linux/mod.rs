// SPDX-License-Identifier: MIT
//! Linux record back-end.
//!
//! Module map (mirrors `doc/plans/phase-5-time-travel.md` § "Architecture"):
//!
//! - `seccomp` — `seccomp-bpf` user-notify install (3B).
//! - `ptrace_driver` — tracee control loop (3B).
//! - `instrs` — `RDRAND`/`RDTSC`/`CPUID` trapping (3D).
//! - `vdso_patch` — vDSO entry-point patching (3D).
//! - `signals` — async/sync signal record (3E).
//! - `thread_sched` — single-CPU serialisation (3F).
//! - `pt_assist` — Phase 6 PT trace consumption (3H).
//!
//! All deferred to follow-up batches.

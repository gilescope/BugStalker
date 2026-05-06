// SPDX-License-Identifier: MIT
//! Linux record back-end.
//!
//! Module map (mirrors `doc/plans/phase-5-time-travel.md` § "Architecture"):
//!
//! - `seccomp` — `seccomp-bpf` user-notify install (3B step 6).
//! - `ptrace_driver` — tracee control loop + Event::Syscall emit
//!   (3B step 7).
//! - `instrs` — `RDRAND`/`RDTSC`/`CPUID` trapping (3D).
//! - `vdso_patch` — vDSO entry-point patching (3D).
//! - `signals` — async/sync signal record (3E).
//! - `thread_sched` — single-CPU serialisation (3F).
//! - `pt_assist` — Phase 6 PT trace consumption (3H).

pub mod instrs;
pub mod ptrace_driver;
pub mod seccomp;
pub mod signals;

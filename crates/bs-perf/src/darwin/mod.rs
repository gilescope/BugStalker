// SPDX-License-Identifier: MIT
//! Darwin perf back-end (Phase 6, Tier 2).
//!
//! No PMU sampling here yet — that's the deferred `kperf` tier
//! (Phase 6 plan §"Darwin back-end"). What lives in this module is
//! the coarse fallback the same plan section explicitly endorses:
//!
//! > If kperf bindings fail, use `proc_pid_rusage()` to report
//! > whole-process cycle counts at each stop. Useful for "this run
//! > cost X cycles" but not heat-mapping. Surfaced as the per-stop
//! > summary line; no gutter.
//!
//! Apple's `proc_pid_rusage` doesn't actually expose raw cycles
//! until `rusage_info_v6` (macOS 12+), and even then `ri_cycles` is
//! per-cluster and not guaranteed present. We pin to v4 because it
//! is universally available on every macOS BugStalker supports and
//! reports `ri_user_time` + `ri_system_time` in nanoseconds — that
//! gives the per-stop summary a real CPU-time figure (carried in
//! [`StopSummary::run_cpu_time_ns`](crate::aggregator::StopSummary::run_cpu_time_ns))
//! instead of the previous zero, with no platform-version risk.
//!
//! The gutter heat-map stays empty on macOS until the `kperf` tier
//! lands. The status-bar item, however, becomes real.

pub mod kperf;
pub mod rusage;
pub mod symbols;

pub use kperf::{probe_kperf, KperfMonitor, KperfStatus, KperfUnavailableReason};
pub use rusage::ProcessSnapshot;

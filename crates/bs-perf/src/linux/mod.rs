// SPDX-License-Identifier: MIT
//! Linux back-end. Module map (mirrors
//! doc/plans/phase-6-perf-overlay.md § "Linux back-end"):
//!
//! - `perf_event` — `perf_event_open(2)` wrapper + cycles event
//!   builder. (this step)
//! - `ring` — mmap'd ring buffer drain. (next step)
//! - `aggregator` — PC → cycle-count map. (subsequent step)
//!
//! ## Architectural rule
//!
//! All instrumentation is kernel-side. The debuggee runs naked;
//! the kernel writes IP samples into a kernel ring buffer; we
//! read them on our own thread. Nothing in this module touches
//! the debuggee's address space.

// Module is gated at the parent (`pub mod linux;` in src/lib.rs is
// `#[cfg(target_os = "linux")]`). No inner attribute here.

pub mod perf_event;

pub use perf_event::{open_cycles_for_pid, PerfMonitor};

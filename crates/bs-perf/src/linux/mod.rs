// SPDX-License-Identifier: MIT
//! Linux back-end. Module map (mirrors
//! doc/plans/phase-6-perf-overlay.md § "Linux back-end"):
//!
//! - `perf_event` — `perf_event_open(2)` wrapper + cycles event
//!   builder. (this step)
//! - `ring` — mmap'd ring buffer drain.
//! - `intel_pt` — pure-Rust Intel PT capability and raw capture plumbing.
//!   The feature-gated decode boundary lives in `crate::pt_decode`.
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

pub mod intel_pt;
pub mod perf_event;
pub mod ring;

pub use intel_pt::{
    DEFAULT_INTEL_PT_AUX_BYTES, DEFAULT_INTEL_PT_AUX_WATERMARK_BYTES, DEFAULT_INTEL_PT_DATA_BYTES,
    IntelPtAuxBuffer, IntelPtAuxDrainStats, IntelPtCapture, IntelPtCaptureDrain, IntelPtMonitor,
    IntelPtProbe, IntelPtStatus, IntelPtUnavailableReason, build_intel_pt_attr,
    default_intel_pt_data_pages, open_intel_pt_for_pid, open_intel_pt_for_pid_with_pmu_type,
    probe_intel_pt, probe_intel_pt_at,
};
pub use perf_event::{PerfMonitor, open_cycles_for_pid, open_instructions_for_pid};
pub use ring::{
    DEFAULT_RING_DATA_PAGES, DrainStats, PerfAuxLayout, PerfAuxSnapshot, PerfRecord,
    PerfRingBuffer, PerfSample,
};

// SPDX-License-Identifier: MIT
//! BugStalker performance overlay (Phase 6).
//!
//! Cycles + IP sampling via `perf_event_open(2)`. The CPU's PMU
//! writes IP samples to a kernel ring buffer the debuggee cannot
//! perceive; a separate debugger thread drains the ring on its
//! own timer; decoding into `(file, line)` heat-maps happens
//! at-stop. Architectural rule from the plan:
//!
//! > Hardware writes; debuggee runs naked; decoding waits for
//! > the stop.
//!
//! ## Scope of this step (step 114)
//!
//! Crate scaffold + the cycles event opener. Subsequent steps
//! layer the ring buffer drain (step 115), DWARF crossover
//! (step 116), aggregator + UI (step 117–118), DAP overlay
//! request (step 119).
//!
//! ## Tier coverage
//!
//! | Tier                  | Status         | Notes                                                                |
//! | --------------------- | -------------- | -------------------------------------------------------------------- |
//! | Linux x86_64          | scaffold       | cycles+IP via `PERF_TYPE_HARDWARE`                                   |
//! | Linux aarch64         | scaffold       | identical event setup; PMU differs                                   |
//! | Darwin (cycles+IP)    | unavailable    | private kperf API not yet bound; M4-or-higher silicon floor          |
//! | Intel PT              | future feature | precise x86 tier under `intel-pt` cargo feature; pulls in `libipt`   |
//! | ARM CoreSight ETM     | future feature | precise aarch64-linux tier under `coresight-etm`; needs              |
//! |                       |                | `CONFIG_CORESIGHT` + board DTS support; OpenCSD-equivalent decoder   |
//! | Apple Processor Trace | future feature | precise Darwin tier (Instruments 16.3 / M4+); ~1% overhead;          |
//! |                       |                | the Apple Silicon analog of Intel PT — exact flame graph             |
//! | ARM SPE               | not started    | statistical aarch64 tier — `arm-spe` feature, `perf_event_open` PMU  |
//!
//! ## Public surface
//!
//! - [`PerfMonitor`] — per-PID handle. On Linux it owns a perf
//!   event fd; on Darwin it's a unit struct with all methods
//!   returning [`PerfError::Unsupported`].
//! - [`open_cycles_for_pid`] — opens a cycles+IP sampling event
//!   on a target PID. Linux only.
//! - [`PerfError`] — the single error type.
//!
//! ## Pure-Rust policy
//!
//! Default build is zero C dependencies. `perf-event-open-sys`
//! provides typed bindings to the perf_event_open(2) syscall +
//! UAPI structs; it's pure Rust. `libc` for the ioctl/close
//! primitives we still need around the fd.
//!
//! The `intel-pt` cargo feature (future step) opts into libipt
//! for the precise tier — that's the single explicit step that
//! introduces C linkage, exactly as the plan calls out in
//! §"Pure-Rust policy". Default `cargo build` stays C-dep-free.

#![warn(missing_docs)]

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(not(target_os = "linux"))]
pub mod stub;

// Re-export the top-level `open_cycles_for_pid` so callers don't
// have to spell the platform module path. The Linux impl returns
// a real PerfMonitor; off-Linux it's a stub that returns
// `Err(PerfError::Unsupported)`.
#[cfg(target_os = "linux")]
pub use linux::{open_cycles_for_pid, PerfMonitor};
#[cfg(not(target_os = "linux"))]
pub use stub::{open_cycles_for_pid, PerfMonitor};

/// Single error type for the crate. Layered: kernel/syscall
/// failures are wrapped; cross-platform unavailability has its
/// own variant so callers can detect it without parsing strings.
#[derive(thiserror::Error, Debug)]
pub enum PerfError {
    /// `perf_event_open(2)` rejected the attribute set or
    /// refused for permission reasons. The most common cause on
    /// distro kernels is `kernel.perf_event_paranoid >= 2`
    /// without `CAP_SYS_ADMIN`; the message includes the raw
    /// errno so support can disambiguate.
    #[error("perf_event_open: {0}")]
    Open(std::io::Error),

    /// `ioctl(PERF_EVENT_IOC_ENABLE/DISABLE/RESET)` failed. Rare
    /// in practice — usually means the fd was closed underneath.
    #[error("perf event ioctl: {0}")]
    Ioctl(std::io::Error),

    /// The target platform doesn't have a perf monitor implementation.
    /// Currently: anything that isn't Linux. Callers display a
    /// graceful "perf overlay unavailable on this platform" and
    /// continue — the rest of the debugger keeps working.
    #[error("perf overlay is unavailable on this platform")]
    Unsupported,
}

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
//! ## Scope of this step (steps 114-133)
//!
//! Crate scaffold, cycles event opener, and the mmap ring buffer
//! drain/parser, plus a DWARF `.debug_line` resolver from sampled
//! PCs to source frames, the in-memory aggregation model, and
//! UI-facing overlay view models, and Rust-side DAP shapes for the
//! overlay request. The root debugger integration owns live DAP
//! wiring; this crate also exposes a pure-Rust Intel PT capability
//! probe so the later precise tier can fail with concrete host
//! diagnostics before full capture/decode lands; the Intel PT
//! event attribute builder records the future capture syscall
//! contract, and `IntelPtMonitor` opens the disabled PT event fd,
//! maps the PT data ring/AUX buffer, drains AUX bytes, groups those
//! resources behind a raw capture owner, and provides the first
//! feature-gated libipt instruction decode boundary plus decoded-PT
//! source attribution into the shared aggregator. The root debugger
//! integration now also snapshots decode image sections for future
//! live PT decode and has an opt-in PT live collector path.
//!
//! ## Tier coverage
//!
//! | Tier                  | Status         | Notes                                                                |
//! | --------------------- | -------------- | -------------------------------------------------------------------- |
//! | Linux x86_64          | scaffold       | cycles+IP via `PERF_TYPE_HARDWARE`                                   |
//! | Linux aarch64         | scaffold       | identical event setup; PMU differs                                   |
//! | Darwin (rusage Tier 2)| scaffold       | per-stop user+sys CPU time via `proc_pid_rusage(RUSAGE_INFO_V4)`     |
//! | Darwin (kperf Tier 1) | deferred       | private kperf API bind via `dlsym`; cycles+IP heat-map               |
//! | Intel PT              | decode boundary | raw capture plus feature-gated libipt instruction decode              |
//! | AMD Processor Trace   | future feature | Phase 11 provider; AMD LBR Stack first, packet PT if exposed         |
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
//! Intel PT packet decoding opts into libipt behind the `intel-pt`
//! cargo feature — that's the single explicit step that introduces C
//! linkage, exactly as the plan calls out in §"Pure-Rust policy". The
//! default Intel PT probe/capture boundary remains pure Rust, so
//! default `cargo build` stays C-dep-free.

#![warn(missing_docs)]

pub mod aggregator;
pub mod dap;
#[cfg(target_os = "macos")]
pub mod darwin;
pub mod decoder;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod overlay;
pub mod pt_decode;

#[cfg(not(target_os = "linux"))]
pub mod stub;

// Re-export the top-level `open_cycles_for_pid` so callers don't
// have to spell the platform module path. The Linux impl returns
// a real PerfMonitor; off-Linux it's a stub that returns
// `Err(PerfError::Unsupported)`.
#[cfg(target_os = "linux")]
pub use linux::{PerfMonitor, open_cycles_for_pid};
#[cfg(not(target_os = "linux"))]
pub use stub::{PerfMonitor, open_cycles_for_pid};

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

    /// Duplicating the perf event fd for an owned ring mapping failed.
    #[error("perf event fd dup: {0}")]
    FdDup(std::io::Error),

    /// `mmap(2)` or `munmap(2)` failed for the perf ring buffer.
    #[error("perf ring mmap: {0}")]
    Mmap(std::io::Error),

    /// The requested ring size cannot be represented as a perf
    /// data ring. The kernel expects a non-zero, power-of-two
    /// number of data pages after the metadata page.
    #[error("invalid perf ring data page count {pages}; expected a non-zero power of two")]
    InvalidRingPages {
        /// Requested data-page count.
        pages: usize,
    },

    /// The requested AUX trace buffer size cannot be represented as a
    /// perf AUX ring. The kernel expects a non-zero, power-of-two
    /// byte size that is also an exact number of pages.
    #[error(
        "invalid perf AUX buffer size {bytes}; expected a non-zero power-of-two multiple of page size {page_size}"
    )]
    InvalidAuxBufferSize {
        /// Requested AUX byte size.
        bytes: usize,
        /// System page size observed while validating the request.
        page_size: usize,
    },

    /// The kernel advanced `data_head` beyond the unread ring
    /// capacity. We cannot recover record boundaries, so the
    /// caller should report lost samples and reset the tail.
    #[error("perf ring overrun: unread bytes {available} exceed ring size {data_size}")]
    RingOverrun {
        /// Bytes between `data_tail` and `data_head`.
        available: u64,
        /// Data-ring byte size.
        data_size: u64,
    },

    /// The kernel advanced `aux_head` beyond the unread AUX trace
    /// capacity. The captured packet stream for that window is no
    /// longer contiguous.
    #[error("perf AUX overrun: unread bytes {available} exceed AUX size {aux_size}")]
    AuxOverrun {
        /// Bytes between `aux_tail` and `aux_head`.
        available: u64,
        /// AUX ring byte size.
        aux_size: u64,
    },

    /// A perf ring record was structurally invalid.
    #[error("malformed perf ring record: {0}")]
    MalformedRecord(&'static str),

    /// Reading an object/debug-info file failed.
    #[error("debug-info file I/O: {0}")]
    DebugInfoIo(std::io::Error),

    /// Parsing an object/debug-info file failed.
    #[error("object file parse: {0}")]
    Object(#[from] object::Error),

    /// DWARF decoding failed.
    #[error("DWARF line decode: {0}")]
    Dwarf(#[from] gimli::Error),

    /// Intel PT packet/instruction decoding failed.
    #[error("Intel PT decode: {0}")]
    PtDecode(String),

    /// The target platform doesn't have a perf monitor implementation.
    /// Currently: anything that isn't Linux. Callers display a
    /// graceful "perf overlay unavailable on this platform" and
    /// continue — the rest of the debugger keeps working.
    #[error("perf overlay is unavailable on this platform")]
    Unsupported,
}

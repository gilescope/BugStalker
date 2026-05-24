// SPDX-License-Identifier: MIT
//! `perf_event_open(2)` wrapper for cycles + IP sampling.
//!
//! The opened fd is owned by [`PerfMonitor`]. Drop closes the fd
//! via `close(2)`; `ioctl(PERF_EVENT_IOC_DISABLE)` is run first so
//! the kernel stops accruing samples for the dying event.
//!
//! ## Sampling configuration
//!
//! From the plan (§"Cycles + IP sampling (universal tier)"):
//!
//! ```text
//!   attr.type           = PERF_TYPE_HARDWARE
//!   attr.config         = PERF_COUNT_HW_CPU_CYCLES
//!   attr.sample_freq    = 1000          ; tunable, default 1 kHz
//!   attr.freq           = 1
//!   attr.sample_type    = IP | TID | TIME | CPU
//!   attr.exclude_kernel = 1
//!   attr.exclude_hv     = 1
//!   attr.precise_ip     = 2             ; PEBS for low-skid
//!   attr.disabled       = 1             ; enable explicitly via ioctl
//! ```
//!
//! `precise_ip = 2` requests PEBS on Intel. Some kernels/PMUs reject
//! that request with `EINVAL`/`EOPNOTSUPP` instead of degrading it, so
//! the opener retries with `precise_ip = 0` and accepts skid rather
//! than disabling the whole cycles tier.
//!
//! ## What's NOT in this step
//!
//! - DWARF crossover: PC → (file, line). Wired in step 116.
//! - Aggregator + UI. Wired in step 117–118.

// Cfg gate inherited from src/linux/mod.rs — no inner attribute
// here.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use perf_event_open_sys::bindings::perf_event_attr;
use perf_event_open_sys::{bindings, ioctls, perf_event_open};

use super::ring::PerfRingBuffer;
use crate::PerfError;

/// Default sampling frequency in Hz. The plan's recommendation;
/// tunable via the future `BS_PERF_DRAIN_HZ` env var (added in a
/// later step).
pub const DEFAULT_SAMPLE_FREQ_HZ: u64 = 1000;

/// Per-PID perf event handle. Owns the perf_event fd; holds it
/// disabled until `enable()` is called. `Drop` runs the disable
/// ioctl + closes the fd.
#[derive(Debug)]
pub struct PerfMonitor {
    fd: OwnedFd,
    /// PID we opened the event against. Captured for diagnostics
    /// — the kernel doesn't echo it back.
    pid: i32,
    /// Sample frequency stamped into the attribute, in Hz. Same
    /// rationale: kernel doesn't expose it for readback.
    sample_freq_hz: u64,
    /// `precise_ip` value accepted by the kernel. `2` is the
    /// preferred low-skid mode; `0` is the portable fallback.
    precise_ip: u64,
}

impl PerfMonitor {
    /// PID this monitor was opened against.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Sample frequency that was requested at open time, in Hz.
    pub fn sample_freq_hz(&self) -> u64 {
        self.sample_freq_hz
    }

    /// `precise_ip` level accepted by the kernel for this monitor.
    pub fn precise_ip(&self) -> u64 {
        self.precise_ip
    }

    /// Borrow the raw perf event fd. Used by the future ring
    /// buffer module to mmap it, and by tests that want to
    /// confirm the fd is open.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Map this event's perf data ring. `data_pages` is the
    /// number of data pages after the kernel metadata page; it
    /// must be non-zero and a power of two. The returned ring
    /// owns a duplicate fd, so it remains valid even if this
    /// monitor is dropped first.
    pub fn mmap_ring(&self, data_pages: usize) -> Result<PerfRingBuffer, PerfError> {
        PerfRingBuffer::map(self.fd.as_raw_fd(), data_pages)
    }

    /// `ioctl(PERF_EVENT_IOC_ENABLE)`. The event was opened in
    /// `disabled = 1` state; sampling doesn't begin until this
    /// returns.
    pub fn enable(&mut self) -> Result<(), PerfError> {
        // SAFETY: ioctl on a fd we own; second arg is 0 (no
        // extra data).
        let r = unsafe { ioctls::ENABLE(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// `ioctl(PERF_EVENT_IOC_DISABLE)`. Sampling pauses; the fd
    /// stays valid and can be re-enabled.
    pub fn disable(&mut self) -> Result<(), PerfError> {
        // SAFETY: same as enable.
        let r = unsafe { ioctls::DISABLE(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// `ioctl(PERF_EVENT_IOC_RESET)`. Zero the accumulated
    /// counters without affecting enable/disable state.
    pub fn reset(&mut self) -> Result<(), PerfError> {
        // SAFETY: same as enable.
        let r = unsafe { ioctls::RESET(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Read the cumulative counter value via `read(fd, ...)`. With
    /// no `read_format` flags set (our default) the kernel returns
    /// a single 8-byte u64. Works on sampling and counter-only
    /// events alike — the read surface is the same.
    pub fn read_count(&self) -> Result<u64, PerfError> {
        let mut buf = [0u8; 8];
        // SAFETY: read into an 8-byte stack buffer with the fd we own.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        if n != buf.len() as isize {
            return Err(PerfError::MalformedRecord(
                "perf_event counter read returned short buffer",
            ));
        }
        Ok(u64::from_ne_bytes(buf))
    }
}

impl Drop for PerfMonitor {
    fn drop(&mut self) {
        // Best-effort disable so the kernel stops accruing
        // samples; ignore the result (the fd will be closed
        // immediately after regardless).
        // SAFETY: ioctl on a still-open fd we own.
        unsafe {
            let _ = ioctls::DISABLE(self.fd.as_raw_fd(), 0);
        }
        // OwnedFd's Drop closes via close(2).
    }
}

/// Open a cycles + IP sampling event on `pid` with the default
/// frequency ([`DEFAULT_SAMPLE_FREQ_HZ`]). The event is in
/// `disabled` state on return — call [`PerfMonitor::enable`]
/// before the debuggee starts running.
///
/// `pid == -1` opens a CPU-wide event (requires CAP_SYS_ADMIN
/// or `kernel.perf_event_paranoid <= -1`). `pid >= 0` is per-
/// process (or per-thread, depending on what `pid` actually
/// names — kernel's "pid" argument doubles as a tid).
///
/// Errors:
///
/// - `EACCES` — `kernel.perf_event_paranoid >= 2` and we don't
///   have CAP_SYS_ADMIN. The most common failure on hardened
///   distros. `perf_event_paranoid` ≤ 1 (or ≤ 2 with the right
///   capability) is required.
/// - `EPERM` — same family; capability fix.
/// - `ENOSYS` — kernel without CONFIG_PERF_EVENTS. Rare.
/// - `ESRCH` — `pid` doesn't exist or we can't see it.
///
/// Permission failures bubble up as [`PerfError::Open`] with
/// the OS error preserved; the caller decides whether to
/// downgrade the feature or surface a clear "needs sudo" message.
pub fn open_cycles_for_pid(pid: i32) -> Result<PerfMonitor, PerfError> {
    open_cycles_for_pid_with_freq(pid, DEFAULT_SAMPLE_FREQ_HZ)
}

/// Variant of [`open_cycles_for_pid`] taking an explicit
/// frequency. Used by tests to validate the freq round-trip;
/// also handy for future tuning UX (`BS_PERF_DRAIN_HZ` env var,
/// landed in a later step).
pub fn open_cycles_for_pid_with_freq(
    pid: i32,
    sample_freq_hz: u64,
) -> Result<PerfMonitor, PerfError> {
    let mut attr = build_cycles_attr(sample_freq_hz);
    match open_cycles_with_attr(pid, sample_freq_hz, &mut attr) {
        Ok(monitor) => Ok(monitor),
        Err(e) if should_retry_without_precise_ip(&e, attr.precise_ip()) => {
            attr.set_precise_ip(0);
            open_cycles_with_attr(pid, sample_freq_hz, &mut attr).map_err(PerfError::Open)
        }
        Err(e) => Err(PerfError::Open(e)),
    }
}

fn open_cycles_with_attr(
    pid: i32,
    sample_freq_hz: u64,
    attr: &mut perf_event_attr,
) -> Result<PerfMonitor, io::Error> {
    let precise_ip = attr.precise_ip();
    // CPU = -1: any CPU (the kernel routes per-task events to
    // wherever the task runs). group_fd = -1: this event is its
    // own group leader. flags = 0: no FD_CLOEXEC, FD_NO_GROUP,
    // PID_CGROUP, etc.
    let raw = unsafe {
        perf_event_open(
            attr as *mut perf_event_attr,
            pid,
            /* cpu */ -1,
            /* group_fd */ -1,
            /* flags */ 0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: positive return value from perf_event_open is a
    // valid file descriptor we now own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(PerfMonitor {
        fd,
        pid,
        sample_freq_hz,
        precise_ip,
    })
}

fn should_retry_without_precise_ip(error: &io::Error, precise_ip: u64) -> bool {
    precise_ip != 0 && matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP))
}

/// Open a pure-counter instructions-retired event on `pid`. No
/// sampling, no ring buffer — just an 8-byte cumulative count we
/// read at each stop. Pairs with the cycles+IP sampler to give the
/// DAP body an `runInstructions` figure (and, divided by cycles,
/// IPC).
///
/// Same permission story as [`open_cycles_for_pid`]: `EACCES` /
/// `EPERM` on `perf_event_paranoid >= 2` without `CAP_SYS_ADMIN`;
/// callers downgrade the feature rather than fail the session.
pub fn open_instructions_for_pid(pid: i32) -> Result<PerfMonitor, PerfError> {
    let mut attr = build_instructions_attr();
    // SAFETY: standard perf_event_open boilerplate; same shape as
    // open_cycles_for_pid above.
    let raw = unsafe {
        perf_event_open(
            &mut attr as *mut perf_event_attr,
            pid,
            /* cpu */ -1,
            /* group_fd */ -1,
            /* flags */ 0,
        )
    };
    if raw < 0 {
        return Err(PerfError::Open(io::Error::last_os_error()));
    }
    // SAFETY: positive raw fd is owned now.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(PerfMonitor {
        fd,
        pid,
        sample_freq_hz: 0,
        precise_ip: 0,
    })
}

/// Build a `perf_event_attr` for the instructions-retired counter.
/// No sampling — `sample_freq` / `sample_type` left zero. Disabled
/// by default; the caller `enable()`s before the run.
pub fn build_instructions_attr() -> perf_event_attr {
    // SAFETY: zeroed perf_event_attr is the documented blank slate.
    let mut attr: perf_event_attr = unsafe { core::mem::zeroed() };
    attr.size = core::mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = bindings::PERF_TYPE_HARDWARE;
    attr.config = u64::from(bindings::PERF_COUNT_HW_INSTRUCTIONS);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_disabled(1);
    // `inherit = 0` (default) — the kernel doesn't auto-create
    // child events on fork/clone. Matches the cycles sampler.
    attr
}

/// Build the `perf_event_attr` for cycles + IP sampling. Public
/// so the next step's mmap-ring tests can stamp the same
/// attribute without re-deriving the bit-fiddling.
pub fn build_cycles_attr(sample_freq_hz: u64) -> perf_event_attr {
    // SAFETY: zeroed perf_event_attr is the documented "blank
    // slate" — every field is either 0 or has 0 as its default.
    let mut attr: perf_event_attr = unsafe { core::mem::zeroed() };
    attr.size = core::mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = bindings::PERF_TYPE_HARDWARE;
    attr.config = u64::from(bindings::PERF_COUNT_HW_CPU_CYCLES);
    attr.__bindgen_anon_1.sample_freq = sample_freq_hz;
    attr.sample_type = u64::from(bindings::PERF_SAMPLE_IP)
        | u64::from(bindings::PERF_SAMPLE_TID)
        | u64::from(bindings::PERF_SAMPLE_TIME)
        | u64::from(bindings::PERF_SAMPLE_CPU);
    // freq=1 + sample_freq=N: kernel auto-tunes the period to
    // achieve N Hz. freq=0 + sample_period=N: fixed period.
    attr.set_freq(1);
    // Don't sample kernel addresses; the user's source-line
    // attribution can't reach them anyway.
    attr.set_exclude_kernel(1);
    // Skip hypervisor samples on bare metal hosts (Phase 6's
    // primary target); on guest VMs without HV access this is
    // a no-op.
    attr.set_exclude_hv(1);
    // PEBS-quality precision on Intel; kernel auto-degrades on
    // CPUs without PEBS (older Intel, AMD pre-IBS, aarch64).
    // Plan asks for `2`. If a host's PMU genuinely can't deliver
    // any precision, perf_event_open returns EOPNOTSUPP and the
    // caller falls back to opening with `precise_ip = 0`.
    attr.set_precise_ip(2);
    // Open disabled; explicit enable() begins counting.
    attr.set_disabled(1);
    attr
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a cycles attribute and confirms the bit-fields
    /// landed where the plan says they should. No syscall —
    /// portable across hosts that lack PERF_EVENTS.
    #[test]
    fn cycles_attr_has_documented_field_values() {
        let attr = build_cycles_attr(1000);
        assert_eq!(attr.type_, bindings::PERF_TYPE_HARDWARE);
        assert_eq!(attr.config, u64::from(bindings::PERF_COUNT_HW_CPU_CYCLES));
        // sample_freq lives in __bindgen_anon_1 (a union with
        // sample_period); freq() bit selects the freq
        // interpretation.
        unsafe {
            assert_eq!(attr.__bindgen_anon_1.sample_freq, 1000);
        }
        assert_eq!(attr.freq(), 1);
        assert_eq!(attr.exclude_kernel(), 1);
        assert_eq!(attr.exclude_hv(), 1);
        assert_eq!(attr.precise_ip(), 2);
        assert_eq!(attr.disabled(), 1);
        // sample_type bit-field: IP | TID | TIME | CPU.
        let want = u64::from(bindings::PERF_SAMPLE_IP)
            | u64::from(bindings::PERF_SAMPLE_TID)
            | u64::from(bindings::PERF_SAMPLE_TIME)
            | u64::from(bindings::PERF_SAMPLE_CPU);
        assert_eq!(attr.sample_type, want);
    }

    #[test]
    fn cycles_attr_size_is_sizeof_struct() {
        let attr = build_cycles_attr(1000);
        assert_eq!(attr.size, core::mem::size_of::<perf_event_attr>() as u32);
    }

    /// Open + enable + disable + drop — all on `pid = getpid()`
    /// (self-monitoring; always allowed). Skips on hosts where
    /// `perf_event_open` is denied or absent (`EPERM`/`EACCES`/
    /// `ENOSYS`). The test asserts nothing about *samples* yet
    /// — that needs the ring buffer module from the next step.
    #[test]
    fn open_self_round_trips_through_enable_disable() {
        let pid = unsafe { libc::getpid() };
        let m = match open_cycles_for_pid(pid) {
            Ok(m) => m,
            Err(PerfError::Open(e)) => {
                let raw = e.raw_os_error();
                if matches!(
                    raw,
                    Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EOPNOTSUPP),
                ) {
                    eprintln!("skipping: perf_event_open denied — {e}");
                    return;
                }
                panic!("open failed: {e:?}");
            }
            Err(e) => panic!("open failed: {e:?}"),
        };
        assert_eq!(m.pid(), pid);
        assert_eq!(m.sample_freq_hz(), DEFAULT_SAMPLE_FREQ_HZ);
        assert!(matches!(m.precise_ip(), 0 | 2));
        assert!(m.raw_fd() >= 0);
        let mut m = m;
        m.enable().expect("enable");
        m.disable().expect("disable");
        m.reset().expect("reset");
    }

    /// `precise_ip = 2` is the requested setting; very old PMUs
    /// reject it. We document this expectation in a separate
    /// test that probes the *attribute* (already verified) — the
    /// runtime fall-back to lower precision is the kernel's job.
    #[test]
    fn cycles_attr_requests_pebs_precision() {
        let attr = build_cycles_attr(1000);
        assert_eq!(attr.precise_ip(), 2);
    }

    /// Instructions attribute: HW type, INSTRUCTIONS config, no
    /// sampling fields, disabled until enable().
    #[test]
    fn instructions_attr_is_pure_counter() {
        let attr = build_instructions_attr();
        assert_eq!(attr.type_, bindings::PERF_TYPE_HARDWARE);
        assert_eq!(attr.config, u64::from(bindings::PERF_COUNT_HW_INSTRUCTIONS));
        assert_eq!(attr.sample_type, 0);
        // sample_freq lives in the union; freq() bit unset means
        // the field is interpreted as sample_period — which we
        // also leave zero.
        assert_eq!(attr.freq(), 0);
        unsafe {
            assert_eq!(attr.__bindgen_anon_1.sample_freq, 0);
        }
        assert_eq!(attr.exclude_kernel(), 1);
        assert_eq!(attr.exclude_hv(), 1);
        assert_eq!(attr.disabled(), 1);
        assert_eq!(attr.size, core::mem::size_of::<perf_event_attr>() as u32);
    }

    /// Open the instructions counter on our own pid, run a hot
    /// loop, read the count. Should be far above zero. Skips on
    /// hosts where `perf_event_open` is denied.
    #[test]
    fn instructions_counter_advances_under_hot_loop() {
        let pid = unsafe { libc::getpid() };
        let mut m = match open_instructions_for_pid(pid) {
            Ok(m) => m,
            Err(PerfError::Open(e)) => {
                let raw = e.raw_os_error();
                if matches!(
                    raw,
                    Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EOPNOTSUPP),
                ) {
                    eprintln!("skipping: perf_event_open denied — {e}");
                    return;
                }
                panic!("open failed: {e:?}");
            }
            Err(e) => panic!("open failed: {e:?}"),
        };
        m.reset().expect("reset");
        m.enable().expect("enable");
        // Burn a known amount of work. Volatile sink stops the
        // optimiser from collapsing the loop into a constant.
        let mut sink: u64 = 0;
        for i in 0_u64..1_000_000 {
            sink = sink.wrapping_add(i.wrapping_mul(7));
        }
        std::hint::black_box(sink);
        m.disable().expect("disable");
        let count = m.read_count().expect("read_count");
        assert!(
            count > 500_000,
            "1M-iteration loop should retire ≫500k instructions, got {count}"
        );
    }
}

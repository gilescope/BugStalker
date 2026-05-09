// SPDX-License-Identifier: MIT
//! Intel Processor Trace capability and capture plumbing.
//!
//! This module discovers whether the Linux kernel exposes the
//! `intel_pt` PMU, opens disabled PT perf events, and owns the data
//! ring/AUX mmap boundary and raw capture window. Packet decode
//! remains later Phase 6 work.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::ptr;

use perf_event_open_sys::bindings::{self, perf_event_attr};
use perf_event_open_sys::{ioctls, perf_event_open};

use super::ring::{DrainStats, PerfRecord, PerfRingBuffer};
use crate::PerfError;

const INTEL_PT_TYPE_PATH: &str = "bus/event_source/devices/intel_pt/type";
const PERF_EVENT_PARANOID_PATH: &str = "sys/kernel/perf_event_paranoid";

/// Default Intel PT AUX trace buffer size.
pub const DEFAULT_INTEL_PT_AUX_BYTES: usize = 64 * 1024 * 1024;

/// Planned Intel PT perf data-ring size. The data ring carries AUX
/// metadata records; the PT packet stream itself lives in the AUX ring.
pub const DEFAULT_INTEL_PT_DATA_BYTES: usize = 4 * 1024 * 1024;

/// Wake the collector after this much AUX data is ready.
pub const DEFAULT_INTEL_PT_AUX_WATERMARK_BYTES: u32 = 1024 * 1024;

/// Result of probing Intel PT support on a Linux host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntelPtProbe {
    /// High-level support status.
    pub status: IntelPtStatus,
    /// Kernel PMU type id read from
    /// `/sys/bus/event_source/devices/intel_pt/type`, when present.
    pub pmu_type: Option<u32>,
    /// Current `/proc/sys/kernel/perf_event_paranoid` value, when it
    /// could be read and parsed.
    pub perf_event_paranoid: Option<i32>,
}

impl IntelPtProbe {
    /// True when the PMU exists and the paranoid setting should allow
    /// opening PT events without extra capabilities.
    pub fn usable_without_elevated_caps(&self) -> bool {
        self.status == IntelPtStatus::Available
    }

    fn unavailable(reason: IntelPtUnavailableReason) -> Self {
        Self {
            status: IntelPtStatus::Unavailable(reason),
            pmu_type: None,
            perf_event_paranoid: None,
        }
    }
}

/// High-level Intel PT support state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntelPtStatus {
    /// Intel PT PMU exists and the kernel paranoid setting is `<= 1`.
    Available,
    /// Intel PT PMU exists, but opening PT events will likely need
    /// `CAP_SYS_ADMIN` or a lower `perf_event_paranoid` setting.
    PermissionLikelyRequired {
        /// Observed `perf_event_paranoid` value.
        perf_event_paranoid: i32,
    },
    /// Intel PT cannot be used on this host.
    Unavailable(IntelPtUnavailableReason),
}

/// Concrete reason Intel PT is not currently available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntelPtUnavailableReason {
    /// The target architecture cannot expose Intel PT.
    UnsupportedArchitecture {
        /// Rust target architecture name.
        arch: &'static str,
    },
    /// The kernel did not expose `/sys/.../intel_pt/type`.
    PmuMissing {
        /// Path that was expected to contain the PT PMU type.
        path: String,
    },
    /// The PMU type file existed but did not contain a `u32`.
    InvalidPmuType {
        /// Path that contained invalid data.
        path: String,
        /// Trimmed file contents.
        value: String,
    },
    /// The paranoid sysctl existed but did not contain an `i32`.
    InvalidPerfEventParanoid {
        /// Path that contained invalid data.
        path: String,
        /// Trimmed file contents.
        value: String,
    },
    /// Reading a required probe file failed.
    Io {
        /// Path that failed.
        path: String,
        /// I/O error rendered at the probe boundary.
        error: String,
    },
}

/// Open Intel PT event handle.
///
/// This owns the perf event fd. Data-ring and AUX mappings own
/// duplicated fds so their drop order is not fragile.
#[derive(Debug)]
pub struct IntelPtMonitor {
    fd: OwnedFd,
    pid: i32,
    pmu_type: u32,
}

/// Owned Intel PT capture resources for one PID/TID.
#[derive(Debug)]
pub struct IntelPtCapture {
    monitor: IntelPtMonitor,
    data_ring: PerfRingBuffer,
    aux_buffer: IntelPtAuxBuffer,
}

/// Owned mmap of an Intel PT AUX trace buffer.
#[derive(Debug)]
pub struct IntelPtAuxBuffer {
    ptr: *mut u8,
    len: usize,
    _event_fd: OwnedFd,
}

/// Summary from one AUX drain pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IntelPtAuxDrainStats {
    /// Number of bytes copied from the AUX ring.
    pub bytes: usize,
    /// AUX tail cursor before this drain.
    pub tail: u64,
    /// AUX head cursor consumed by this drain.
    pub head: u64,
}

/// Raw bytes and metadata drained from one Intel PT capture window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IntelPtCaptureDrain {
    /// Metadata records drained from the perf data ring.
    pub data_records: Vec<PerfRecord>,
    /// Data-ring parser/loss counters.
    pub data_stats: DrainStats,
    /// Raw PT packet bytes drained from the AUX ring.
    pub aux_bytes: Vec<u8>,
    /// AUX cursor/byte counters.
    pub aux_stats: IntelPtAuxDrainStats,
}

impl IntelPtCapture {
    /// Open a PT capture with the default Phase 6 buffer sizes.
    pub fn open_for_pid(pid: i32) -> Result<Self, PerfError> {
        let probe = probe_intel_pt();
        let Some(pmu_type) = probe.pmu_type else {
            return Err(PerfError::Unsupported);
        };
        Self::open_for_pid_with_pmu_type(
            pid,
            pmu_type,
            default_intel_pt_data_pages()?,
            DEFAULT_INTEL_PT_AUX_BYTES,
        )
    }

    /// Open a PT capture with explicit PMU and buffer sizing.
    ///
    /// `data_pages` is the number of data-ring pages after the kernel
    /// metadata page. `aux_bytes` is the AUX ring size.
    pub fn open_for_pid_with_pmu_type(
        pid: i32,
        pmu_type: u32,
        data_pages: usize,
        aux_bytes: usize,
    ) -> Result<Self, PerfError> {
        let monitor = open_intel_pt_for_pid_with_pmu_type(pid, pmu_type)?;
        let mut data_ring = monitor.mmap_data_ring(data_pages)?;
        let aux_buffer = monitor.mmap_aux_buffer(&mut data_ring, aux_bytes)?;
        Ok(Self {
            monitor,
            data_ring,
            aux_buffer,
        })
    }

    /// PID/TID this PT capture is attached to.
    pub fn pid(&self) -> i32 {
        self.monitor.pid()
    }

    /// Kernel PMU type used for the PT event.
    pub fn pmu_type(&self) -> u32 {
        self.monitor.pmu_type()
    }

    /// Start a capture window.
    pub fn start(&mut self) -> Result<(), PerfError> {
        self.monitor.reset()?;
        self.monitor.enable()
    }

    /// Stop capture and drain both metadata and AUX bytes.
    pub fn stop_and_drain(&mut self) -> Result<IntelPtCaptureDrain, PerfError> {
        self.monitor.disable()?;
        self.drain()
    }

    /// Drain both metadata and AUX bytes without changing enable state.
    pub fn drain(&mut self) -> Result<IntelPtCaptureDrain, PerfError> {
        let (data_records, data_stats) = self.data_ring.drain()?;
        let (aux_bytes, aux_stats) = self.aux_buffer.drain(&mut self.data_ring)?;
        Ok(IntelPtCaptureDrain {
            data_records,
            data_stats,
            aux_bytes,
            aux_stats,
        })
    }
}

impl IntelPtMonitor {
    /// PID/TID this PT event was opened against.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Kernel PMU type used for the open call.
    pub fn pmu_type(&self) -> u32 {
        self.pmu_type
    }

    /// Borrow the raw perf event fd for future mmap/ioctl plumbing.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Map this PT event's perf data ring.
    ///
    /// The data ring carries `PERF_RECORD_AUX` and other metadata
    /// records. The PT packet bytes themselves live in a separate AUX
    /// mapping configured through [`IntelPtMonitor::mmap_aux_buffer`].
    pub fn mmap_data_ring(&self, data_pages: usize) -> Result<PerfRingBuffer, PerfError> {
        PerfRingBuffer::map(self.fd.as_raw_fd(), data_pages)
    }

    /// Map this PT event's AUX trace buffer.
    ///
    /// `data_ring` must be the data ring mapped from this same monitor;
    /// its metadata page is where the kernel publishes AUX cursors.
    pub fn mmap_aux_buffer(
        &self,
        data_ring: &mut PerfRingBuffer,
        aux_bytes: usize,
    ) -> Result<IntelPtAuxBuffer, PerfError> {
        IntelPtAuxBuffer::map(self.fd.as_raw_fd(), data_ring, aux_bytes)
    }

    /// Enable PT capture for this event.
    pub fn enable(&mut self) -> Result<(), PerfError> {
        // SAFETY: ioctl on a fd we own; second arg is 0.
        let r = unsafe { ioctls::ENABLE(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Disable PT capture for this event.
    pub fn disable(&mut self) -> Result<(), PerfError> {
        // SAFETY: ioctl on a fd we own; second arg is 0.
        let r = unsafe { ioctls::DISABLE(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Reset the PT event.
    pub fn reset(&mut self) -> Result<(), PerfError> {
        // SAFETY: ioctl on a fd we own; second arg is 0.
        let r = unsafe { ioctls::RESET(self.fd.as_raw_fd(), 0) };
        if r < 0 {
            return Err(PerfError::Ioctl(io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl IntelPtAuxBuffer {
    /// Map an AUX trace buffer for `event_fd`.
    pub fn map(
        event_fd: RawFd,
        data_ring: &mut PerfRingBuffer,
        aux_bytes: usize,
    ) -> Result<Self, PerfError> {
        let layout = data_ring.configure_aux_area(aux_bytes)?;
        let dup = dup_fd(event_fd)?;
        let offset: libc::off_t = layout
            .offset
            .try_into()
            .map_err(|_| PerfError::MalformedRecord("AUX mmap offset overflows off_t"))?;

        // SAFETY: mmap is called with a live perf event fd, shared
        // read/write mapping, page-aligned offset configured in the
        // event metadata page, and a validated non-zero AUX length.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                layout.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                dup.as_raw_fd(),
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(PerfError::Mmap(io::Error::last_os_error()));
        }

        Ok(Self {
            ptr: ptr.cast(),
            len: layout.size,
            _event_fd: dup,
        })
    }

    /// Number of bytes in the AUX ring.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the AUX ring has zero capacity.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drain all currently visible AUX bytes into an owned buffer.
    ///
    /// The debugger calls this after capture has been disabled. The
    /// returned bytes are contiguous even if the kernel ring wrapped.
    pub fn drain(
        &self,
        data_ring: &mut PerfRingBuffer,
    ) -> Result<(Vec<u8>, IntelPtAuxDrainStats), PerfError> {
        let snapshot = data_ring.aux_snapshot()?;
        if snapshot.size != self.len {
            return Err(PerfError::MalformedRecord("AUX metadata size changed"));
        }

        let available = snapshot.head.saturating_sub(snapshot.tail);
        if available > self.len as u64 {
            data_ring.write_aux_tail(snapshot.head);
            return Err(PerfError::AuxOverrun {
                available,
                aux_size: self.len as u64,
            });
        }

        let bytes = self.copy_from_aux(snapshot.tail, available as usize);
        data_ring.write_aux_tail(snapshot.head);
        Ok((
            bytes,
            IntelPtAuxDrainStats {
                bytes: available as usize,
                tail: snapshot.tail,
                head: snapshot.head,
            },
        ))
    }

    fn copy_from_aux(&self, cursor: u64, len: usize) -> Vec<u8> {
        if len == 0 {
            return Vec::new();
        }

        let start = (cursor % self.len as u64) as usize;
        let first_len = len.min(self.len - start);
        let mut out = vec![0u8; len];
        // SAFETY: start/first_len and optional wrapped remainder are
        // checked against self.len; out has exactly len bytes.
        unsafe {
            ptr::copy_nonoverlapping(self.ptr.add(start), out.as_mut_ptr(), first_len);
            if first_len < len {
                ptr::copy_nonoverlapping(
                    self.ptr,
                    out.as_mut_ptr().add(first_len),
                    len - first_len,
                );
            }
        }
        out
    }
}

impl Drop for IntelPtAuxBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr/len come from a successful AUX mmap.
        unsafe {
            let _ = libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

impl Drop for IntelPtMonitor {
    fn drop(&mut self) {
        // Best-effort disable before OwnedFd closes the event.
        // SAFETY: ioctl on a still-open fd we own.
        unsafe {
            let _ = ioctls::DISABLE(self.fd.as_raw_fd(), 0);
        }
    }
}

/// Probe Intel PT using the live host `/sys` and `/proc` trees.
pub fn probe_intel_pt() -> IntelPtProbe {
    probe_intel_pt_at(Path::new("/sys"), Path::new("/proc"))
}

/// Probe Intel PT using caller-provided sysfs/procfs roots.
///
/// The split root form keeps tests deterministic and lets future DAP
/// diagnostics use the same parser against captured host snapshots.
pub fn probe_intel_pt_at(sys_root: &Path, proc_root: &Path) -> IntelPtProbe {
    if !intel_pt_arch_supported() {
        return IntelPtProbe::unavailable(IntelPtUnavailableReason::UnsupportedArchitecture {
            arch: std::env::consts::ARCH,
        });
    }

    let pmu_type_path = sys_root.join(INTEL_PT_TYPE_PATH);
    let pmu_type = match read_trimmed(&pmu_type_path) {
        Ok(value) => match value.parse::<u32>() {
            Ok(pmu_type) => pmu_type,
            Err(_) => {
                return IntelPtProbe::unavailable(IntelPtUnavailableReason::InvalidPmuType {
                    path: display_path(&pmu_type_path),
                    value,
                });
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return IntelPtProbe::unavailable(IntelPtUnavailableReason::PmuMissing {
                path: display_path(&pmu_type_path),
            });
        }
        Err(err) => {
            return IntelPtProbe::unavailable(IntelPtUnavailableReason::Io {
                path: display_path(&pmu_type_path),
                error: err.to_string(),
            });
        }
    };

    let paranoid_path = proc_root.join(PERF_EVENT_PARANOID_PATH);
    match read_trimmed(&paranoid_path) {
        Ok(value) => match value.parse::<i32>() {
            Ok(perf_event_paranoid) if perf_event_paranoid <= 1 => IntelPtProbe {
                status: IntelPtStatus::Available,
                pmu_type: Some(pmu_type),
                perf_event_paranoid: Some(perf_event_paranoid),
            },
            Ok(perf_event_paranoid) => IntelPtProbe {
                status: IntelPtStatus::PermissionLikelyRequired {
                    perf_event_paranoid,
                },
                pmu_type: Some(pmu_type),
                perf_event_paranoid: Some(perf_event_paranoid),
            },
            Err(_) => IntelPtProbe {
                status: IntelPtStatus::Unavailable(
                    IntelPtUnavailableReason::InvalidPerfEventParanoid {
                        path: display_path(&paranoid_path),
                        value,
                    },
                ),
                pmu_type: Some(pmu_type),
                perf_event_paranoid: None,
            },
        },
        Err(err) => IntelPtProbe {
            status: IntelPtStatus::Unavailable(IntelPtUnavailableReason::Io {
                path: display_path(&paranoid_path),
                error: err.to_string(),
            }),
            pmu_type: Some(pmu_type),
            perf_event_paranoid: None,
        },
    }
}

/// Build the `perf_event_attr` used to open an Intel PT event.
///
/// Keeping the builder pure lets the DAP/UI layer validate the
/// intended kernel contract on hosts that do not expose Intel PT.
pub fn build_intel_pt_attr(pmu_type: u32) -> perf_event_attr {
    // SAFETY: zeroed perf_event_attr is the documented empty
    // attribute set; fields are filled explicitly below.
    let mut attr: perf_event_attr = unsafe { core::mem::zeroed() };
    attr.type_ = pmu_type;
    attr.size = core::mem::size_of::<perf_event_attr>() as u32;
    attr.config = 0;
    attr.sample_type = u64::from(bindings::PERF_SAMPLE_TID)
        | u64::from(bindings::PERF_SAMPLE_TIME)
        | u64::from(bindings::PERF_SAMPLE_CPU);
    attr.aux_watermark = DEFAULT_INTEL_PT_AUX_WATERMARK_BYTES;
    attr.set_disabled(1);
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_sample_id_all(1);
    attr
}

/// Open an Intel PT event on `pid` using the live host probe.
///
/// The event starts disabled. Packet decode is intentionally not part
/// of this step.
pub fn open_intel_pt_for_pid(pid: i32) -> Result<IntelPtMonitor, PerfError> {
    let probe = probe_intel_pt();
    let Some(pmu_type) = probe.pmu_type else {
        return Err(PerfError::Unsupported);
    };
    open_intel_pt_for_pid_with_pmu_type(pid, pmu_type)
}

/// Open an Intel PT event with a caller-provided PMU type.
///
/// This is useful when the probe result is already cached by the DAP
/// layer. The event starts disabled.
pub fn open_intel_pt_for_pid_with_pmu_type(
    pid: i32,
    pmu_type: u32,
) -> Result<IntelPtMonitor, PerfError> {
    let mut attr = build_intel_pt_attr(pmu_type);
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

    // SAFETY: positive return value from perf_event_open is a valid fd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(IntelPtMonitor { fd, pid, pmu_type })
}

/// Default Intel PT data-ring page count for this host page size.
pub fn default_intel_pt_data_pages() -> Result<usize, PerfError> {
    let page_size = page_size()?;
    let pages = DEFAULT_INTEL_PT_DATA_BYTES / page_size;
    if pages == 0
        || !pages.is_power_of_two()
        || pages
            .checked_mul(page_size)
            .is_none_or(|bytes| bytes != DEFAULT_INTEL_PT_DATA_BYTES)
    {
        return Err(PerfError::InvalidRingPages { pages });
    }
    Ok(pages)
}

fn dup_fd(fd: RawFd) -> Result<OwnedFd, PerfError> {
    // SAFETY: fcntl duplicates a live fd or returns -1 with errno.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(PerfError::FdDup(io::Error::last_os_error()));
    }
    // SAFETY: positive fcntl return is a newly-owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn intel_pt_arch_supported() -> bool {
    cfg!(any(target_arch = "x86", target_arch = "x86_64"))
}

fn read_trimmed(path: &Path) -> std::io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn page_size() -> Result<usize, PerfError> {
    // SAFETY: sysconf with _SC_PAGESIZE has no side effects and no
    // pointer arguments.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size <= 0 {
        return Err(PerfError::Mmap(io::Error::last_os_error()));
    }
    Ok(size as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn probe_reports_available_intel_pt() {
        let root = TestRoots::new("available");
        root.write_sys(INTEL_PT_TYPE_PATH, "9\n");
        root.write_proc(PERF_EVENT_PARANOID_PATH, "1\n");

        let probe = probe_intel_pt_at(&root.sys, &root.proc);

        if intel_pt_arch_supported() {
            assert_eq!(probe.status, IntelPtStatus::Available);
            assert_eq!(probe.pmu_type, Some(9));
            assert_eq!(probe.perf_event_paranoid, Some(1));
            assert!(probe.usable_without_elevated_caps());
        } else {
            assert!(matches!(
                probe.status,
                IntelPtStatus::Unavailable(
                    IntelPtUnavailableReason::UnsupportedArchitecture { .. }
                )
            ));
        }
    }

    #[test]
    fn probe_reports_missing_pmu() {
        let root = TestRoots::new("missing");
        root.write_proc(PERF_EVENT_PARANOID_PATH, "1\n");

        let probe = probe_intel_pt_at(&root.sys, &root.proc);

        if intel_pt_arch_supported() {
            assert!(matches!(
                probe.status,
                IntelPtStatus::Unavailable(IntelPtUnavailableReason::PmuMissing { .. })
            ));
        }
    }

    #[test]
    fn probe_reports_permission_likely_required() {
        let root = TestRoots::new("paranoid");
        root.write_sys(INTEL_PT_TYPE_PATH, "9\n");
        root.write_proc(PERF_EVENT_PARANOID_PATH, "2\n");

        let probe = probe_intel_pt_at(&root.sys, &root.proc);

        if intel_pt_arch_supported() {
            assert_eq!(
                probe.status,
                IntelPtStatus::PermissionLikelyRequired {
                    perf_event_paranoid: 2
                }
            );
            assert_eq!(probe.pmu_type, Some(9));
            assert_eq!(probe.perf_event_paranoid, Some(2));
            assert!(!probe.usable_without_elevated_caps());
        }
    }

    #[test]
    fn probe_reports_invalid_pmu_type() {
        let root = TestRoots::new("invalid-pmu");
        root.write_sys(INTEL_PT_TYPE_PATH, "not-a-number\n");
        root.write_proc(PERF_EVENT_PARANOID_PATH, "1\n");

        let probe = probe_intel_pt_at(&root.sys, &root.proc);

        if intel_pt_arch_supported() {
            assert!(matches!(
                probe.status,
                IntelPtStatus::Unavailable(IntelPtUnavailableReason::InvalidPmuType { .. })
            ));
        }
    }

    #[test]
    fn intel_pt_attr_has_documented_field_values() {
        let attr = build_intel_pt_attr(9);

        assert_eq!(attr.type_, 9);
        assert_eq!(attr.size, core::mem::size_of::<perf_event_attr>() as u32);
        assert_eq!(attr.config, 0);
        assert_eq!(attr.disabled(), 1);
        assert_eq!(attr.exclude_kernel(), 1);
        assert_eq!(attr.exclude_hv(), 1);
        assert_eq!(attr.sample_id_all(), 1);
        assert_eq!(attr.aux_watermark, DEFAULT_INTEL_PT_AUX_WATERMARK_BYTES);

        let want = u64::from(bindings::PERF_SAMPLE_TID)
            | u64::from(bindings::PERF_SAMPLE_TIME)
            | u64::from(bindings::PERF_SAMPLE_CPU);
        assert_eq!(attr.sample_type, want);
    }

    #[test]
    fn open_self_intel_pt_round_trips_when_available() {
        let probe = probe_intel_pt();
        let Some(pmu_type) = probe.pmu_type else {
            eprintln!("skipping: Intel PT PMU unavailable: {:?}", probe.status);
            return;
        };

        let pid = unsafe { libc::getpid() };
        let m = match open_intel_pt_for_pid_with_pmu_type(pid, pmu_type) {
            Ok(m) => m,
            Err(PerfError::Open(e)) => {
                let raw = e.raw_os_error();
                if matches!(
                    raw,
                    Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EOPNOTSUPP),
                ) {
                    eprintln!("skipping: Intel PT perf_event_open denied: {e}");
                    return;
                }
                panic!("open failed: {e:?}");
            }
            Err(PerfError::Unsupported) => {
                eprintln!("skipping: Intel PT unsupported");
                return;
            }
            Err(e) => panic!("open failed: {e:?}"),
        };

        assert_eq!(m.pid(), pid);
        assert_eq!(m.pmu_type(), pmu_type);
        assert!(m.raw_fd() >= 0);
        let mut m = m;
        m.enable().expect("enable");
        m.disable().expect("disable");
        m.reset().expect("reset");
    }

    #[test]
    fn mmap_self_intel_pt_data_and_aux_when_available() {
        let probe = probe_intel_pt();
        let Some(pmu_type) = probe.pmu_type else {
            eprintln!("skipping: Intel PT PMU unavailable: {:?}", probe.status);
            return;
        };

        let pid = unsafe { libc::getpid() };
        let m = match open_intel_pt_for_pid_with_pmu_type(pid, pmu_type) {
            Ok(m) => m,
            Err(PerfError::Open(e)) if skip_live_perf_errno(&e) => {
                eprintln!("skipping: Intel PT perf_event_open denied: {e}");
                return;
            }
            Err(e) => panic!("open failed: {e:?}"),
        };

        let mut data_ring = match m.mmap_data_ring(1) {
            Ok(ring) => ring,
            Err(PerfError::Mmap(e)) if skip_live_perf_errno(&e) => {
                eprintln!("skipping: Intel PT data-ring mmap denied: {e}");
                return;
            }
            Err(e) => panic!("data-ring mmap failed: {e:?}"),
        };
        let aux_bytes = data_ring.data_size();
        let aux = match m.mmap_aux_buffer(&mut data_ring, aux_bytes) {
            Ok(aux) => aux,
            Err(PerfError::Mmap(e)) if skip_live_perf_errno(&e) => {
                eprintln!("skipping: Intel PT AUX mmap denied: {e}");
                return;
            }
            Err(e) => panic!("AUX mmap failed: {e:?}"),
        };

        assert_eq!(aux.len(), aux_bytes);
        assert!(!aux.is_empty());
        let (bytes, stats) = aux.drain(&mut data_ring).expect("drain");
        assert!(bytes.is_empty());
        assert_eq!(stats.bytes, 0);
        assert_eq!(stats.head, stats.tail);
    }

    #[test]
    fn capture_self_intel_pt_start_stop_drain_when_available() {
        let probe = probe_intel_pt();
        let Some(pmu_type) = probe.pmu_type else {
            eprintln!("skipping: Intel PT PMU unavailable: {:?}", probe.status);
            return;
        };

        let pid = unsafe { libc::getpid() };
        let page_size = page_size().expect("page size");
        let mut capture =
            match IntelPtCapture::open_for_pid_with_pmu_type(pid, pmu_type, 1, page_size) {
                Ok(capture) => capture,
                Err(PerfError::Open(e) | PerfError::Mmap(e)) if skip_live_perf_errno(&e) => {
                    eprintln!("skipping: Intel PT capture setup denied: {e}");
                    return;
                }
                Err(e) => panic!("capture setup failed: {e:?}"),
            };

        assert_eq!(capture.pid(), pid);
        assert_eq!(capture.pmu_type(), pmu_type);
        match capture.start() {
            Ok(()) => {}
            Err(PerfError::Ioctl(e)) if skip_live_perf_errno(&e) => {
                eprintln!("skipping: Intel PT capture start denied: {e}");
                return;
            }
            Err(e) => panic!("capture start failed: {e:?}"),
        }
        std::hint::black_box(1u64.wrapping_add(2));
        let drained = match capture.stop_and_drain() {
            Ok(drained) => drained,
            Err(PerfError::Ioctl(e)) if skip_live_perf_errno(&e) => {
                eprintln!("skipping: Intel PT capture stop denied: {e}");
                return;
            }
            Err(PerfError::AuxOverrun { .. }) => {
                eprintln!("skipping: tiny test AUX buffer overran");
                return;
            }
            Err(e) => panic!("capture drain failed: {e:?}"),
        };

        assert!(drained.aux_bytes.len() <= page_size);
        assert_eq!(drained.aux_stats.bytes, drained.aux_bytes.len());
    }

    #[test]
    fn default_data_pages_matches_default_data_bytes() {
        let pages = default_intel_pt_data_pages().expect("default pages");
        let page_size = page_size().expect("page size");
        assert!(pages.is_power_of_two());
        assert_eq!(pages * page_size, DEFAULT_INTEL_PT_DATA_BYTES);
    }

    fn skip_live_perf_errno(e: &std::io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(
                libc::EPERM
                    | libc::EACCES
                    | libc::ENOSYS
                    | libc::EOPNOTSUPP
                    | libc::EINVAL
                    | libc::ENOMEM
            ),
        )
    }

    struct TestRoots {
        root: PathBuf,
        sys: PathBuf,
        proc: PathBuf,
    }

    impl TestRoots {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "bs-perf-intel-pt-{name}-{}-{nanos}",
                std::process::id()
            ));
            let sys = root.join("sys");
            let proc = root.join("proc");
            fs::create_dir_all(&sys).expect("create sys root");
            fs::create_dir_all(&proc).expect("create proc root");
            Self { root, sys, proc }
        }

        fn write_sys(&self, rel: &str, contents: &str) {
            write_fixture(&self.sys.join(rel), contents);
        }

        fn write_proc(&self, rel: &str, contents: &str) {
            write_fixture(&self.proc.join(rel), contents);
        }
    }

    impl Drop for TestRoots {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_fixture(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("fixture has parent"))
            .expect("create fixture parent");
        fs::write(path, contents).expect("write fixture");
    }
}

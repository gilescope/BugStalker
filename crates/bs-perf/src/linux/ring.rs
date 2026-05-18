// SPDX-License-Identifier: MIT
//! mmap'd `perf_event_open(2)` ring buffer drain.
//!
//! The kernel writes records into a power-of-two data ring after
//! one metadata page. We read `data_head`, copy complete records
//! out without holding references into the mmap, then publish the
//! consumed offset by writing `data_tail`.
//!
//! This step deliberately parses only the sample shape opened by
//! [`super::perf_event::open_cycles_for_pid`]:
//!
//! ```text
//! PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU
//! ```
//!
//! Unknown record kinds are counted and skipped. Lost-sample
//! records are surfaced so the future aggregator can make loss
//! visible instead of silently smoothing over it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use perf_event_open_sys::bindings;

use crate::PerfError;

/// Default data-ring size: 256 pages. On the normal 4 KiB Linux
/// page this is the plan's 1 MiB per-event buffer, plus one
/// metadata page mapped in front of it.
pub const DEFAULT_RING_DATA_PAGES: usize = 256;

const PERF_EVENT_HEADER_SIZE: usize = 8;
const PERF_SAMPLE_RECORD_SIZE: usize = 40;
const PERF_LOST_RECORD_MIN_SIZE: usize = 24;
const PERF_LOST_SAMPLES_RECORD_MIN_SIZE: usize = 16;
const PERF_AUX_RECORD_MIN_SIZE: usize = 32;

/// One IP sample from the cycles+IP perf event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfSample {
    /// Sampled instruction pointer.
    pub ip: u64,
    /// Process id from `PERF_SAMPLE_TID`.
    pub pid: u32,
    /// Thread id from `PERF_SAMPLE_TID`.
    pub tid: u32,
    /// Perf timestamp.
    pub time: u64,
    /// CPU id from `PERF_SAMPLE_CPU`.
    pub cpu: u32,
}

/// A parsed perf ring record that matters to Phase 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerfRecord {
    /// A cycles+IP sample.
    Sample(PerfSample),
    /// `PERF_RECORD_LOST`: the kernel dropped `lost` records for
    /// this event id before it could write them into the ring.
    Lost {
        /// Event id reported by the kernel.
        id: u64,
        /// Number of records lost.
        lost: u64,
    },
    /// `PERF_RECORD_LOST_SAMPLES`: sample-only loss accounting.
    LostSamples {
        /// Number of samples lost.
        lost: u64,
    },
    /// `PERF_RECORD_AUX`: new bytes landed in the AUX trace buffer.
    Aux {
        /// Offset in the AUX ring.
        offset: u64,
        /// Number of AUX bytes described by this record.
        size: u64,
        /// Raw `PERF_AUX_FLAG_*` bits.
        flags: u64,
    },
    /// Any other record kind. We keep the kind and size for
    /// diagnostics but skip the payload.
    Unknown {
        /// Raw `perf_event_header::type`.
        kind: u32,
        /// Raw `perf_event_header::size`.
        size: u16,
    },
}

/// Summary counters from one drain pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainStats {
    /// Number of records parsed and returned.
    pub records: usize,
    /// Sum of loss reported by `PERF_RECORD_LOST` and
    /// `PERF_RECORD_LOST_SAMPLES`.
    pub lost: u64,
    /// Number of skipped record kinds.
    pub unknown: usize,
}

/// AUX mmap layout configured through the perf metadata page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfAuxLayout {
    /// File offset passed to `mmap(2)` for the AUX area.
    pub offset: u64,
    /// AUX ring byte size.
    pub size: usize,
}

/// Current AUX producer/consumer cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfAuxSnapshot {
    /// Kernel-written AUX head cursor.
    pub head: u64,
    /// Debugger-written AUX tail cursor.
    pub tail: u64,
    /// AUX ring byte size configured in the metadata page.
    pub size: usize,
}

/// Owned mmap of a perf event data ring.
#[derive(Debug)]
pub struct PerfRingBuffer {
    ptr: *mut u8,
    len: usize,
    data_offset: usize,
    data_size: usize,
    _event_fd: OwnedFd,
}

impl PerfRingBuffer {
    /// Map a perf event fd. The fd is duplicated before mapping so
    /// the ring can outlive the original [`super::perf_event::PerfMonitor`]
    /// handle without dangling the kernel event.
    pub fn map(fd: RawFd, data_pages: usize) -> Result<Self, PerfError> {
        validate_data_pages(data_pages)?;
        let page_size = page_size()?;
        let len = page_size
            .checked_mul(data_pages + 1)
            .ok_or(PerfError::InvalidRingPages { pages: data_pages })?;
        let dup = dup_fd(fd)?;
        // SAFETY: mmap is called with a live perf event fd, shared
        // read/write mapping, and length computed from the system
        // page size. On success the returned pointer is owned by
        // PerfRingBuffer and unmapped in Drop.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                dup.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(PerfError::Mmap(io::Error::last_os_error()));
        }

        let mut ring = Self {
            ptr: ptr.cast(),
            len,
            data_offset: page_size,
            data_size: data_pages * page_size,
            _event_fd: dup,
        };
        let (kernel_offset, kernel_size) = ring.kernel_data_layout();
        if kernel_offset != 0 && kernel_size != 0 {
            ring.data_offset = usize::try_from(kernel_offset)
                .map_err(|_| PerfError::MalformedRecord("kernel data_offset overflows usize"))?;
            ring.data_size = usize::try_from(kernel_size)
                .map_err(|_| PerfError::MalformedRecord("kernel data_size overflows usize"))?;
        }
        Ok(ring)
    }

    /// Number of bytes in the data ring.
    pub fn data_size(&self) -> usize {
        self.data_size
    }

    /// Configure the metadata page for a later AUX mmap.
    ///
    /// The caller must map the returned `offset`/`size` against the
    /// same perf event fd. The AUX ring size must be a non-zero
    /// power-of-two number of pages, matching the kernel protocol.
    pub fn configure_aux_area(&mut self, aux_bytes: usize) -> Result<PerfAuxLayout, PerfError> {
        let page_size = page_size()?;
        validate_aux_bytes(aux_bytes, page_size)?;
        let data_end =
            self.data_offset
                .checked_add(self.data_size)
                .ok_or(PerfError::MalformedRecord(
                    "perf data layout overflows usize",
                ))?;
        let aux_offset = align_up(data_end, page_size)
            .ok_or(PerfError::MalformedRecord("AUX offset overflows usize"))?;
        let aux_offset_u64 = u64::try_from(aux_offset)
            .map_err(|_| PerfError::MalformedRecord("AUX offset overflows u64"))?;
        let aux_size_u64 = u64::try_from(aux_bytes)
            .map_err(|_| PerfError::MalformedRecord("AUX size overflows u64"))?;

        // SAFETY: metadata page starts at mapping base and has the
        // kernel's perf_event_mmap_page layout. The AUX fields are
        // userspace-owned until the AUX mmap succeeds.
        unsafe {
            let page = &mut *self.metadata_ptr();
            ptr::write_volatile(&mut page.aux_offset, aux_offset_u64);
            ptr::write_volatile(&mut page.aux_size, aux_size_u64);
            let head = ptr::read_volatile(&page.aux_head);
            ptr::write_volatile(&mut page.aux_tail, head);
        }

        Ok(PerfAuxLayout {
            offset: aux_offset_u64,
            size: aux_bytes,
        })
    }

    /// Read the current AUX cursors from the metadata page.
    pub fn aux_snapshot(&self) -> Result<PerfAuxSnapshot, PerfError> {
        // SAFETY: aux_head/aux_tail/aux_size live in the mapped
        // metadata page. Acquire pairs with the kernel's AUX writes.
        let (head, tail, size) = unsafe {
            let page = &*self.metadata_ptr();
            let head = ptr::read_volatile(&page.aux_head);
            fence(Ordering::Acquire);
            let tail = ptr::read_volatile(&page.aux_tail);
            let size = ptr::read_volatile(&page.aux_size);
            (head, tail, size)
        };
        let size = usize::try_from(size)
            .map_err(|_| PerfError::MalformedRecord("AUX size overflows usize"))?;
        Ok(PerfAuxSnapshot { head, tail, size })
    }

    /// Publish a consumed AUX tail cursor to the kernel metadata page.
    pub fn write_aux_tail(&mut self, tail: u64) {
        fence(Ordering::Release);
        // SAFETY: aux_tail is the userspace consumer cursor.
        unsafe {
            ptr::write_volatile(&mut (*self.metadata_ptr()).aux_tail, tail);
        }
    }

    /// Drain all complete records currently visible in the ring.
    pub fn drain(&mut self) -> Result<(Vec<PerfRecord>, DrainStats), PerfError> {
        let head = self.read_head();
        let tail = self.read_tail();
        let available = head.saturating_sub(tail);
        if available > self.data_size as u64 {
            self.write_tail(head);
            return Err(PerfError::RingOverrun {
                available,
                data_size: self.data_size as u64,
            });
        }

        let mut records = Vec::new();
        let mut stats = DrainStats::default();
        let mut cursor = tail;
        while cursor < head {
            let header = self.copy_from_ring(cursor, PERF_EVENT_HEADER_SIZE)?;
            let size = u16::from_ne_bytes(header[6..8].try_into().expect("header size"));
            let size = usize::from(size);
            if size < PERF_EVENT_HEADER_SIZE {
                return Err(PerfError::MalformedRecord(
                    "record size is smaller than header",
                ));
            }
            let next = cursor
                .checked_add(size as u64)
                .ok_or(PerfError::MalformedRecord("record cursor overflow"))?;
            if next > head {
                break;
            }

            let bytes = self.copy_from_ring(cursor, size)?;
            let record = parse_record_bytes(&bytes)?;
            stats.records += 1;
            match &record {
                PerfRecord::Lost { lost, .. } | PerfRecord::LostSamples { lost } => {
                    stats.lost = stats.lost.saturating_add(*lost);
                }
                PerfRecord::Unknown { .. } => stats.unknown += 1,
                PerfRecord::Sample(_) | PerfRecord::Aux { .. } => {}
            }
            records.push(record);
            cursor = next;
        }
        self.write_tail(cursor);
        Ok((records, stats))
    }

    fn metadata_ptr(&self) -> *mut bindings::perf_event_mmap_page {
        self.ptr.cast()
    }

    fn data_ptr(&self) -> *const u8 {
        // SAFETY: data_offset is established from the kernel mmap
        // metadata or the page-size fallback and points inside the
        // mapping. Bounds are checked by copy_from_ring.
        unsafe { self.ptr.add(self.data_offset) }
    }

    fn kernel_data_layout(&self) -> (u64, u64) {
        // SAFETY: metadata page starts at mapping base and has the
        // kernel's perf_event_mmap_page layout.
        let page = unsafe { &*self.metadata_ptr() };
        (page.data_offset, page.data_size)
    }

    fn read_head(&self) -> u64 {
        // SAFETY: data_head is written by the kernel. Volatile read
        // plus acquire fence matches the perf ring protocol.
        let head = unsafe { ptr::read_volatile(&(*self.metadata_ptr()).data_head) };
        fence(Ordering::Acquire);
        head
    }

    fn read_tail(&self) -> u64 {
        // SAFETY: data_tail is written only by us for this mapping.
        unsafe { ptr::read_volatile(&(*self.metadata_ptr()).data_tail) }
    }

    fn write_tail(&mut self, tail: u64) {
        fence(Ordering::Release);
        // SAFETY: publishing consumed bytes to the kernel-owned
        // metadata page.
        unsafe {
            ptr::write_volatile(&mut (*self.metadata_ptr()).data_tail, tail);
        }
    }

    fn copy_from_ring(&self, cursor: u64, len: usize) -> Result<Vec<u8>, PerfError> {
        if len > self.data_size {
            return Err(PerfError::MalformedRecord("record exceeds ring size"));
        }
        let start = (cursor % self.data_size as u64) as usize;
        let first_len = len.min(self.data_size - start);
        let mut out = vec![0u8; len];
        // SAFETY: start/first_len and optional wrapped remainder
        // are checked against data_size; out has exactly len bytes.
        unsafe {
            ptr::copy_nonoverlapping(self.data_ptr().add(start), out.as_mut_ptr(), first_len);
            if first_len < len {
                ptr::copy_nonoverlapping(
                    self.data_ptr(),
                    out.as_mut_ptr().add(first_len),
                    len - first_len,
                );
            }
        }
        Ok(out)
    }
}

impl Drop for PerfRingBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr/len are either a successful mmap from map()
        // or the process is aborting. Drop cannot report errors.
        unsafe {
            let _ = libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

fn validate_data_pages(data_pages: usize) -> Result<(), PerfError> {
    if data_pages == 0 || !data_pages.is_power_of_two() {
        return Err(PerfError::InvalidRingPages { pages: data_pages });
    }
    Ok(())
}

fn validate_aux_bytes(aux_bytes: usize, page_size: usize) -> Result<(), PerfError> {
    if aux_bytes == 0 || !aux_bytes.is_power_of_two() || aux_bytes % page_size != 0 {
        return Err(PerfError::InvalidAuxBufferSize {
            bytes: aux_bytes,
            page_size,
        });
    }
    Ok(())
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    Some((value.checked_add(align - 1)?) & !(align - 1))
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

fn dup_fd(fd: RawFd) -> Result<OwnedFd, PerfError> {
    // SAFETY: fcntl duplicates a live fd or returns -1 with errno.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(PerfError::FdDup(io::Error::last_os_error()));
    }
    // SAFETY: positive fcntl return is a newly-owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn parse_record_bytes(bytes: &[u8]) -> Result<PerfRecord, PerfError> {
    if bytes.len() < PERF_EVENT_HEADER_SIZE {
        return Err(PerfError::MalformedRecord("record is smaller than header"));
    }
    let kind = read_u32(bytes, 0)?;
    let size = usize::from(read_u16(bytes, 6)?);
    if size != bytes.len() {
        return Err(PerfError::MalformedRecord(
            "record size does not match payload",
        ));
    }
    match kind {
        k if k == bindings::PERF_RECORD_SAMPLE => parse_sample(bytes),
        k if k == bindings::PERF_RECORD_LOST => parse_lost(bytes),
        k if k == bindings::PERF_RECORD_LOST_SAMPLES => parse_lost_samples(bytes),
        k if k == bindings::PERF_RECORD_AUX => parse_aux(bytes),
        _ => Ok(PerfRecord::Unknown {
            kind,
            size: size as u16,
        }),
    }
}

fn parse_sample(bytes: &[u8]) -> Result<PerfRecord, PerfError> {
    if bytes.len() != PERF_SAMPLE_RECORD_SIZE {
        return Err(PerfError::MalformedRecord("unexpected sample record size"));
    }
    Ok(PerfRecord::Sample(PerfSample {
        ip: read_u64(bytes, 8)?,
        pid: read_u32(bytes, 16)?,
        tid: read_u32(bytes, 20)?,
        time: read_u64(bytes, 24)?,
        cpu: read_u32(bytes, 32)?,
    }))
}

fn parse_lost(bytes: &[u8]) -> Result<PerfRecord, PerfError> {
    if bytes.len() < PERF_LOST_RECORD_MIN_SIZE {
        return Err(PerfError::MalformedRecord("unexpected lost record size"));
    }
    Ok(PerfRecord::Lost {
        id: read_u64(bytes, 8)?,
        lost: read_u64(bytes, 16)?,
    })
}

fn parse_lost_samples(bytes: &[u8]) -> Result<PerfRecord, PerfError> {
    if bytes.len() < PERF_LOST_SAMPLES_RECORD_MIN_SIZE {
        return Err(PerfError::MalformedRecord(
            "unexpected lost-samples record size",
        ));
    }
    Ok(PerfRecord::LostSamples {
        lost: read_u64(bytes, 8)?,
    })
}

fn parse_aux(bytes: &[u8]) -> Result<PerfRecord, PerfError> {
    if bytes.len() < PERF_AUX_RECORD_MIN_SIZE {
        return Err(PerfError::MalformedRecord("unexpected AUX record size"));
    }
    Ok(PerfRecord::Aux {
        offset: read_u64(bytes, 8)?,
        size: read_u64(bytes, 16)?,
        flags: read_u64(bytes, 24)?,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PerfError> {
    let end = offset + 2;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(PerfError::MalformedRecord("u16 field out of bounds"));
    };
    Ok(u16::from_ne_bytes(slice.try_into().expect("u16 width")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PerfError> {
    let end = offset + 4;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(PerfError::MalformedRecord("u32 field out of bounds"));
    };
    Ok(u32::from_ne_bytes(slice.try_into().expect("u32 width")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, PerfError> {
    let end = offset + 8;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(PerfError::MalformedRecord("u64 field out of bounds"));
    };
    Ok(u64::from_ne_bytes(slice.try_into().expect("u64 width")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u32, size: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&0u16.to_ne_bytes());
        bytes.extend_from_slice(&size.to_ne_bytes());
        bytes
    }

    #[test]
    fn parses_cycles_sample_record_shape() {
        let mut bytes = header(bindings::PERF_RECORD_SAMPLE, PERF_SAMPLE_RECORD_SIZE as u16);
        bytes.extend_from_slice(&0xfeed_face_cafe_babeu64.to_ne_bytes());
        bytes.extend_from_slice(&123u32.to_ne_bytes());
        bytes.extend_from_slice(&456u32.to_ne_bytes());
        bytes.extend_from_slice(&789u64.to_ne_bytes());
        bytes.extend_from_slice(&7u32.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());

        let record = parse_record_bytes(&bytes).expect("parse");
        assert_eq!(
            record,
            PerfRecord::Sample(PerfSample {
                ip: 0xfeed_face_cafe_babe,
                pid: 123,
                tid: 456,
                time: 789,
                cpu: 7,
            })
        );
    }

    #[test]
    fn parses_lost_records() {
        let mut bytes = header(bindings::PERF_RECORD_LOST, PERF_LOST_RECORD_MIN_SIZE as u16);
        bytes.extend_from_slice(&11u64.to_ne_bytes());
        bytes.extend_from_slice(&22u64.to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&bytes).expect("parse"),
            PerfRecord::Lost { id: 11, lost: 22 }
        );

        let mut with_sample_id = bytes;
        with_sample_id[6..8]
            .copy_from_slice(&(PERF_LOST_RECORD_MIN_SIZE as u16 + 16).to_ne_bytes());
        with_sample_id.extend_from_slice(&123u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&456u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&789u64.to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&with_sample_id).expect("parse"),
            PerfRecord::Lost { id: 11, lost: 22 }
        );
    }

    #[test]
    fn parses_lost_samples_records() {
        let mut bytes = header(
            bindings::PERF_RECORD_LOST_SAMPLES,
            PERF_LOST_SAMPLES_RECORD_MIN_SIZE as u16,
        );
        bytes.extend_from_slice(&33u64.to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&bytes).expect("parse"),
            PerfRecord::LostSamples { lost: 33 }
        );

        let mut with_sample_id = bytes;
        with_sample_id[6..8]
            .copy_from_slice(&(PERF_LOST_SAMPLES_RECORD_MIN_SIZE as u16 + 16).to_ne_bytes());
        with_sample_id.extend_from_slice(&123u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&456u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&789u64.to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&with_sample_id).expect("parse"),
            PerfRecord::LostSamples { lost: 33 }
        );
    }

    #[test]
    fn parses_aux_records_with_optional_sample_id_trailer() {
        let mut bytes = header(bindings::PERF_RECORD_AUX, PERF_AUX_RECORD_MIN_SIZE as u16);
        bytes.extend_from_slice(&0x1000u64.to_ne_bytes());
        bytes.extend_from_slice(&0x2000u64.to_ne_bytes());
        bytes.extend_from_slice(&u64::from(bindings::PERF_AUX_FLAG_TRUNCATED).to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&bytes).expect("parse"),
            PerfRecord::Aux {
                offset: 0x1000,
                size: 0x2000,
                flags: u64::from(bindings::PERF_AUX_FLAG_TRUNCATED),
            }
        );

        let mut with_sample_id = bytes;
        with_sample_id[6..8].copy_from_slice(&(PERF_AUX_RECORD_MIN_SIZE as u16 + 16).to_ne_bytes());
        with_sample_id.extend_from_slice(&123u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&456u32.to_ne_bytes());
        with_sample_id.extend_from_slice(&789u64.to_ne_bytes());
        assert_eq!(
            parse_record_bytes(&with_sample_id).expect("parse"),
            PerfRecord::Aux {
                offset: 0x1000,
                size: 0x2000,
                flags: u64::from(bindings::PERF_AUX_FLAG_TRUNCATED),
            }
        );
    }

    #[test]
    fn unknown_records_are_counted_but_not_rejected() {
        let bytes = header(0xfeed, PERF_EVENT_HEADER_SIZE as u16);
        assert_eq!(
            parse_record_bytes(&bytes).expect("parse"),
            PerfRecord::Unknown {
                kind: 0xfeed,
                size: PERF_EVENT_HEADER_SIZE as u16,
            }
        );
    }

    #[test]
    fn malformed_size_is_rejected() {
        let bytes = header(bindings::PERF_RECORD_SAMPLE, 12);
        assert!(matches!(
            parse_record_bytes(&bytes),
            Err(PerfError::MalformedRecord(_))
        ));
    }

    #[test]
    fn data_pages_must_be_nonzero_power_of_two() {
        assert!(validate_data_pages(1).is_ok());
        assert!(validate_data_pages(256).is_ok());
        assert!(matches!(
            validate_data_pages(0),
            Err(PerfError::InvalidRingPages { pages: 0 })
        ));
        assert!(matches!(
            validate_data_pages(3),
            Err(PerfError::InvalidRingPages { pages: 3 })
        ));
    }

    #[test]
    fn aux_bytes_must_be_nonzero_power_of_two_pages() {
        let page_size = 4096;
        assert!(validate_aux_bytes(page_size, page_size).is_ok());
        assert!(validate_aux_bytes(64 * 1024 * 1024, page_size).is_ok());
        assert!(matches!(
            validate_aux_bytes(0, page_size),
            Err(PerfError::InvalidAuxBufferSize { bytes: 0, .. })
        ));
        assert!(matches!(
            validate_aux_bytes(page_size + 1, page_size),
            Err(PerfError::InvalidAuxBufferSize { .. })
        ));
        assert!(matches!(
            validate_aux_bytes(page_size * 3, page_size),
            Err(PerfError::InvalidAuxBufferSize { .. })
        ));
    }
}

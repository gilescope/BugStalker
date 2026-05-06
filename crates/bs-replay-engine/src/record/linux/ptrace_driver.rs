// SPDX-License-Identifier: MIT
//! Sub-phase 3B step 7 — supervisor-side seccomp notification
//! loop and `Event::Syscall` emit.
//!
//! On Linux ≥ 5.5 the recorder uses the listener fd that
//! `seccomp(SET_MODE_FILTER, NEW_LISTENER, …)` returns; for each
//! trapped syscall the supervisor:
//!
//! 1. `ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV, &notif)` — pull
//!    one notification. Blocks until a tracee runs a syscall.
//! 2. Build a [`super::super::syscall_capture::CallFrame`] from
//!    `notif.data` (which is the kernel's `seccomp_data`: nr,
//!    arch, instruction_pointer, args[6]).
//! 3. Read pointer-shaped args via [`MemoryReader`] (the Linux
//!    impl wraps `process_vm_readv` against `/proc/<tid>/mem`)
//!    and run the curated/long-tail/catch-all dispatcher.
//! 4. `ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, &resp)` with
//!    `SECCOMP_USER_NOTIF_FLAG_CONTINUE` — kernel runs the
//!    syscall as if seccomp wasn't there, the tracee resumes.
//! 5. Emit one [`Event::Syscall`] via the [`TraceWriter`].
//!
//! ## Result capture — staged in
//!
//! Step 7 captures *entry-side* arguments. Post-syscall result +
//! out-buffer capture requires waiting for the syscall-exit-stop
//! and reading the tracee's RAX, which is the ptrace adjunct
//! that lands in step 7b together with the `RecordChild` helper.
//! In step 7's wire format `Event::Syscall.result` is stamped
//! `RESULT_NOT_CAPTURED_YET` (a documented sentinel); replay
//! readers warn-and-continue when they see it.
//!
//! Why split: the wiring in this module is platform-agnostic
//! (mock-driven testable) but the real fork+exec lifecycle and
//! ptrace setup are heavy enough that one focused commit per
//! concern keeps the changeset reviewable.
//!
//! ## Public surface
//!
//! - [`SeccompNotif`] — Rust mirror of `struct seccomp_notif`.
//! - [`SeccompNotifResp`] — mirror of `struct seccomp_notif_resp`.
//! - [`recv_notif`] / [`respond_continue`] — ioctl wrappers.
//! - [`record_one_syscall`] — composes recv + capture + send
//!   into a single supervisor turn; consumes a
//!   [`MemoryReader`] so tests inject a mock.

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::format::event::Event;
use crate::format::trace_writer::{TraceWriteError, TraceWriter};
use crate::record::syscall_capture::{
    self, capture_pre_syscall, CallFrame, CapturedSyscall, MemoryReader,
};

/// Documented sentinel `Event::Syscall.result` value the
/// recorder emits while step 7's result-capture path is staged
/// in. Replay readers downgrade to "args-only" mode when they
/// see this — the trace remains useful for argument-only
/// inspection but full deterministic replay needs the result.
///
/// Encoded as `i64::MIN` (smallest possible value, a value no
/// real Linux syscall return can produce — kernel returns are
/// in `[-MAX_ERRNO, isize::MAX]`, and `MAX_ERRNO == 4095`).
pub const RESULT_NOT_CAPTURED_YET: i64 = i64::MIN;

// ---------------------------------------------------------------------------
// Kernel ABI mirrors
// ---------------------------------------------------------------------------

/// `struct seccomp_data` from `linux/seccomp.h`. Layout is
/// stable since Linux 3.5.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SeccompData {
    /// `__NR_*` syscall number.
    pub nr: i32,
    /// Architecture (`AUDIT_ARCH_*`).
    pub arch: u32,
    /// Tracee instruction pointer at syscall entry.
    pub instruction_pointer: u64,
    /// Six argument registers in ABI order.
    pub args: [u64; 6],
}

/// `struct seccomp_notif` from `linux/seccomp.h`. The kernel
/// fills this in response to `SECCOMP_IOCTL_NOTIF_RECV`. Layout
/// is stable since Linux 5.0; the `pid` field's reservation in
/// the layout pre-dates the public exposure of the ioctl.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SeccompNotif {
    /// Monotonic notification id; the supervisor echoes it back
    /// in the response.
    pub id: u64,
    /// PID of the tracee that issued the syscall.
    pub pid: u32,
    /// Reserved for future kernel fields; always 0 today.
    pub flags: u32,
    /// Captured tracee state at syscall entry.
    pub data: SeccompData,
}

/// `struct seccomp_notif_resp` from `linux/seccomp.h`. The
/// supervisor fills this and sends it back via
/// `SECCOMP_IOCTL_NOTIF_SEND`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SeccompNotifResp {
    /// Same `id` from the matching notification.
    pub id: u64,
    /// What the supervisor wants the kernel to do.
    pub val: i64,
    /// Errno (negative value) when intercepting; ignored when
    /// `flags == FLAG_CONTINUE`.
    pub error: i32,
    /// `SECCOMP_USER_NOTIF_FLAG_CONTINUE` lets the kernel run
    /// the syscall natively. Zero means "intercept": the
    /// kernel returns `val`/`error` to the tracee without
    /// running the syscall.
    pub flags: u32,
}

/// `linux/seccomp.h`: SECCOMP_USER_NOTIF_FLAG_CONTINUE.
pub const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// ioctl numbers
// ---------------------------------------------------------------------------
//
// Built from the kernel's `_IOWR / _IOW` macros; the ioctl
// numbering ABI is stable. Two ioctls are needed for the basic
// supervisor loop:
//
//   #define SECCOMP_IOCTL_NOTIF_RECV  _IOWR('!', 0, struct seccomp_notif)
//   #define SECCOMP_IOCTL_NOTIF_SEND  _IOWR('!', 1, struct seccomp_notif_resp)
//
// `_IOC_TYPE = '!'` (0x21), `_IOC_DIR = _IOC_READ | _IOC_WRITE
// = 3`, the size is `sizeof(struct …)`, the nr is the trailing
// integer.

const fn ioc(dir: u32, type_: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((dir & 0x3) << 30
        | (size & 0x3FFF) << 16
        | (type_ & 0xFF) << 8
        | (nr & 0xFF)) as libc::c_ulong
}
const IOC_NONE: u32 = 0;
#[allow(dead_code)]
const IOC_WRITE: u32 = 1;
#[allow(dead_code)]
const IOC_READ: u32 = 2;
const IOC_RW: u32 = 3;
const SECCOMP_IOC_TYPE: u32 = b'!' as u32;

/// `SECCOMP_IOCTL_NOTIF_RECV`.
pub fn ioctl_notif_recv() -> libc::c_ulong {
    ioc(IOC_RW, SECCOMP_IOC_TYPE, 0, std::mem::size_of::<SeccompNotif>() as u32)
}

/// `SECCOMP_IOCTL_NOTIF_SEND`.
pub fn ioctl_notif_send() -> libc::c_ulong {
    ioc(IOC_RW, SECCOMP_IOC_TYPE, 1, std::mem::size_of::<SeccompNotifResp>() as u32)
}

#[allow(dead_code)] // exported for completeness; used in step 7b
fn ioc_none() -> u32 {
    IOC_NONE
}

// ---------------------------------------------------------------------------
// ioctl wrappers
// ---------------------------------------------------------------------------

/// Pull one syscall notification off the listener fd. Blocks
/// until a tracee runs a syscall.
pub fn recv_notif(listener: BorrowedFd<'_>) -> io::Result<SeccompNotif> {
    let mut notif = SeccompNotif::default();
    // SAFETY: SECCOMP_IOCTL_NOTIF_RECV expects a writable
    // pointer to a `struct seccomp_notif`. The local is fully
    // owned and big enough.
    let r = unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            ioctl_notif_recv() as _,
            &mut notif as *mut SeccompNotif as *mut libc::c_void,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(notif)
}

/// Tell the kernel to run the syscall natively (record-mode
/// FLAG_CONTINUE response).
pub fn respond_continue(
    listener: BorrowedFd<'_>,
    id: u64,
) -> io::Result<()> {
    let resp = SeccompNotifResp {
        id,
        val: 0,
        error: 0,
        flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
    };
    // SAFETY: SECCOMP_IOCTL_NOTIF_SEND reads the local through
    // the supplied pointer; the local's layout matches the
    // kernel struct definition above.
    let r = unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            ioctl_notif_send() as _,
            &resp as *const SeccompNotifResp as *const libc::c_void,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Tell the kernel to *intercept* the syscall — kernel returns
/// `val` (or `-error`) to the tracee without running the
/// syscall. Used by the replay shim (3C, step 8) — *not* by the
/// recorder.
pub fn respond_intercept(
    listener: BorrowedFd<'_>,
    id: u64,
    val: i64,
    err: i32,
) -> io::Result<()> {
    let resp = SeccompNotifResp { id, val, error: err, flags: 0 };
    let r = unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            ioctl_notif_send() as _,
            &resp as *const SeccompNotifResp as *const libc::c_void,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recorder loop primitives
// ---------------------------------------------------------------------------

/// Convert a [`SeccompNotif`] to a [`CallFrame`].
pub fn frame_from_notif(notif: &SeccompNotif) -> CallFrame {
    CallFrame {
        nr: notif.data.nr.max(0) as u32,
        args: notif.data.args,
    }
}

/// Encode a captured pre-syscall observation into the wire
/// shape the trace writer expects: an [`Event::Syscall`] with
/// the captured byte-blob in `output`.
pub fn event_for_capture(cap: &CapturedSyscall) -> Event {
    Event::Syscall {
        nr: cap.nr,
        args: cap.args,
        result: cap.result,
        output: cap.encode_output(),
    }
}

/// One supervisor turn — without the actual ioctl. The tracer
/// loop does:
///
/// 1. Pull a notif.
/// 2. Hand it to this function with a `MemoryReader` that
///    points at the tracee's address space.
/// 3. Send `respond_continue`.
/// 4. Push the returned event into the writer.
///
/// Splitting the function keeps the actual ioctl out of the
/// unit tests — every capture decision drops out of step (2),
/// which is pure Rust.
pub fn capture_from_notif(
    notif: &SeccompNotif,
    reader: &dyn MemoryReader,
) -> CapturedSyscall {
    let mut cap = capture_pre_syscall(frame_from_notif(notif), reader);
    // The result is stamped with the documented sentinel until
    // step 7b's syscall-exit-stop path lands. Out-buffer post-
    // capture (curated `OutBuf`) gets recorded then too.
    cap.result = RESULT_NOT_CAPTURED_YET;
    cap
}

/// End-to-end supervisor turn against a live listener fd.
/// Recv → capture → continue → emit event. Returns the captured
/// observation for callers that want to inspect / log it.
pub fn record_one_syscall(
    listener: BorrowedFd<'_>,
    reader: &dyn MemoryReader,
    writer: &mut TraceWriter,
) -> Result<CapturedSyscall, RecorderError> {
    let notif = recv_notif(listener).map_err(RecorderError::Recv)?;
    let cap = capture_from_notif(&notif, reader);
    respond_continue(listener, notif.id).map_err(RecorderError::Respond)?;
    writer
        .write_event(event_for_capture(&cap))
        .map_err(RecorderError::Write)?;
    Ok(cap)
}

/// Errors arising from the recorder loop.
#[derive(thiserror::Error, Debug)]
pub enum RecorderError {
    /// `SECCOMP_IOCTL_NOTIF_RECV` failed.
    #[error("seccomp recv: {0}")]
    Recv(io::Error),
    /// `SECCOMP_IOCTL_NOTIF_SEND` failed.
    #[error("seccomp respond: {0}")]
    Respond(io::Error),
    /// Trace writer failed.
    #[error("trace write: {0}")]
    Write(TraceWriteError),
}

/// `MemoryReader` impl reading from `/proc/<pid>/mem`. The
/// recorder's primary path; the file descriptor is opened once
/// per-tracee and held for the duration of the recording.
///
/// `process_vm_readv` would also work; the file backing keeps
/// the surface usable from any thread without per-tracee setup.
#[derive(Debug)]
pub struct ProcMemReader {
    file: std::fs::File,
}

impl ProcMemReader {
    /// Open `/proc/<pid>/mem` for reading.
    pub fn open(pid: i32) -> io::Result<Self> {
        let path = format!("/proc/{pid}/mem");
        let file = std::fs::File::open(path)?;
        Ok(Self { file })
    }
}

impl MemoryReader for ProcMemReader {
    fn read(&self, addr: u64, max: usize) -> Vec<u8> {
        // Plan §Pure-Rust policy: rustix wraps pread; std also
        // does, but without the same Result discipline. `pread`
        // is deliberate — `read_at` would advance the cursor
        // and break thread safety.
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; max];
        match self.file.read_at(&mut buf, addr) {
            Ok(n) => {
                buf.truncate(n);
                buf
            }
            // EFAULT mid-read: address mapped at the start but
            // unmapped before the read completed — happens with
            // mremap-style code. Truncate to what we got; an
            // empty slice means we got nothing.
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Reuse the mock from syscall_capture for end-to-end tests.
    #[derive(Default)]
    struct Mock {
        regions: BTreeMap<u64, Vec<u8>>,
    }
    impl Mock {
        fn put(&mut self, addr: u64, data: &[u8]) {
            self.regions.insert(addr, data.to_vec());
        }
    }
    impl MemoryReader for Mock {
        fn read(&self, addr: u64, max: usize) -> Vec<u8> {
            let candidate = self
                .regions
                .range(..=addr)
                .next_back()
                .map(|(b, v)| (*b, v.clone()));
            let (base, mut bytes) = match candidate {
                Some((b, v)) if addr - b < v.len() as u64 => (b, v),
                _ => return Vec::new(),
            };
            let off = (addr - base) as usize;
            bytes.drain(..off);
            if bytes.len() > max {
                bytes.truncate(max);
            }
            bytes
        }
    }

    fn write_notif(nr: i32, args: [u64; 6]) -> SeccompNotif {
        SeccompNotif {
            id: 0xDEAD_BEEF,
            pid: 12345,
            flags: 0,
            data: SeccompData {
                nr,
                arch: 0xC000_003E, // AUDIT_ARCH_X86_64
                instruction_pointer: 0x4007_a0,
                args,
            },
        }
    }

    #[test]
    fn frame_from_notif_carries_nr_and_args() {
        let n = write_notif(1, [2, 0xCAFE_BA00, 5, 0, 0, 0]);
        let f = frame_from_notif(&n);
        assert_eq!(f.nr, 1);
        assert_eq!(f.args[0], 2);
        assert_eq!(f.args[1], 0xCAFE_BA00);
    }

    #[test]
    fn capture_from_notif_stamps_result_sentinel() {
        let n = write_notif(1, [2, 0xCAFE_BA00, 5, 0, 0, 0]);
        let mut mem = Mock::default();
        mem.put(0xCAFE_BA00, b"hello");
        let cap = capture_from_notif(&n, &mem);
        assert_eq!(cap.result, RESULT_NOT_CAPTURED_YET);
        assert_eq!(cap.regions.len(), 1);
        assert_eq!(cap.regions[0].bytes, b"hello");
    }

    #[test]
    fn event_for_capture_stamps_args_and_output() {
        let n = write_notif(1, [2, 0xCAFE_BA00, 5, 0, 0, 0]);
        let mut mem = Mock::default();
        mem.put(0xCAFE_BA00, b"hello");
        let cap = capture_from_notif(&n, &mem);
        let ev = event_for_capture(&cap);
        match ev {
            Event::Syscall { nr, args, result, output } => {
                assert_eq!(nr, 1);
                assert_eq!(args[0], 2);
                assert_eq!(args[1], 0xCAFE_BA00);
                assert_eq!(result, RESULT_NOT_CAPTURED_YET);
                let back =
                    syscall_capture::CapturedSyscall::decode_output(nr, args, result, &output)
                        .expect("decode");
                assert_eq!(back, cap);
            }
            other => panic!("expected Event::Syscall, got {other:?}"),
        }
    }

    #[test]
    fn ioctl_numbers_match_kernel_macros() {
        // The kernel literally does:
        //   #define SECCOMP_IOCTL_NOTIF_RECV \
        //       _IOWR('!', 0, struct seccomp_notif)
        //   #define SECCOMP_IOCTL_NOTIF_SEND \
        //       _IOWR('!', 1, struct seccomp_notif_resp)
        // Validate the numeric encoding — anyone touching
        // ioc()/IOC_RW/SECCOMP_IOC_TYPE incorrectly hits this.
        let recv = ioctl_notif_recv();
        let send = ioctl_notif_send();
        // Direction: read+write = 3 → top two bits = 0b11
        assert_eq!((recv >> 30) & 0x3, 3);
        assert_eq!((send >> 30) & 0x3, 3);
        // Type byte 0x21 ('!')
        assert_eq!((recv >> 8) & 0xFF, b'!' as libc::c_ulong);
        // nr 0 vs 1
        assert_eq!(recv & 0xFF, 0);
        assert_eq!(send & 0xFF, 1);
        // Size byte must equal sizeof(struct).
        assert_eq!(
            (recv >> 16) & 0x3FFF,
            std::mem::size_of::<SeccompNotif>() as libc::c_ulong,
        );
        assert_eq!(
            (send >> 16) & 0x3FFF,
            std::mem::size_of::<SeccompNotifResp>() as libc::c_ulong,
        );
    }

    #[test]
    fn seccomp_data_layout_matches_kernel_abi() {
        // The kernel's `struct seccomp_data`:
        //   __s32 nr;
        //   __u32 arch;
        //   __u64 instruction_pointer;
        //   __u64 args[6];
        // Total = 4 + 4 + 8 + 6*8 = 64 bytes.
        assert_eq!(std::mem::size_of::<SeccompData>(), 64);
        assert_eq!(
            std::mem::size_of::<SeccompNotif>(),
            // u64 id + u32 pid + u32 flags + 64 bytes data
            8 + 4 + 4 + 64,
        );
        assert_eq!(
            std::mem::size_of::<SeccompNotifResp>(),
            // u64 id + i64 val + i32 error + u32 flags
            8 + 8 + 4 + 4,
        );
    }
}

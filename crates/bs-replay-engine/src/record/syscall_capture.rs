// SPDX-License-Identifier: MIT
//! Platform-independent syscall-capture primitive (sub-phase 3B,
//! step 5).
//!
//! Splits the recorder into "what to read" (this module — pure
//! Rust over a [`MemoryReader`] trait) and "where to read from"
//! (the Linux `ptrace + process_vm_readv` driver in
//! `record::linux`). The split lets every capture decision be
//! exercised by mock-driven unit tests; the Linux driver is then
//! a thin adapter.
//!
//! ## Three-tier capture, in priority order
//!
//! 1. **Curated** — [`bs_syscall_spec::lookup_x86_64`] hits.
//!    Read exactly the bytes the [`SyscallSpec`]'s
//!    [`ParamKind::InBuf`] / [`ParamKind::OutBuf`] / [`ParamKind::InCStr`]
//!    decorations name. No waste.
//! 2. **Long tail** — [`bs_syscall_spec::lookup_long_tail_x86_64`]
//!    hits. We have the syscall's name but not its pointer-arg
//!    shape; fall back to the catch-all heuristic.
//! 3. **Catch-all** — neither table knows the syscall. The
//!    recorder logs the six argument registers verbatim and a
//!    [`CATCH_ALL_WINDOW`]-byte window around any user-space
//!    pointer-shaped arg. Bounded loud failure on replay if the
//!    program uses a syscall whose out-pointer shape we
//!    misjudged — the [`bs_syscall_spec`] catch-all docstring
//!    spells out the trade.
//!
//! The encoded output of one capture lives inside
//! [`crate::format::event::Event::Syscall::output`] — the wire
//! format treats it as opaque bytes; the encoding contract is
//! between this module and the replay shim.

use bs_syscall_spec::{ParamKind, ScalarKind, SyscallSpec};

/// Default size of the bytes-around-pointer window the catch-all
/// captures. Plan §3B step 5: "a fixed window (say 256 bytes)
/// around any pointer arg".
pub const CATCH_ALL_WINDOW: usize = 256;

/// Cap on bytes captured for a curated [`ParamKind::InCStr`]
/// (paths). PATH_MAX on Linux. Programs do occasionally pass
/// crafted-longer strings — they get truncated, and the recorder
/// flags the truncation in the captured region's bookkeeping.
pub const CSTR_CAP: usize = 4096;

/// Hard cap on a single OutBuf/InBuf capture. Prevents a runaway
/// `read(fd, buf, 1<<30)` from devouring trace storage. The
/// kernel's actual return value clamps the InBuf-by-ret case
/// already; this cap protects the InBuf-by-named-len case too.
pub const BUFFER_CAP: usize = 64 * 1024;

/// One side of one syscall observation — what registers held,
/// what memory the kernel touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSyscall {
    /// `__NR_*`.
    pub nr: u32,
    /// The six argument registers in ABI order (RDI/RSI/RDX/R10/
    /// R8/R9 on x86-64; X0–X5 on aarch64).
    pub args: [u64; 6],
    /// Sign-extended return value; negative is `-errno`.
    pub result: i64,
    /// Bytes the kernel read or wrote at recorder-capture time,
    /// in the order they were emitted. Encoded inside
    /// [`crate::format::event::Event::Syscall::output`] via
    /// [`Self::encode_output`] / [`Self::decode_output`].
    pub regions: Vec<CapturedRegion>,
    /// Which capture tier produced this. Diagnostic only.
    pub tier: CaptureTier,
}

/// A captured slice of tracee memory associated with one syscall
/// argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRegion {
    /// Index 0–5 — which argument register held the address.
    pub arg_idx: u8,
    /// User-space virtual address the bytes came from. Kept so a
    /// replay shim writes them back to the same location.
    pub addr: u64,
    /// The bytes themselves. May be shorter than `requested_len`
    /// when the read truncated at a page boundary or hit
    /// [`CSTR_CAP`] / [`BUFFER_CAP`].
    pub bytes: Vec<u8>,
    /// What the recorder originally asked for. `bytes.len() <
    /// requested_len` flags a truncation.
    pub requested_len: usize,
    /// What kind of region — for diagnostic only; the wire
    /// format collapses every kind to "bytes the replay shim
    /// writes back".
    pub kind: CapturedKind,
}

/// Why this region was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedKind {
    /// Curated `InBuf` — read pre-syscall.
    InBuf,
    /// Curated `OutBuf` — read post-syscall.
    OutBuf,
    /// Curated `InCStr` — null-terminated string.
    InCStr,
    /// Catch-all heuristic — pointer-shaped argument.
    CatchAll,
}

/// Which tier produced the capture. The recorder logs the tier
/// alongside the captured args so post-mortem analysis can flag
/// "this trace had N catch-all captures that may replay
/// imperfectly".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTier {
    /// Curated [`SyscallSpec`] used.
    Curated,
    /// Long-tail name was known but no pointer-arg shape — used
    /// catch-all heuristic.
    LongTail,
    /// Neither table knew the syscall at all — catch-all
    /// heuristic plus `syscall_<nr>` synthesised name.
    Unknown,
}

/// What's known about the registers + return at the moment of
/// capture. Built by the platform driver from ptrace stops; the
/// pure-Rust capture primitive operates on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallFrame {
    /// `__NR_*`.
    pub nr: u32,
    /// Six argument registers in ABI order.
    pub args: [u64; 6],
}

/// Read-from-tracee abstraction. The Linux driver implements
/// this in terms of `process_vm_readv`; tests use a mock that
/// serves pre-loaded byte slices. Decoupling the syscall
/// dispatch logic from the read mechanism is what lets every
/// capture decision land in unit tests.
pub trait MemoryReader {
    /// Read up to `max` bytes from `addr`. Should return shorter
    /// slices when the read truncates (page boundary, EOF on
    /// /proc/<pid>/mem, EFAULT mid-read). May return an empty
    /// slice if the address is unmapped.
    fn read(&self, addr: u64, max: usize) -> Vec<u8>;
}

/// Decide which tier applies to a given syscall number.
pub fn classify(nr: u32) -> Tier<'static> {
    if let Some(spec) = bs_syscall_spec::lookup_x86_64(nr) {
        return Tier::Curated(spec);
    }
    if let Some(g) = bs_syscall_spec::lookup_long_tail_x86_64(nr) {
        return Tier::LongTail(g);
    }
    Tier::Unknown
}

/// Result of [`classify`].
#[derive(Debug, Clone, Copy)]
pub enum Tier<'a> {
    /// Curated table hit.
    Curated(&'a SyscallSpec),
    /// Long-tail table hit (name only).
    LongTail(&'a bs_syscall_spec::GenericSyscall),
    /// Neither table knows this syscall.
    Unknown,
}

/// Capture the *pre-syscall* memory side effects of a syscall —
/// the buffers the kernel will *read* from the tracee. Runs on
/// curated `InBuf` parameters and on every pointer-shaped arg of
/// a long-tail/unknown syscall.
///
/// `result` is unused at the pre-syscall step (the syscall
/// hasn't executed yet) — but the signature keeps it consistent
/// with [`capture_post_syscall`] so the caller can use one shape
/// for both passes.
pub fn capture_pre_syscall(frame: CallFrame, reader: &dyn MemoryReader) -> CapturedSyscall {
    let mut regions = Vec::new();
    let tier = match classify(frame.nr) {
        Tier::Curated(spec) => {
            for (idx, p) in spec.params.iter().enumerate() {
                if let ParamKind::InBuf { len_param } = p.kind
                    && let Some(len) = resolve_buf_len(
                        spec, &frame, /*ret=*/ 0, len_param, /*pre=*/ true,
                    )
                {
                    push_buf_region(
                        &mut regions,
                        idx,
                        frame.args[idx],
                        len.min(BUFFER_CAP),
                        CapturedKind::InBuf,
                        reader,
                    );
                }
                if matches!(p.kind, ParamKind::InCStr) {
                    push_cstr_region(&mut regions, idx, frame.args[idx], reader);
                }
            }
            CaptureTier::Curated
        }
        Tier::LongTail(_) => {
            push_catch_all_regions(&mut regions, &frame, reader);
            CaptureTier::LongTail
        }
        Tier::Unknown => {
            push_catch_all_regions(&mut regions, &frame, reader);
            CaptureTier::Unknown
        }
    };
    CapturedSyscall {
        nr: frame.nr,
        args: frame.args,
        result: 0,
        regions,
        tier,
    }
}

/// Capture the *post-syscall* observation: what the kernel
/// actually wrote to the tracee's buffers (curated `OutBuf`),
/// plus the return value. The catch-all path runs again here so
/// long-tail / unknown syscalls also record post-state windows.
pub fn capture_post_syscall(
    frame: CallFrame,
    result: i64,
    reader: &dyn MemoryReader,
) -> CapturedSyscall {
    let mut regions = Vec::new();
    let tier = match classify(frame.nr) {
        Tier::Curated(spec) => {
            for (idx, p) in spec.params.iter().enumerate() {
                if let ParamKind::OutBuf { len_param } = p.kind
                    && let Some(len) =
                        resolve_buf_len(spec, &frame, result, len_param, /*pre=*/ false)
                {
                    push_buf_region(
                        &mut regions,
                        idx,
                        frame.args[idx],
                        len.min(BUFFER_CAP),
                        CapturedKind::OutBuf,
                        reader,
                    );
                }
            }
            CaptureTier::Curated
        }
        Tier::LongTail(_) => {
            push_catch_all_regions(&mut regions, &frame, reader);
            CaptureTier::LongTail
        }
        Tier::Unknown => {
            push_catch_all_regions(&mut regions, &frame, reader);
            CaptureTier::Unknown
        }
    };
    CapturedSyscall {
        nr: frame.nr,
        args: frame.args,
        result,
        regions,
        tier,
    }
}

/// Resolve the length of an `In/OutBuf` from its `len_param`
/// reference. Special token `"ret"` means "use the syscall
/// return value (only valid post-syscall)"; anything else names
/// another scalar parameter on the same spec.
fn resolve_buf_len(
    spec: &SyscallSpec,
    frame: &CallFrame,
    result: i64,
    len_param: &str,
    pre: bool,
) -> Option<usize> {
    if len_param == "ret" {
        if pre {
            // No return value to use yet; skip this region pre-syscall.
            return None;
        }
        return if result < 0 {
            Some(0)
        } else {
            Some(result as usize)
        };
    }
    let idx = spec.params.iter().position(|p| p.name == len_param)?;
    let raw = frame.args[idx];
    // Defensive cap — the recorder isn't going to read 4 EiB of
    // memory because someone passed an u64::MAX as the length.
    if raw > BUFFER_CAP as u64 {
        return Some(BUFFER_CAP);
    }
    // For signed scalars the high half is sign-extension, but
    // the kernel doesn't accept negative buffer lengths anyway.
    Some(match scalar_of(spec, idx) {
        Some(ScalarKind::I32 | ScalarKind::I64 | ScalarKind::ISize) if (raw as i64) < 0 => 0,
        _ => raw as usize,
    })
}

fn scalar_of(spec: &SyscallSpec, idx: usize) -> Option<ScalarKind> {
    if let ParamKind::Scalar(k) = spec.params[idx].kind {
        Some(k)
    } else {
        None
    }
}

fn push_buf_region(
    out: &mut Vec<CapturedRegion>,
    idx: usize,
    addr: u64,
    len: usize,
    kind: CapturedKind,
    reader: &dyn MemoryReader,
) {
    if addr == 0 || len == 0 {
        return;
    }
    let bytes = reader.read(addr, len);
    out.push(CapturedRegion {
        arg_idx: idx as u8,
        addr,
        bytes,
        requested_len: len,
        kind,
    });
}

fn push_cstr_region(
    out: &mut Vec<CapturedRegion>,
    idx: usize,
    addr: u64,
    reader: &dyn MemoryReader,
) {
    if addr == 0 {
        return;
    }
    let raw = reader.read(addr, CSTR_CAP);
    let nul = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    out.push(CapturedRegion {
        arg_idx: idx as u8,
        addr,
        bytes: raw[..nul].to_vec(),
        requested_len: CSTR_CAP,
        kind: CapturedKind::InCStr,
    });
}

/// Heuristic for "this u64 register looks like a userspace
/// pointer" — the catch-all uses this to decide whether to read
/// a window of bytes around an arg.
///
/// Linux x86-64 user-space sits in `[0x10000, 0x7fff_ffff_ffff]`
/// post-ASLR; addresses in that range are very likely pointers.
/// The recorder doesn't *need* to be precise — false positives
/// just produce harmless garbage windows; false negatives miss a
/// region that catch-all would have captured. Tunable cap on the
/// upper end avoids confusing kernel addresses (top half) with
/// user pointers.
pub fn looks_like_user_pointer(v: u64) -> bool {
    const USER_LO: u64 = 0x1000;
    const USER_HI: u64 = 0x0000_7fff_ffff_ffff;
    (USER_LO..=USER_HI).contains(&v)
}

fn push_catch_all_regions(
    out: &mut Vec<CapturedRegion>,
    frame: &CallFrame,
    reader: &dyn MemoryReader,
) {
    for (idx, &v) in frame.args.iter().enumerate() {
        if !looks_like_user_pointer(v) {
            continue;
        }
        let bytes = reader.read(v, CATCH_ALL_WINDOW);
        out.push(CapturedRegion {
            arg_idx: idx as u8,
            addr: v,
            bytes,
            requested_len: CATCH_ALL_WINDOW,
            kind: CapturedKind::CatchAll,
        });
    }
}

// ---------------------------------------------------------------------------
// Wire encoding
// ---------------------------------------------------------------------------
//
// The captured regions ride inside `Event::Syscall::output` as a
// length-prefixed byte stream. Format:
//
//   u32 LE: region count
//   per region:
//     u8: arg_idx
//     u8: kind (InBuf=0 / OutBuf=1 / InCStr=2 / CatchAll=3)
//     u64 LE: addr
//     u32 LE: requested_len
//     u32 LE: bytes_len
//     [u8; bytes_len]: bytes
//   u8: tier (Curated=0 / LongTail=1 / Unknown=2)
//
// Versioning: bumping format-version is the only way to evolve
// this — the on-disk Event::Syscall::output is opaque to the
// format crate. The format-version policy in `format/version.rs`
// covers the upgrade story.

const KIND_IN_BUF: u8 = 0;
const KIND_OUT_BUF: u8 = 1;
const KIND_IN_CSTR: u8 = 2;
const KIND_CATCH_ALL: u8 = 3;

const TIER_CURATED: u8 = 0;
const TIER_LONG_TAIL: u8 = 1;
const TIER_UNKNOWN: u8 = 2;

impl CapturedSyscall {
    /// Encode this capture into the byte string the trace
    /// stores as [`crate::format::event::Event::Syscall::output`].
    pub fn encode_output(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.regions.len() as u32).to_le_bytes());
        for r in &self.regions {
            buf.push(r.arg_idx);
            buf.push(match r.kind {
                CapturedKind::InBuf => KIND_IN_BUF,
                CapturedKind::OutBuf => KIND_OUT_BUF,
                CapturedKind::InCStr => KIND_IN_CSTR,
                CapturedKind::CatchAll => KIND_CATCH_ALL,
            });
            buf.extend_from_slice(&r.addr.to_le_bytes());
            buf.extend_from_slice(&(r.requested_len as u32).to_le_bytes());
            buf.extend_from_slice(&(r.bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&r.bytes);
        }
        buf.push(match self.tier {
            CaptureTier::Curated => TIER_CURATED,
            CaptureTier::LongTail => TIER_LONG_TAIL,
            CaptureTier::Unknown => TIER_UNKNOWN,
        });
        buf
    }

    /// Decode the byte string previously produced by
    /// [`Self::encode_output`]. The non-region fields (`nr`,
    /// `args`, `result`) come from [`crate::format::event::Event::Syscall`]
    /// — they aren't redundantly stored in the inner blob.
    pub fn decode_output(
        nr: u32,
        args: [u64; 6],
        result: i64,
        bytes: &[u8],
    ) -> Result<Self, DecodeError> {
        let mut cur = ByteReader::new(bytes);
        let count = cur.u32()? as usize;
        let mut regions = Vec::with_capacity(count);
        for _ in 0..count {
            let arg_idx = cur.u8()?;
            let kind_raw = cur.u8()?;
            let addr = cur.u64()?;
            let requested_len = cur.u32()? as usize;
            let bytes_len = cur.u32()? as usize;
            let bytes = cur.take(bytes_len)?.to_vec();
            let kind = match kind_raw {
                KIND_IN_BUF => CapturedKind::InBuf,
                KIND_OUT_BUF => CapturedKind::OutBuf,
                KIND_IN_CSTR => CapturedKind::InCStr,
                KIND_CATCH_ALL => CapturedKind::CatchAll,
                other => return Err(DecodeError::UnknownKind(other)),
            };
            regions.push(CapturedRegion {
                arg_idx,
                addr,
                bytes,
                requested_len,
                kind,
            });
        }
        let tier_raw = cur.u8()?;
        let tier = match tier_raw {
            TIER_CURATED => CaptureTier::Curated,
            TIER_LONG_TAIL => CaptureTier::LongTail,
            TIER_UNKNOWN => CaptureTier::Unknown,
            other => return Err(DecodeError::UnknownTier(other)),
        };
        if !cur.is_empty() {
            return Err(DecodeError::TrailingBytes(cur.remaining()));
        }
        Ok(Self {
            nr,
            args,
            result,
            regions,
            tier,
        })
    }
}

/// Errors arising from [`CapturedSyscall::decode_output`].
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer ended before the record claimed it would.
    #[error("captured-syscall blob truncated at offset {offset}")]
    Truncated {
        /// Where the truncation was detected.
        offset: usize,
    },
    /// Captured-region kind tag wasn't one of the documented
    /// values.
    #[error("unknown CapturedKind tag {0}")]
    UnknownKind(u8),
    /// Tier tag wasn't one of the documented values.
    #[error("unknown CaptureTier tag {0}")]
    UnknownTier(u8),
    /// Caller supplied more bytes than the record consumed —
    /// the producer is mismatched with the consumer.
    #[error("captured-syscall blob has {0} unread trailing bytes")]
    TrailingBytes(usize),
}

struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(DecodeError::Truncated { offset: self.pos })?;
        if end > self.bytes.len() {
            return Err(DecodeError::Truncated { offset: self.pos });
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }
    fn is_empty(&self) -> bool {
        self.pos == self.bytes.len()
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Mock memory reader — keyed by a (base, len) range; the
    /// test pre-loads what it expects the recorder to read.
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
            // Find the largest base ≤ addr.
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

    #[test]
    fn classify_curated_long_tail_unknown() {
        assert!(matches!(classify(0), Tier::Curated(_))); // read
        assert!(matches!(classify(157), Tier::LongTail(_))); // prctl
        assert!(matches!(classify(9999), Tier::Unknown));
    }

    #[test]
    fn write_pre_syscall_captures_in_buf_using_count_param() {
        // `write(fd=2, buf=0xCAFEBA00, count=5)`
        let frame = CallFrame {
            nr: 1,
            args: [2, 0xCAFEBA00, 5, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xCAFEBA00, b"hello\0worldXX");

        let cap = capture_pre_syscall(frame, &mem);
        assert_eq!(cap.tier, CaptureTier::Curated);
        assert_eq!(cap.regions.len(), 1);
        let r = &cap.regions[0];
        assert_eq!(r.arg_idx, 1);
        assert_eq!(r.addr, 0xCAFEBA00);
        assert_eq!(r.requested_len, 5);
        assert_eq!(r.kind, CapturedKind::InBuf);
        assert_eq!(r.bytes, b"hello");
    }

    #[test]
    fn read_post_syscall_uses_ret_for_buf_length() {
        // `read(fd=3, buf=0xDEADBEEF00, count=4096) -> 11`.
        let frame = CallFrame {
            nr: 0,
            args: [3, 0xDEADBEEF00, 4096, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xDEADBEEF00, b"hello world!!!");

        let cap = capture_post_syscall(frame, 11, &mem);
        assert_eq!(cap.tier, CaptureTier::Curated);
        assert_eq!(cap.regions.len(), 1);
        let r = &cap.regions[0];
        assert_eq!(r.kind, CapturedKind::OutBuf);
        assert_eq!(r.requested_len, 11);
        assert_eq!(r.bytes, b"hello world");
    }

    #[test]
    fn read_with_negative_return_captures_zero_bytes() {
        let frame = CallFrame {
            nr: 0,
            args: [3, 0xDEADBEEF00, 4096, 0, 0, 0],
        };
        let cap = capture_post_syscall(frame, -1, &Mock::default());
        // `requested_len = 0` skips the read entirely (no region pushed).
        assert!(cap.regions.is_empty());
    }

    #[test]
    fn open_pre_syscall_captures_pathname_cstr_to_nul() {
        let frame = CallFrame {
            nr: 2,
            args: [0xAAAA0000, 0, 0, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xAAAA0000, b"/tmp/file\0extra-junk");

        let cap = capture_pre_syscall(frame, &mem);
        assert_eq!(cap.regions.len(), 1);
        assert_eq!(cap.regions[0].kind, CapturedKind::InCStr);
        assert_eq!(cap.regions[0].bytes, b"/tmp/file");
    }

    #[test]
    fn long_tail_falls_back_to_catch_all() {
        // prctl(option=PR_SET_NAME=15, arg2=0xC0FFEE) — option is
        // not a pointer (looks_like_user_pointer rejects 15),
        // arg2 looks pointer-shaped.
        let frame = CallFrame {
            nr: 157,
            args: [15, 0xC0FFEE, 0, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xC0FFEE, b"trial-name\0");

        let cap = capture_pre_syscall(frame, &mem);
        assert_eq!(cap.tier, CaptureTier::LongTail);
        assert_eq!(cap.regions.len(), 1);
        assert_eq!(cap.regions[0].kind, CapturedKind::CatchAll);
        assert_eq!(cap.regions[0].arg_idx, 1);
        assert_eq!(cap.regions[0].requested_len, CATCH_ALL_WINDOW);
    }

    #[test]
    fn unknown_syscall_uses_catch_all_with_unknown_tier() {
        let frame = CallFrame {
            nr: 9999,
            args: [0xCAFE0000, 1234, 0xBEEF0000, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xCAFE0000, b"first-buf");
        mem.put(0xBEEF0000, b"second-buf");

        let cap = capture_pre_syscall(frame, &mem);
        assert_eq!(cap.tier, CaptureTier::Unknown);
        // Two pointer-shaped args (idx 0 and 2); idx 1 is a small
        // scalar that fails the heuristic.
        assert_eq!(cap.regions.len(), 2);
        assert_eq!(cap.regions[0].arg_idx, 0);
        assert_eq!(cap.regions[1].arg_idx, 2);
    }

    #[test]
    fn looks_like_user_pointer_thresholds() {
        assert!(!looks_like_user_pointer(0));
        assert!(!looks_like_user_pointer(0xFFF));
        assert!(looks_like_user_pointer(0x1000));
        assert!(looks_like_user_pointer(0x7fff_ffff_0000));
        // Kernel half — not a userspace pointer.
        assert!(!looks_like_user_pointer(0xFFFF_FFFF_8000_0000));
    }

    #[test]
    fn buffer_cap_clamps_an_oversized_write() {
        // write(fd=1, buf=…, count=u64::MAX) — recorder must not
        // try to read 16 EiB.
        let frame = CallFrame {
            nr: 1,
            args: [1, 0x1000, u64::MAX, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0x1000, &vec![0xABu8; BUFFER_CAP * 2]);
        let cap = capture_pre_syscall(frame, &mem);
        assert_eq!(cap.regions.len(), 1);
        assert_eq!(cap.regions[0].requested_len, BUFFER_CAP);
        assert_eq!(cap.regions[0].bytes.len(), BUFFER_CAP);
    }

    #[test]
    fn encode_decode_round_trip_curated_write() {
        let frame = CallFrame {
            nr: 1,
            args: [2, 0xCAFEBA00, 5, 0, 0, 0],
        };
        let mut mem = Mock::default();
        mem.put(0xCAFEBA00, b"hello");
        let pre = capture_pre_syscall(frame, &mem);

        let out = pre.encode_output();
        let back =
            CapturedSyscall::decode_output(pre.nr, pre.args, pre.result, &out).expect("decode");
        assert_eq!(pre, back);
    }

    #[test]
    fn decode_rejects_unknown_kind() {
        // count=0 regions, then tier — but stuff a stray unknown
        // kind into the regions section.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0); // arg_idx
        buf.push(99); // kind — not a defined tag
        buf.extend_from_slice(&0u64.to_le_bytes()); // addr
        buf.extend_from_slice(&0u32.to_le_bytes()); // requested_len
        buf.extend_from_slice(&0u32.to_le_bytes()); // bytes_len
        buf.push(TIER_CURATED);
        let err = CapturedSyscall::decode_output(0, [0; 6], 0, &buf).unwrap_err();
        assert_eq!(err, DecodeError::UnknownKind(99));
    }

    #[test]
    fn decode_rejects_truncated_buffer() {
        let buf = [0u8; 2]; // not enough for a u32 region count
        let err = CapturedSyscall::decode_output(0, [0; 6], 0, &buf).unwrap_err();
        assert_eq!(err, DecodeError::Truncated { offset: 0 });
    }

    #[test]
    fn null_buf_pointer_skips_region() {
        let frame = CallFrame {
            nr: 1,
            args: [1, 0, 5, 0, 0, 0],
        };
        let cap = capture_pre_syscall(frame, &Mock::default());
        assert!(cap.regions.is_empty());
    }
}

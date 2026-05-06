// SPDX-License-Identifier: MIT
//! Linux syscall specifications for the sub-phase 3B recorder.
//!
//! Plain-data tables describing what each syscall takes, what it
//! returns, and which arguments are pointers the recorder must
//! read out via `process_vm_readv`. The `syscall! { … }` DSL
//! macro (separate proc-macro crate, lands in step 52) parses
//! source-level entries into this shape; recorder + replayer
//! crates iterate the resulting `&[SyscallSpec]` to dispatch.
//!
//! See `doc/plans/phase-5-time-travel.md` § "3B. Single-threaded
//! syscall record" for the macro-driven coverage strategy this
//! crate underpins.

#![deny(missing_docs)]

/// One Linux syscall — name, ABI number, parameter list, return.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SyscallSpec {
    /// `man 2`-style name (`read`, `write`, `openat`, …).
    pub name: &'static str,
    /// `__NR_*` syscall number on the target ABI. Linux x86-64
    /// for the curated entries below; aarch64 numbers diverge,
    /// so a future port adds a parallel table.
    pub nr: u32,
    /// Parameters in ABI order (RDI/RSI/RDX/R10/R8/R9 on x86-64).
    /// At most 6 — the kernel's syscall ABI never uses more.
    pub params: &'static [Param],
    /// Return-value shape.
    pub ret: ReturnKind,
}

/// One syscall parameter.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Param {
    /// Parameter name as written in `man 2 X`. Used by
    /// pointer-len references — `OutBuf { len_param: "count" }`
    /// names the parameter holding the length, not the length
    /// itself.
    pub name: &'static str,
    /// What kind of argument this is (scalar, pointer, etc.).
    pub kind: ParamKind,
}

/// Argument shape — what the recorder needs to know to capture
/// inputs and outputs.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ParamKind {
    /// Plain scalar value (passed by register).
    Scalar(ScalarKind),
    /// File descriptor — recorded as an `i32` but flagged
    /// distinctly so analysis tools can map between record and
    /// replay fd numbers if they diverge.
    Fd,
    /// Pointer to bytes the kernel *reads* (e.g. `write(buf)`).
    /// The recorder reads `len_param` bytes from `buf` *before*
    /// the syscall returns and stores them in the trace; replay
    /// re-establishes those bytes in the tracee at the same
    /// address before letting the syscall proceed.
    InBuf {
        /// Name of the parameter holding the byte count.
        /// Conventionally another scalar in the same syscall.
        /// Special token `"ret"` means "use the syscall's return
        /// value as the length" — common for `read`/`recv`.
        len_param: &'static str,
    },
    /// Pointer to bytes the kernel *writes* (e.g. `read(buf)`).
    /// On record the recorder reads `len_param` bytes back from
    /// `buf` *after* the syscall returns and logs them; on
    /// replay we write the recorded bytes to `buf` instead of
    /// running the syscall.
    OutBuf {
        /// Same convention as [`ParamKind::InBuf::len_param`].
        len_param: &'static str,
    },
    /// Null-terminated C string (`*const c_char`). The recorder
    /// captures up to a sane cap (e.g. PATH_MAX = 4096) — paths
    /// are rarely longer.
    InCStr,
    /// Generic pointer with no recorder-known shape. Captured
    /// only by raw u64 value; replay assumes the same pointer
    /// remains valid (true for kernel-managed pointers like
    /// vDSO entry points).
    OpaquePtr,
}

/// Scalar parameter widths — match the kernel ABI.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(missing_docs)] // self-evident enum variants
pub enum ScalarKind {
    U32,
    U64,
    I32,
    I64,
    USize,
    ISize,
}

/// Return-value shape. Most syscalls return `isize` (negative is
/// `-errno`), but a few diverge.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum ReturnKind {
    /// Most read/write/io syscalls.
    Isize,
    /// `open`/`close`/`socket` etc.
    I32,
    /// `mmap`/`brk` return raw addresses.
    U64,
    /// `exit`/`exit_group` never return.
    Never,
}

/// Curated subset of Linux x86-64 syscalls the recorder ships
/// with hand-vetted parameter shapes. The long-tail strategy
/// (machine-extracted table + generic catch-all) is documented
/// in the plan; until that lands, anything not in this table
/// is a recorder TODO.
///
/// Syscall numbers are the canonical x86-64 `__NR_*` values; see
/// `arch/x86/entry/syscalls/syscall_64.tbl` in the kernel
/// source.
pub const KNOWN_X86_64: &[SyscallSpec] = &[
    SyscallSpec {
        name: "read",
        nr: 0,
        params: &[
            Param { name: "fd", kind: ParamKind::Fd },
            Param {
                name: "buf",
                kind: ParamKind::OutBuf { len_param: "ret" },
            },
            Param {
                name: "count",
                kind: ParamKind::Scalar(ScalarKind::USize),
            },
        ],
        ret: ReturnKind::Isize,
    },
    SyscallSpec {
        name: "write",
        nr: 1,
        params: &[
            Param { name: "fd", kind: ParamKind::Fd },
            Param {
                name: "buf",
                kind: ParamKind::InBuf { len_param: "count" },
            },
            Param {
                name: "count",
                kind: ParamKind::Scalar(ScalarKind::USize),
            },
        ],
        ret: ReturnKind::Isize,
    },
    SyscallSpec {
        name: "open",
        nr: 2,
        params: &[
            Param { name: "pathname", kind: ParamKind::InCStr },
            Param { name: "flags", kind: ParamKind::Scalar(ScalarKind::I32) },
            Param { name: "mode", kind: ParamKind::Scalar(ScalarKind::U32) },
        ],
        ret: ReturnKind::I32,
    },
    SyscallSpec {
        name: "close",
        nr: 3,
        params: &[Param { name: "fd", kind: ParamKind::Fd }],
        ret: ReturnKind::I32,
    },
    SyscallSpec {
        name: "mmap",
        nr: 9,
        params: &[
            Param { name: "addr", kind: ParamKind::OpaquePtr },
            Param { name: "length", kind: ParamKind::Scalar(ScalarKind::USize) },
            Param { name: "prot", kind: ParamKind::Scalar(ScalarKind::I32) },
            Param { name: "flags", kind: ParamKind::Scalar(ScalarKind::I32) },
            Param { name: "fd", kind: ParamKind::Fd },
            Param { name: "offset", kind: ParamKind::Scalar(ScalarKind::I64) },
        ],
        ret: ReturnKind::U64,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper — find the spec by name.
    fn lookup(name: &str) -> Option<&'static SyscallSpec> {
        KNOWN_X86_64.iter().find(|s| s.name == name)
    }

    #[test]
    fn no_duplicate_syscall_numbers() {
        let mut seen = std::collections::HashSet::new();
        for s in KNOWN_X86_64 {
            assert!(
                seen.insert(s.nr),
                "duplicate __NR_{} number {} (also used by another spec)",
                s.name, s.nr,
            );
        }
    }

    #[test]
    fn no_duplicate_syscall_names() {
        let mut seen = std::collections::HashSet::new();
        for s in KNOWN_X86_64 {
            assert!(
                seen.insert(s.name),
                "duplicate spec for `{}`",
                s.name,
            );
        }
    }

    #[test]
    fn buffer_len_params_reference_real_param_names_or_ret() {
        for s in KNOWN_X86_64 {
            for p in s.params {
                let len_ref = match p.kind {
                    ParamKind::InBuf { len_param }
                    | ParamKind::OutBuf { len_param } => Some(len_param),
                    _ => None,
                };
                if let Some(name) = len_ref {
                    if name == "ret" {
                        continue; // sentinel — use syscall return
                    }
                    assert!(
                        s.params.iter().any(|q| q.name == name),
                        "spec `{}` parameter `{}` references unknown len_param `{}`",
                        s.name, p.name, name,
                    );
                }
            }
        }
    }

    #[test]
    fn at_most_six_params_per_syscall() {
        for s in KNOWN_X86_64 {
            assert!(
                s.params.len() <= 6,
                "spec `{}` has {} params; kernel ABI is limited to 6",
                s.name,
                s.params.len(),
            );
        }
    }

    #[test]
    fn read_uses_ret_as_buf_length() {
        // `read(fd, buf, count)` returns the actual byte count;
        // the recorder must use `ret` (not `count`) to know how
        // much to log from `buf`. count is the buffer size, not
        // the bytes-actually-written count.
        let read = lookup("read").unwrap();
        let buf = read.params.iter().find(|p| p.name == "buf").unwrap();
        match buf.kind {
            ParamKind::OutBuf { len_param } => assert_eq!(len_param, "ret"),
            other => panic!("read.buf should be OutBuf, got {other:?}"),
        }
    }

    #[test]
    fn write_uses_count_as_buf_length() {
        // `write(fd, buf, count)` — recorder logs `count` bytes
        // from `buf` *before* the syscall; the kernel may write
        // fewer (return < count) but those count bytes are what
        // the program intended to write.
        let write = lookup("write").unwrap();
        let buf = write.params.iter().find(|p| p.name == "buf").unwrap();
        match buf.kind {
            ParamKind::InBuf { len_param } => assert_eq!(len_param, "count"),
            other => panic!("write.buf should be InBuf, got {other:?}"),
        }
    }

    #[test]
    fn known_syscall_numbers_match_kernel_canonical() {
        // From arch/x86/entry/syscalls/syscall_64.tbl. Spot-check
        // the famous low-numbered ones — anyone editing this
        // table incorrectly hits this test.
        assert_eq!(lookup("read").unwrap().nr, 0);
        assert_eq!(lookup("write").unwrap().nr, 1);
        assert_eq!(lookup("open").unwrap().nr, 2);
        assert_eq!(lookup("close").unwrap().nr, 3);
        assert_eq!(lookup("mmap").unwrap().nr, 9);
    }
}

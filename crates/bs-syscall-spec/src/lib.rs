// SPDX-License-Identifier: MIT
//! Linux syscall specifications for the sub-phase 3B recorder.
//!
//! Plain-data tables describing what each syscall takes, what it
//! returns, and which arguments are pointers the recorder must
//! read out via `process_vm_readv`. The `syscall! { … }` DSL
//! macro (`bs-syscall-macro`) parses source-level entries into
//! this shape; recorder + replayer crates iterate the resulting
//! `&[SyscallSpec]` to dispatch.
//!
//! See `doc/plans/phase-5-time-travel.md` § "3B. Single-threaded
//! syscall record" for the macro-driven coverage strategy this
//! crate underpins.
//!
//! ## Coverage tiers
//!
//! 1. **Curated** — [`KNOWN_X86_64`] hand-vetted via the
//!    `syscall!` macro. Hot syscalls, precise pointer-arg shapes.
//! 2. **Long tail** — generic table emitted at build time from
//!    the kernel's `arch/x86/entry/syscalls/syscall_64.tbl` (see
//!    `bs-syscall-spec/build.rs`). Name + number + arg arity
//!    only; recorder pairs it with the catch-all primitive.
//! 3. **Catch-all** — for syscalls that fall off the end of
//!    both, the recorder logs the six argument registers plus a
//!    fixed window of bytes around any user-space pointer-shaped
//!    arg. Bounded loud failure on replay if the program uses
//!    one with weird out-pointer shape.

#![deny(missing_docs)]

// The `syscall!` proc-macro emits absolute `::bs_syscall_spec::…`
// paths so external crates can use it without an explicit `use`.
// Inside *this* crate the same identifier doesn't resolve unless
// we register the crate-level alias for itself.
extern crate self as bs_syscall_spec;

use bs_syscall_macro::syscall;

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
    /// only by raw u64 value; the catch-all primitive logs a
    /// fixed window of bytes around it for diagnostics.
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

/// Long-tail entry — name + number only. The recorder pairs
/// these with the catch-all primitive (six argument registers
/// + a fixed window of bytes around any pointer-shaped arg).
///
/// Generated at build time from `data/syscall_64.tbl` by
/// `build.rs`. The file format is documented in the data file's
/// header; updating means appending a `<nr> <name>` row and
/// rebuilding.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct GenericSyscall {
    /// `__NR_*` syscall number on x86-64.
    pub nr: u32,
    /// `man 2`-style name.
    pub name: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/long_tail_x86_64.rs"));

/// Look up the long-tail entry for an x86-64 syscall number.
/// Falls through to the catch-all if even the long-tail table
/// has no name for the given `nr`.
pub fn lookup_long_tail_x86_64(nr: u32) -> Option<&'static GenericSyscall> {
    // Long tail is sorted by `nr` (build.rs validates this) so a
    // binary search is cheap and stable.
    LONG_TAIL_X86_64
        .binary_search_by_key(&nr, |g| g.nr)
        .ok()
        .map(|i| &LONG_TAIL_X86_64[i])
}

/// Best-effort name lookup — try the curated table first, fall
/// back to the long tail. Returns `None` only when neither
/// knows the syscall (catch-all territory; the recorder logs
/// `syscall_<nr>`).
pub fn name_for_x86_64(nr: u32) -> Option<&'static str> {
    if let Some(s) = lookup_x86_64(nr) {
        return Some(s.name);
    }
    lookup_long_tail_x86_64(nr).map(|g| g.name)
}

/// Curated subset of Linux x86-64 syscalls the recorder ships
/// with hand-vetted parameter shapes. The `syscall! { … }` macro
/// expands this list into a `&[SyscallSpec]` const-initializer
/// at compile time so the recorder and replayer share one
/// authoritative source — there's no second hand-rolled table to
/// drift out of sync.
///
/// Coverage target: the ~30 syscalls real Rust programs spend
/// most of their time in. Anything not here falls through to
/// the long-tail table or the catch-all primitive.
///
/// Syscall numbers are the canonical x86-64 `__NR_*` values; see
/// `arch/x86/entry/syscalls/syscall_64.tbl` in the kernel
/// source.
pub const KNOWN_X86_64: &[SyscallSpec] = syscall! {
    // -- core I/O ---------------------------------------------------------
    read[0](fd: fd, buf: out_buf(len = ret), count: usize) -> isize;
    write[1](fd: fd, buf: in_buf(len = count), count: usize) -> isize;
    open[2](pathname: cstr, flags: i32, mode: u32) -> i32;
    close[3](fd: fd) -> i32;
    lseek[8](fd: fd, offset: i64, whence: i32) -> isize;
    pread64[17](fd: fd, buf: out_buf(len = ret), count: usize, offset: i64) -> isize;
    pwrite64[18](fd: fd, buf: in_buf(len = count), count: usize, offset: i64) -> isize;
    readv[19](fd: fd, iov: ptr, iovcnt: i32) -> isize;
    writev[20](fd: fd, iov: ptr, iovcnt: i32) -> isize;

    // -- memory -----------------------------------------------------------
    mmap[9](addr: ptr, length: usize, prot: i32, flags: i32, fd: fd, offset: i64) -> u64;
    mprotect[10](addr: ptr, length: usize, prot: i32) -> i32;
    munmap[11](addr: ptr, length: usize) -> i32;
    brk[12](addr: u64) -> u64;

    // -- ioctl & friends --------------------------------------------------
    ioctl[16](fd: fd, request: u64, arg: ptr) -> isize;
    fcntl[72](fd: fd, cmd: i32, arg: u64) -> isize;

    // -- process & lifecycle ---------------------------------------------
    nanosleep[35](req: ptr, rem: ptr) -> i32;
    getpid[39](/* no params */) -> i32;
    clone[56](flags: u64, child_stack: ptr, ptid: ptr, ctid: ptr, tls: u64) -> isize;
    fork[57](/* no params */) -> i32;
    execve[59](filename: cstr, argv: ptr, envp: ptr) -> i32;
    exit[60](status: i32) -> never;
    wait4[61](pid: i32, wstatus: ptr, options: i32, rusage: ptr) -> i32;
    exit_group[231](status: i32) -> never;

    // -- threading & sync -------------------------------------------------
    futex[202](uaddr: ptr, op: i32, val: u32, timeout: ptr, uaddr2: ptr, val3: u32) -> isize;
    gettid[186](/* no params */) -> i32;

    // -- time -------------------------------------------------------------
    clock_gettime[228](clk_id: i32, tp: ptr) -> i32;

    // -- random -----------------------------------------------------------
    getrandom[318](buf: out_buf(len = ret), buflen: usize, flags: u32) -> isize;

    // -- modern file ops --------------------------------------------------
    openat[257](dirfd: fd, pathname: cstr, flags: i32, mode: u32) -> i32;

    // -- io_uring trio ----------------------------------------------------
    io_uring_setup[425](entries: u32, params: ptr) -> i32;
    io_uring_enter[426](fd: fd, to_submit: u32, min_complete: u32, flags: u32, sig: ptr, sigsz: usize) -> i32;
    io_uring_register[427](fd: fd, opcode: u32, arg: ptr, nr_args: u32) -> i32;
};

/// Look up the curated [`SyscallSpec`] for an x86-64 syscall
/// number. Returns `None` for syscalls outside the curated set
/// — callers should fall back to the long-tail table or the
/// catch-all primitive.
pub fn lookup_x86_64(nr: u32) -> Option<&'static SyscallSpec> {
    KNOWN_X86_64.iter().find(|s| s.nr == nr)
}

/// Look up by name. Useful for tests and tooling.
pub fn lookup_x86_64_by_name(name: &str) -> Option<&'static SyscallSpec> {
    KNOWN_X86_64.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let read = lookup_x86_64_by_name("read").unwrap();
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
        let write = lookup_x86_64_by_name("write").unwrap();
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
        assert_eq!(lookup_x86_64_by_name("read").unwrap().nr, 0);
        assert_eq!(lookup_x86_64_by_name("write").unwrap().nr, 1);
        assert_eq!(lookup_x86_64_by_name("open").unwrap().nr, 2);
        assert_eq!(lookup_x86_64_by_name("close").unwrap().nr, 3);
        assert_eq!(lookup_x86_64_by_name("mmap").unwrap().nr, 9);
        assert_eq!(lookup_x86_64_by_name("brk").unwrap().nr, 12);
        assert_eq!(lookup_x86_64_by_name("clone").unwrap().nr, 56);
        assert_eq!(lookup_x86_64_by_name("execve").unwrap().nr, 59);
        assert_eq!(lookup_x86_64_by_name("exit").unwrap().nr, 60);
        assert_eq!(lookup_x86_64_by_name("futex").unwrap().nr, 202);
        assert_eq!(lookup_x86_64_by_name("exit_group").unwrap().nr, 231);
        assert_eq!(lookup_x86_64_by_name("openat").unwrap().nr, 257);
        assert_eq!(lookup_x86_64_by_name("io_uring_setup").unwrap().nr, 425);
        assert_eq!(lookup_x86_64_by_name("io_uring_enter").unwrap().nr, 426);
        assert_eq!(lookup_x86_64_by_name("io_uring_register").unwrap().nr, 427);
    }

    #[test]
    fn curated_set_covers_at_least_thirty_syscalls() {
        assert!(
            KNOWN_X86_64.len() >= 30,
            "curated table dropped below 30 entries (now {}); 3B coverage target needs ≥30",
            KNOWN_X86_64.len(),
        );
    }

    #[test]
    fn lookup_by_nr_round_trips() {
        for s in KNOWN_X86_64 {
            let by_nr = lookup_x86_64(s.nr).expect("missing by-nr lookup");
            assert_eq!(by_nr.name, s.name);
            assert_eq!(by_nr.nr, s.nr);
        }
    }

    #[test]
    fn long_tail_table_is_sorted_and_nontrivial() {
        // build.rs guarantees this; the test is a tripwire that
        // catches an editor's stray reorder before it reaches a
        // user.
        assert!(
            LONG_TAIL_X86_64.len() >= 200,
            "long-tail table dropped to {} entries; expected ≥200 \
             for Linux ABI baseline coverage",
            LONG_TAIL_X86_64.len(),
        );
        for w in LONG_TAIL_X86_64.windows(2) {
            assert!(
                w[0].nr < w[1].nr,
                "long-tail not sorted: {} (nr {}) before {} (nr {})",
                w[0].name, w[0].nr, w[1].name, w[1].nr,
            );
        }
    }

    #[test]
    fn long_tail_lookup_finds_well_known_syscalls() {
        for (nr, name) in [
            (1, "write"),
            (60, "exit"),
            (157, "prctl"),
            (231, "exit_group"),
            (317, "seccomp"),
            (435, "clone3"),
            (439, "faccessat2"),
            (449, "futex_waitv"),
        ] {
            let g = lookup_long_tail_x86_64(nr)
                .unwrap_or_else(|| panic!("long-tail missing __NR_{nr} ({name})"));
            assert_eq!(g.name, name, "long-tail entry for {nr} mis-labelled");
        }
    }

    #[test]
    fn name_for_x86_64_prefers_curated_then_long_tail() {
        // `read` lives in both tables (curated overrides long
        // tail). `prctl` is long-tail-only.
        assert_eq!(name_for_x86_64(0), Some("read"));
        assert_eq!(name_for_x86_64(157), Some("prctl"));
        // 4096 is well past the highest defined nr — neither
        // table covers it.
        assert_eq!(name_for_x86_64(4096), None);
    }

    #[test]
    fn long_tail_does_not_contradict_curated() {
        // For every curated entry that also has a long-tail
        // peer, the names agree. Catches a typo in either table
        // before it produces a confusing trace event.
        for s in KNOWN_X86_64 {
            if let Some(g) = lookup_long_tail_x86_64(s.nr) {
                assert_eq!(
                    g.name, s.name,
                    "name mismatch at nr {}: curated says `{}`, long tail says `{}`",
                    s.nr, s.name, g.name,
                );
            }
        }
    }

    #[test]
    fn never_returning_syscalls_have_never_kind() {
        for name in ["exit", "exit_group"] {
            let s = lookup_x86_64_by_name(name)
                .unwrap_or_else(|| panic!("`{name}` should be in the curated table"));
            assert_eq!(
                s.ret, ReturnKind::Never,
                "`{name}` must be ReturnKind::Never (does not return)",
            );
        }
    }
}

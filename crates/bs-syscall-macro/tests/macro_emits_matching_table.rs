// SPDX-License-Identifier: MIT
//! The macro emits the same shape as `bs_syscall_spec::KNOWN_X86_64`'s
//! hand-built table. Byte-compare to keep the macro and the curated
//! reference table in lock-step.

use bs_syscall_macro::syscall;
use bs_syscall_spec::{KNOWN_X86_64, SyscallSpec};

const FROM_MACRO: &[SyscallSpec] = syscall! {
    read[0](fd: fd, buf: out_buf(len = ret), count: usize) -> isize;
    write[1](fd: fd, buf: in_buf(len = count), count: usize) -> isize;
    open[2](pathname: cstr, flags: i32, mode: u32) -> i32;
    close[3](fd: fd) -> i32;
    mmap[9](addr: ptr, length: usize, prot: i32, flags: i32, fd: fd, offset: i64) -> u64;
};

#[test]
fn macro_table_matches_hand_built_table() {
    assert_eq!(
        FROM_MACRO.len(),
        KNOWN_X86_64.len(),
        "macro emitted {} entries, KNOWN_X86_64 has {}",
        FROM_MACRO.len(),
        KNOWN_X86_64.len(),
    );
    for (i, (a, b)) in FROM_MACRO.iter().zip(KNOWN_X86_64.iter()).enumerate() {
        assert_eq!(a.name, b.name, "entry {i} name mismatch");
        assert_eq!(a.nr, b.nr, "entry {i} nr mismatch");
        assert_eq!(a.ret, b.ret, "entry {i} ret mismatch");
        assert_eq!(
            a.params.len(),
            b.params.len(),
            "entry {} ({}) params count mismatch",
            i,
            a.name,
        );
        for (j, (pa, pb)) in a.params.iter().zip(b.params.iter()).enumerate() {
            assert_eq!(
                pa.name, pb.name,
                "entry {i} ({}) param {j} name mismatch",
                a.name,
            );
            assert_eq!(
                pa.kind, pb.kind,
                "entry {i} ({}) param {j} kind mismatch",
                a.name,
            );
        }
    }
}

#[test]
fn macro_supports_zero_param_syscall() {
    // close has one param, but a no-arg syscall (e.g. getpid) is
    // also valid syntax.
    const T: &[SyscallSpec] = syscall! {
        getpid[39]() -> i32;
    };
    assert_eq!(T.len(), 1);
    assert_eq!(T[0].name, "getpid");
    assert_eq!(T[0].nr, 39);
    assert!(T[0].params.is_empty());
}

#[test]
fn macro_supports_never_return() {
    const T: &[SyscallSpec] = syscall! {
        exit_group[231](status: i32) -> never;
    };
    assert_eq!(T[0].ret, bs_syscall_spec::ReturnKind::Never);
}

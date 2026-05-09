// SPDX-License-Identifier: MIT
//! Grammar coverage tests for the `syscall! { … }` DSL.
//!
//! Until step 3 the curated table was hand-rolled in
//! `bs_syscall_spec`, and this file's job was to byte-compare a
//! macro-emitted shadow table against it. With step 3 the table
//! itself is now macro-generated (`bs_syscall_spec` consumes
//! `syscall!`), so the comparison test would be circular. The
//! tests now exercise the macro grammar directly: every shape the
//! curated DSL uses must round-trip into the documented
//! [`bs_syscall_spec`] vocabulary.

use bs_syscall_macro::syscall;
use bs_syscall_spec::{KNOWN_X86_64, Param, ParamKind, ReturnKind, ScalarKind, SyscallSpec};

/// Reference shape — every grammar production exercised once.
/// Compared against an inline fixed expected slice so anyone
/// editing the macro's emit code finds out *which* production
/// drifted.
const REFERENCE: &[SyscallSpec] = syscall! {
    read[0](fd: fd, buf: out_buf(len = ret), count: usize) -> isize;
    write[1](fd: fd, buf: in_buf(len = count), count: usize) -> isize;
    open[2](pathname: cstr, flags: i32, mode: u32) -> i32;
    close[3](fd: fd) -> i32;
    mmap[9](addr: ptr, length: usize, prot: i32, flags: i32, fd: fd, offset: i64) -> u64;
};

#[test]
fn reference_set_emits_documented_shapes() {
    let expected: &[SyscallSpec] = &[
        SyscallSpec {
            name: "read",
            nr: 0,
            params: &[
                Param {
                    name: "fd",
                    kind: ParamKind::Fd,
                },
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
                Param {
                    name: "fd",
                    kind: ParamKind::Fd,
                },
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
                Param {
                    name: "pathname",
                    kind: ParamKind::InCStr,
                },
                Param {
                    name: "flags",
                    kind: ParamKind::Scalar(ScalarKind::I32),
                },
                Param {
                    name: "mode",
                    kind: ParamKind::Scalar(ScalarKind::U32),
                },
            ],
            ret: ReturnKind::I32,
        },
        SyscallSpec {
            name: "close",
            nr: 3,
            params: &[Param {
                name: "fd",
                kind: ParamKind::Fd,
            }],
            ret: ReturnKind::I32,
        },
        SyscallSpec {
            name: "mmap",
            nr: 9,
            params: &[
                Param {
                    name: "addr",
                    kind: ParamKind::OpaquePtr,
                },
                Param {
                    name: "length",
                    kind: ParamKind::Scalar(ScalarKind::USize),
                },
                Param {
                    name: "prot",
                    kind: ParamKind::Scalar(ScalarKind::I32),
                },
                Param {
                    name: "flags",
                    kind: ParamKind::Scalar(ScalarKind::I32),
                },
                Param {
                    name: "fd",
                    kind: ParamKind::Fd,
                },
                Param {
                    name: "offset",
                    kind: ParamKind::Scalar(ScalarKind::I64),
                },
            ],
            ret: ReturnKind::U64,
        },
    ];

    assert_eq!(
        REFERENCE.len(),
        expected.len(),
        "macro emitted unexpected entry count"
    );
    for (i, (a, b)) in REFERENCE.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a.name, b.name, "entry {i} name");
        assert_eq!(a.nr, b.nr, "entry {i} ({}) nr", a.name);
        assert_eq!(a.ret, b.ret, "entry {i} ({}) ret", a.name);
        assert_eq!(
            a.params.len(),
            b.params.len(),
            "entry {i} ({}) params count",
            a.name,
        );
        for (j, (pa, pb)) in a.params.iter().zip(b.params.iter()).enumerate() {
            assert_eq!(pa.name, pb.name, "entry {i} ({}) param {j} name", a.name);
            assert_eq!(pa.kind, pb.kind, "entry {i} ({}) param {j} kind", a.name);
        }
    }
}

#[test]
fn reference_subset_appears_in_curated_table() {
    // Every entry in the reference set must also be present in
    // the canonical curated table — REFERENCE is a strict subset.
    for r in REFERENCE {
        let canonical = bs_syscall_spec::lookup_x86_64(r.nr).unwrap_or_else(|| {
            panic!(
                "curated KNOWN_X86_64 missing entry for `{}` (nr={})",
                r.name, r.nr
            )
        });
        assert_eq!(
            canonical.name, r.name,
            "curated entry {} has different name than reference for nr {}",
            canonical.name, r.nr,
        );
    }
}

#[test]
fn macro_supports_zero_param_syscall() {
    // The curated table uses this for getpid/gettid/fork/etc.
    // Verifying explicitly here keeps the grammar covered even
    // if the curated set drops them later.
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
    assert_eq!(T[0].ret, ReturnKind::Never);
}

#[test]
fn curated_table_round_trip_through_lookup() {
    // Sanity: the spec crate's lookup helpers accept everything
    // the macro emitted. If the macro ever produced a stale `nr`
    // that the lookup couldn't find, this fails fast.
    for s in KNOWN_X86_64 {
        let by_nr = bs_syscall_spec::lookup_x86_64(s.nr)
            .unwrap_or_else(|| panic!("by-nr lookup failed for {} ({})", s.name, s.nr));
        let by_name = bs_syscall_spec::lookup_x86_64_by_name(s.name)
            .unwrap_or_else(|| panic!("by-name lookup failed for {}", s.name));
        assert!(std::ptr::eq(by_nr, by_name));
    }
}

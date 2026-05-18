<!-- markdownlint-disable MD041 -->
# bs-syscall-macro

`syscall! { … }` DSL proc-macro for BugStalker's Phase 5 recorder.

Generates `&[bs_syscall_spec::SyscallSpec]` const-initializer
expressions from a compact source-level grammar so the curated
syscall table has one source of truth.

## Grammar

```text
input        := entry (";" entry)* ";"?
entry        := IDENT "[" INT_LIT "]" "(" params ")" "->" ret
params       := /* empty */ | param ("," param)* ","?
param        := IDENT ":" kind
kind         := "fd"
              | "i32" | "u32" | "i64" | "u64" | "isize" | "usize"
              | "cstr"
              | "ptr"
              | "in_buf"  "(" "len" "=" IDENT ")"
              | "out_buf" "(" "len" "=" IDENT ")"
ret          := "isize" | "i32" | "u64" | "never"
```

`len = ret` (the literal identifier `ret`) is the sentinel for
"use the syscall's return value as the byte count" — see
`bs_syscall_spec::ParamKind::OutBuf` for the contract.

## Example

```rust
use bs_syscall_macro::syscall;
use bs_syscall_spec::SyscallSpec;

const TABLE: &[SyscallSpec] = syscall! {
    read[0](fd: fd, buf: out_buf(len = ret), count: usize) -> isize;
    write[1](fd: fd, buf: in_buf(len = count), count: usize) -> isize;
    open[2](pathname: cstr, flags: i32, mode: u32) -> i32;
    close[3](fd: fd) -> i32;
    mmap[9](addr: ptr, length: usize, prot: i32, flags: i32, fd: fd, offset: i64) -> u64;
    exit_group[231](status: i32) -> never;
};
```

The expansion produces a `SyscallSpec` record per entry, with
the macro's absolute paths resolving to `::bs_syscall_spec::*`
so external crates use the macro without needing `use` imports.

## Tests

`cargo nextest run -p bs-syscall-macro`. Five tests:
grammar coverage against an inline reference set, subset
membership against `KNOWN_X86_64`, zero-param syscall, never-
return, lookup round-trip.

## Trivia

The crate uses an unusual `dev-dependency` cycle:
`bs-syscall-spec` depends on `bs-syscall-macro` (normal),
`bs-syscall-macro` depends on `bs-syscall-spec` (dev). Cargo
allows this because dev-deps don't form part of the link graph.
The macro emits `::bs_syscall_spec::…` paths, and
`bs-syscall-spec` resolves them inside its own crate via
`extern crate self as bs_syscall_spec`.

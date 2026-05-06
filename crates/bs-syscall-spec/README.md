<!-- markdownlint-disable MD041 -->
# bs-syscall-spec

Linux syscall specifications for BugStalker's Phase 5 sub-phase 3B
recorder.

Plain-data tables describing each syscall's parameters (widths,
pointer/buffer kinds), its return type, and which arguments are
pointers the recorder must read out via `process_vm_readv`. The
forthcoming `syscall! { … }` DSL macro parses source-level entries
into this shape; recorder + replayer crates iterate the resulting
`&[SyscallSpec]` to dispatch.

## Status

Phase 5 sub-phase 3B foundation. **Step 1: spec types + curated
table** for `read`, `write`, `open`, `close`, `mmap`. Subsequent
steps:

| Step                                            | Lands in              |
| ----------------------------------------------- | --------------------- |
| Spec types + curated 5 syscalls                 | this crate (shipped)  |
| `syscall! { … }` DSL proc-macro                 | `bs-syscall-macro`    |
| Curated top-30 (futex, clone, epoll_*, etc.)    | this crate            |
| Machine-extracted long tail from kernel headers | `build.rs` codegen    |
| Generic catch-all for unspecified syscalls      | recorder              |

See [`doc/plans/phase-5-time-travel.md`](../../doc/plans/phase-5-time-travel.md) §
"3B. Single-threaded syscall record" for the full strategy.

## Quick reference

```rust
use bs_syscall_spec::{KNOWN_X86_64, ParamKind, SyscallSpec};

for s in KNOWN_X86_64 {
    for p in s.params {
        match p.kind {
            ParamKind::OutBuf { len_param } => {
                println!("{}: param {} writes bytes (length from `{}`)",
                         s.name, p.name, len_param);
            }
            _ => {}
        }
    }
}
```

`len_param == "ret"` is a sentinel meaning "the syscall's return
value is the actual byte count" — common for `read` / `recv`.
Other names refer to a sibling parameter on the same syscall.

## Tests

`cargo nextest run -p bs-syscall-spec`. Seven internal-consistency
checks: no duplicate syscall numbers, no duplicate names, buffer
`len_param` references either `"ret"` or a real sibling
parameter, ≤ 6 params per syscall (kernel ABI), and spot-checks
that `read`/`write`'s buffer lengths reference the right places
(`ret` vs `count` is a real bug-prone distinction).

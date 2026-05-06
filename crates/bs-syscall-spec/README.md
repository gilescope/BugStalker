<!-- markdownlint-disable MD041 -->
# bs-syscall-spec

Linux syscall specifications for BugStalker's Phase 5 recorder.

Plain-data tables describing each syscall's parameters (widths,
pointer/buffer kinds), its return type, and which arguments are
pointers the recorder must read out via `process_vm_readv`. The
[`bs-syscall-macro`](../bs-syscall-macro) DSL parses source-level
entries into this shape; recorder + replayer crates iterate the
resulting `&[SyscallSpec]` to dispatch.

## Status

Sub-phase 3B foundation. **Two architectures**, three coverage tiers.

| Tier      | Surface                                  | Source                |
| --------- | ---------------------------------------- | --------------------- |
| Curated   | `KNOWN_X86_64` (~31 syscalls, full spec) | `syscall! { … }` DSL  |
| Long tail | `LONG_TAIL_X86_64` (~260, name + nr)     | `data/syscall_64.tbl` + `build.rs` |
| Long tail | `LONG_TAIL_AARCH64` (~260, name + nr)    | `data/syscall_aarch64.tbl` + `build.rs` |
| Catch-all | (in `bs-replay-engine::record::syscall_capture`) | recorder primitive |

## Quick reference

```rust
use bs_syscall_spec::{
    Arch, KNOWN_X86_64, ParamKind, SyscallSpec,
    lookup_x86_64, lookup_long_tail_aarch64, name_for_x86_64,
};

// Curated: full spec for hot syscalls
for s in KNOWN_X86_64 {
    if matches!(s.params.iter().find(|p| matches!(p.kind, ParamKind::OutBuf { .. })), Some(_)) {
        println!("{} writes via an OutBuf", s.name);
    }
}

// Curated → long tail → catch-all dispatch
let name = name_for_x86_64(157).unwrap_or("syscall_157");

// Cross-arch
assert_eq!(lookup_long_tail_aarch64(63).unwrap().name, "read");
assert_eq!(Arch::host(), Some(Arch::X86_64).or(Some(Arch::Aarch64)));
```

`len_param == "ret"` is a sentinel meaning "the syscall's return
value is the actual byte count" — common for `read`/`recv`. Other
names refer to a sibling parameter on the same syscall.

## Updating the long tail

Append `<nr> <name>` rows to `data/syscall_64.tbl` (or the
aarch64 file). `build.rs` validates strict-increasing order,
unique names, and identifier characters; failures print as
`file:line:reason` so the editor jumps to the offender.

## Tests

`cargo nextest run -p bs-syscall-spec`. 19 internal-consistency
checks: no duplicate syscall numbers / names, buffer `len_param`
references resolve, ≤ 6 params per syscall (kernel ABI),
spot-checks of `read` / `write` length semantics, long-tail
sort + lookup round-trip, x86-64 ↔ aarch64 disagree-on-numbers
tripwire, curated/long-tail consistency.

# macOS 26.4.1 — kernel panic when running bs

**Status:** active. Reported to Apple via Feedback Assistant on 2026-05-15.
**Affects:** every darwin build of bs, both the `bs` CLI and
`cargo test --test debugger`, on `xnu-12377.101.15` / macOS 26.4.1
(`25E253`) running on Apple Silicon (`Mac15,9`, socRevision 12).
**Symptom:** machine force-reboots.

Three reproductions captured locally so far. Treat any darwin session
that uses bs as a reboot risk until Apple ships a kernel fix or we
land a userspace workaround in bs.

## Fingerprint (identical across all three captures)

```text
ESR_EL1       = 0x96000007         data abort, level-3 translation fault, load
PC offset     = +0x66970           relative to kernel_text_exec_base
Caller offset = +0x956338          relative to kernel_text_exec_base
FAR           = inside Zone Metadata range (kernel zone-allocator bookkeeping)
Panicking task threads = 19         identical in all three captures
```

The kernel is performing an EL1 load from a virtual address inside the
running zone allocator's `Metadata` region that has no level-3 page
table entry. Classic shape of a zone-metadata UAF or torn-down-while-
walked race in xnu, surfaced by sustained concurrent `mach_vm_*` /
`task_for_pid` traffic from a debugger.

## Capture log paths (on the reporter's machine)

```text
/Library/Logs/DiagnosticReports/panic-full-2026-05-14-200917.0002.panic   cargo test
/Library/Logs/DiagnosticReports/panic-full-2026-05-14-202052.0002.panic   cargo test
/Library/Logs/DiagnosticReports/panic-full-2026-05-15-080531.0002.panic   live bs
```

The 19-threads-each-time coincidence strongly suggests a thread-count
or contention precondition rather than a specific syscall sequence.

## Mitigations in tree

| where | what it does | undo path |
| ----- | ------------ | --------- |
| `tests/debugger/variables.rs::test_dyn_trait_detection` | `#[cfg(not(target_os = "macos"))]` so the proven culprit doesn't run on darwin during `cargo test` | grep the file for `26.4.1` |
| `src/main.rs` startup banner | one-line stderr warning when bs runs on macOS, pointing here | guard removed when kernel build moves past `xnu-12377.101.15` |

Other darwin tests in `tests/debugger/` are *not* gated yet — they go
through the same harness and could panic too. Widen the gate at the
top of `tests/debugger/main.rs` if a fresh capture shows another
victim test.

## Workarounds for users

1. **Use Linux** for debugger work until Apple patches the kernel.
   The x86 box, a Linux VM, or any other Linux host gets you working
   today.
2. If you must work on darwin: avoid `cargo test --test debugger`
   entirely and accept that the live `bs` CLI may reboot the machine
   at any time.

## When to remove this doc

When Apple ships a kernel build past `xnu-12377.101.15 / 25E253` AND
the reporter (or anyone else) can no longer reproduce the panic over
a long-running `cargo test --test debugger` loop *and* a live `bs`
session that walks at least one full debug + step trace of
`examples/showcase`, this doc and the gates it documents can come
out. Until then, keep both.

A bs-side serialising mutex around every `mach_vm_*` / `task_for_pid`
call may also remove the trigger (the kernel race needs concurrent
syscalls); not yet implemented, would land as `src/debugger/
darwin_mach.rs` change behind a single `Mutex<()>` covering every
call site. Tracked as an open follow-up.

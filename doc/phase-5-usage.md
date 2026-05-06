# Phase 5 — time-travel usage walkthrough

This is the user-facing story for BugStalker's Phase 5
record-and-replay. For the architecture and status grid see
[`doc/phase-5-overview.md`](phase-5-overview.md); for the
canonical plan see [`doc/plans/phase-5-time-travel.md`](plans/phase-5-time-travel.md).

## What you can do today

Three command-line tools ship under
`crates/bs-replay-driver/src/bin/`:

| Tool             | Purpose                                            |
| ---------------- | -------------------------------------------------- |
| `replay-record`  | Record a program's syscalls into a trace dir      |
| `replay-load`    | Replay a recorded program from its trace          |
| `replay-doctor`  | Validate / summarise an existing trace            |

All three are Linux-only at the functional level (the recorder
needs ptrace + seccomp NOTIF). On Darwin they build with stub
mains that print a clear platform message; `--help` works
everywhere.

## Prerequisites

- Linux kernel ≥ 5.5 (for `SECCOMP_FILTER_FLAG_NEW_LISTENER`).
- `kernel.yama.ptrace_scope` ≤ 1 (the default on most distros).
  `0` is most permissive; `2` denies non-CAP_SYS_PTRACE
  ptrace_attach.
- A reasonably-deterministic target program. The recorder
  captures syscalls; non-syscall sources of non-determinism
  (RDTSC fast paths, vDSO `gettimeofday`, threading) are not
  yet patched out by default.

## Recording

```bash
$ cargo build --release -p bs-replay-driver
$ ./target/release/replay-record /tmp/trace.bs -- /bin/cat /etc/hostname
my-hostname
replay-record: 47 syscalls / 0 signals / 0 instr-traps / 95 steps
```

What the recorder did:

1. Opened a fresh trace directory at `/tmp/trace.bs`. (Refuses
   to overwrite — corruption guard.)
2. Forked + execve'd `/bin/cat /etc/hostname` under
   `PTRACE_TRACEME` + `PTRACE_O_TRACESYSGOOD`.
3. Walked every syscall via the `step_until_event` state
   machine, capturing pre-syscall args + post-syscall result +
   any out-buffer bytes.
4. Wrote one `Event::Syscall` per syscall to the trace.

The trace directory:

```text
/tmp/trace.bs/
├── manifest.toml             # build-id, kernel_release, env, …
├── event-000001.lz4          # rkyv-archived Segment, lz4-frame'd
└── event-000002.lz4          # (auto-rotated at ~16 MiB)
```

## Inspecting

```bash
$ ./target/release/replay-doctor --load /tmp/trace.bs
47 events / 1 segments / 0 checkpoints
build-id: deadbeef…
recorded-at: 2026-05-06T14:30:00Z
```

Or with full validation:

```bash
$ ./target/release/replay-doctor --check-host \
      --build-id $(b2sum /bin/cat | cut -d' ' -f1) \
      /tmp/trace.bs
trace OK (1 segments, 47 events)
```

Exit codes: `0` replayable / `1` errors / `2` argv parse error.

## Replaying

```bash
$ ./target/release/replay-load /tmp/trace.bs -- /bin/cat /etc/hostname
my-hostname
replay-load: 47 syscalls applied / 0 signals skipped / 0 traps skipped /
             47 steps / 1024 bytes written
```

What the replay path did:

1. Opened the trace.
2. Forked + execve'd `/bin/cat /etc/hostname`, with the child
   installing a `SECCOMP_RET_USER_NOTIF` filter and handing the
   listener fd over to the parent via `SCM_RIGHTS`.
3. For every syscall the tracee tried to make, the supervisor
   matched it to the next `Event::Syscall` in the trace and
   returned the *recorded* result (no syscall actually
   executed).
4. Stopped when the trace ran out (`TraceExhausted`) or the
   tracee exited.

If the replayed program's syscall sequence diverges from the
recorded one, the shim refuses with `ShimRefused(Mismatch)`
naming which arg moved.

## Programmatic API

For embedding into a larger debugger, use
[`bs_replay_driver::record_program`](../crates/bs-replay-driver/src/record.rs)
and
[`bs_replay_driver::replay_program`](../crates/bs-replay-driver/src/replay.rs)
directly:

```rust
use bs_replay_driver::{
    record_program, replay_program, RecordOptions, ReplayOptions,
};
use bs_replay_driver::engine::format::manifest::Manifest;
use std::ffi::CString;

let argv = vec![CString::new("/bin/cat")?, CString::new("/etc/hostname")?];
let envp = vec![/* env */];

// record
let record_report = record_program(
    "/tmp/trace.bs", &manifest,
    argv.clone(), envp.clone(),
    RecordOptions::default(),
)?;

// replay
let replay_report = replay_program(
    "/tmp/trace.bs",
    argv, envp,
    ReplayOptions::default(),
)?;

assert_eq!(replay_report.syscalls_applied, record_report.syscall_events);
```

## What doesn't work yet

| Limitation                                        | Workaround / status                                  |
| ------------------------------------------------- | ---------------------------------------------------- |
| `gettimeofday` / `clock_gettime` via vDSO         | call `vdso_patch::apply_vdso_trampolines` manually   |
| Signals replayed at exact PC                      | `kill(2)`-based delivery is best-effort (shipped);   |
|                                                   | PC-precise variant via PTRACE_SETSIGINFO is queued   |
| `RDTSC` / `RDRAND` / `CPUID` re-injection         | recorded but not yet rewritten into RAX on replay    |
| Multi-threaded determinism                        | single-CPU pin available; PMU instr counts TODO      |
| aarch64 record_session                            | syscall table only — full port pending               |
| Cross-host replay                                 | manifest CPU-feature check refuses incompatible      |
| Long-tail syscalls with weird out-pointer shapes  | catch-all logs a 256-byte window; bounded loud fail  |

For each of the above, the recorder's wire format already
carries the necessary fields (`Event::Signal`,
`Event::InstructionTrap`); the gap is in the replay-side
plumbing.

## Troubleshooting

| Symptom                                             | Cause                                       |
| --------------------------------------------------- | ------------------------------------------- |
| `replay-record: skipping — kernel/perms`            | Kernel < 5.5 or `ptrace_scope = 2`          |
| `ChildSetupFailed { wstatus: 0x4000 }` (exit 64-66) | seccomp NEW_LISTENER not supported          |
| `recv_notif` failed during replay                   | Tracee exited before trace was exhausted    |
| `ShimRefused(Mismatch)`                             | Replayed program took a different code path |
| `ShimRefused(ResultNotCaptured)`                    | Trace was made by step-7b (entry-args-only) |
| `TraceExhausted { applied: N }`                     | Tracee ran longer than the recording — fine |

Report `replay-doctor`'s output when filing a bug.

## See also

- [`doc/phase-5-overview.md`](phase-5-overview.md) — architecture +
  status grid for every plan sub-phase.
- [`doc/plans/phase-5-time-travel.md`](plans/phase-5-time-travel.md) —
  the canonical plan.
- Per-crate READMEs: [`bs-syscall-spec`](../crates/bs-syscall-spec/README.md),
  [`bs-syscall-macro`](../crates/bs-syscall-macro/README.md),
  [`bs-replay-engine`](../crates/bs-replay-engine/README.md),
  [`bs-replay`](../crates/bs-replay/README.md),
  [`bs-replay-driver`](../crates/bs-replay-driver/README.md).

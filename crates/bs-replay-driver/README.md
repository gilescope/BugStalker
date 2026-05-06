<!-- markdownlint-disable MD041 -->
# bs-replay-driver

Sub-phase 3I integration seam between `bs-replay-engine` and
BugStalker's debugger surface, plus three user-facing CLI
binaries.

## Status

Phase 5's recorder + replay pipeline lands here as
one-call helpers and CLI binaries. The DAP shapes ship; the
"BugStalker doesn't know whether it's attached to a live
process or a replay" fake-tracee adapter (~3 weeks against
`tracee.rs`) is a follow-up.

| Surface                                  | Role                                            |
| ---------------------------------------- | ----------------------------------------------- |
| `record_program(...)`                    | One-call recorder against a user program (Linux) |
| `replay_program(...)`                    | One-call replay against a recorded trace (Linux) |
| `TraceReplayer`                          | Open trace, walk events, seek by index          |
| `TraceReplayer::check_replayability`     | build-id + CPU-superset host check              |
| `ReverseDebugger`                        | Tier 1 navigation: step / rstep / rcontinue     |
| `host::host_features()`                  | Linux `/proc/cpuinfo` enumerator                |
| `capture::capture_host_manifest`         | Writer-side stamp of host environment           |
| `capture::capture_one_shot`              | Tier 2 single-checkpoint helper                 |
| `dap::*Request / *Response`              | `bs/replay*` DAP shapes + handlers              |
| `record_primitives::*` / `replay_primitives::*` | Re-exports of every engine-level primitive |
| `replay-doctor` binary                   | Validate + summarise a trace dir                |
| `replay-record` binary                   | Drive `record_program` from the command line    |
| `replay-load` binary                     | Drive `replay_program` from the command line    |

## Quick start

Record a program (Linux):

```bash
$ replay-record /tmp/trace.bs -- /bin/cat /etc/hostname
replay-record: 47 syscalls / 0 signals / 0 instr-traps / 95 steps
```

Replay it against a fresh tracee:

```bash
$ replay-load /tmp/trace.bs -- /bin/cat /etc/hostname
replay-load: 47 syscalls applied / 0 signals skipped / 0 traps skipped /
             47 steps / 1024 bytes written
```

Validate a trace from the host that wrote it:

```bash
$ replay-doctor --check-host --build-id $(b2sum target/debug/bin) /tmp/trace.bs
trace OK (3 segments, 47 events)
```

Programmatic recording:

```rust
use bs_replay_driver::{record_program, RecordOptions};
use bs_replay_driver::engine::format::manifest::Manifest;
use std::ffi::CString;

let report = record_program(
    "/tmp/trace.bs",
    &manifest,
    vec![CString::new("/bin/cat")?, CString::new("/etc/hostname")?],
    vec![/* envp */],
    RecordOptions::default(),
)?;
println!("recorded {} syscalls", report.syscall_events);
```

Programmatic replay:

```rust
use bs_replay_driver::{replay_program, ReplayOptions};

let report = replay_program(
    "/tmp/trace.bs",
    vec![CString::new("/bin/cat")?, CString::new("/etc/hostname")?],
    vec![/* envp */],
    ReplayOptions::default(),
)?;
println!("applied {} syscalls", report.syscalls_applied);
```

DAP-side:

```rust
use bs_replay_driver::dap::*;
let resp = replayer.dap_jump(&ReplayJumpRequest {
    target: JumpTarget::EventIndex { event_index: 100 },
})?;
//   resp.event_index            == 100
//   resp.restore_from_checkpoint == Some(N) | None
```

## Tests

`cargo nextest run -p bs-replay-driver`. 62+ tests covering the
replayer, reverse-debugger, host enumerator, capture, DAP
handlers, and three CLI binaries' parse + skip paths. On Linux,
the round-trip and bidirectional integration tests exercise the
full record→replay pipeline against `/bin/true`.

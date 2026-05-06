<!-- markdownlint-disable MD041 -->
# bs-replay-driver

Sub-phase 3I integration seam between `bs-replay-engine` and
BugStalker's debugger surface, plus the `replay-doctor` support
binary.

The plan's promise: "BugStalker doesn't know whether it's attached
to a live process or a replay; same breakpoints, same watchpoints,
same step semantics." This crate is where that uniformity lives.
Today it ships the *driver* surface (open / walk / seek / verify
host) and the Tier 1 reverse-step navigation; the real fake-tracee
adapter (~3 weeks of work against `tracee.rs`) lands when there's
a Linux test path.

## Status

Phase 5 sub-phase 3I — **scaffold shipped**. See
[`doc/phase-5-overview.md`](../../doc/phase-5-overview.md).

| Surface                              | Role                                        |
| ------------------------------------ | ------------------------------------------- |
| `TraceReplayer`                      | Open trace, walk events, seek by index      |
| `TraceReplayer::check_replayability` | build-id + CPU-superset host check          |
| `ReverseDebugger`                    | Tier 1 navigation: step / rstep / rcontinue |
| `host::host_features()`              | Linux `/proc/cpuinfo` enumerator            |
| `capture::capture_host_manifest`     | Writer-side stamp of host environment       |
| `dap::*Request / *Response`          | `bs/replay*` DAP shapes + handlers          |
| `replay-doctor` binary               | CLI wrapping `validate_with`                |

## Quick start

Drive replay through the engine:

```rust
use bs_replay_driver::{TraceReplayer, ReverseDebugger};
use bs_replay_driver::host::host_features;

let mut r = TraceReplayer::open("./trace.bs")?;

// Verify the host can replay this trace.
r.check_replayability(
    &host_features().unwrap_or_default(),
    Some(&host_build_id),
)?;

// Walk events from the start.
while let Some(ev) = r.next_event()? { /* … */ }

// Or hand off to the reverse-step UX:
let mut rdb = ReverseDebugger::new(r);
rdb.add_breakpoint(42);
rdb.rcontinue()?; // walk backward to nearest breakpoint
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

## `replay-doctor`

```text
$ replay-doctor --check-host --build-id $(b2sum target/debug/bin) ./trace.bs
trace OK (3 segments, 1024 events)
```

Exit codes: 0 replayable / 1 errors / 2 argv parse.

## Tests

`cargo nextest run -p bs-replay-driver`. 45 tests covering the
replayer, reverse-debugger, host enumerator, capture, DAP
handlers, and the `replay-doctor` CLI smoke tests.

<!-- markdownlint-disable MD041 -->
# bs-replay-engine

Trace format + recorder + replay-shim engine for BugStalker's
Phase 5 time-travel debugging.

A trace is a directory of lz4-frame-compressed rkyv-archived
[`Segment`] files, opaque-payload [`Checkpoint`] snapshot files,
and a hand-rolled key/value text manifest. The engine reads,
writes, and validates that shape; the platform-specific
recorder + replay primitives live in `record::linux::*` and
`replay::linux::*`.

## Status

Sub-phases 3A–3F shipped. See
[`doc/phase-5-overview.md`](../../doc/phase-5-overview.md) for
the synthesis grid.

| Capability                              | Surface                                              |
| --------------------------------------- | ---------------------------------------------------- |
| Append events, auto-rotate at 16 MB     | `format::TraceWriter`                                |
| Read manifest + segments + checkpoints  | `format::TraceReader`                                |
| Walk events sequentially                | `format::EventCursor`                                |
| Binary-search seek by event index       | `TraceReader::segment_for_event`                     |
| Replay-anchor lookup                    | `TraceReader::find_checkpoint_at_or_before`          |
| Top-to-bottom doctor                    | `format::validate / validate_with`                   |
| Three-tier syscall capture (curated/long-tail/catch-all) | `record::syscall_capture`                |
| `seccomp-bpf` user-notify install       | `record::linux::seccomp::install_trap_all_listener`  |
| Listener-fd ioctl wrappers              | `record::linux::ptrace_driver`                       |
| PTRACE-only recorder + signal dispatch  | `record::linux::record_session::step_until_event`    |
| Fork+exec lifecycle (record / replay)   | `record::linux::record_child::spawn` + `replay::linux::replay_child::spawn_replay_child` |
| Syscall-exit-stop result capture        | `record::linux::exit_stop`                           |
| Non-deterministic instruction trapping  | `record::linux::instrs` (RDTSC/RDTSCP/RDRAND/RDSEED/CPUID) |
| Signal capture + replay primitives      | `record::linux::signals`                             |
| Single-CPU pin (`sched_setaffinity`)    | `record::linux::thread_sched`                        |
| vDSO entry-point detector + patcher     | `record::linux::vdso_patch`                          |
| Replay shim — apply recorded events     | `replay::linux::shim::apply_recorded_event`          |

## Encoder choice

- **Records** — `rkyv` 0.8: zero-copy archive reads at replay time
  dominate the trace's lifecycle (write once, replay many).
- **Compression** — `lz4_flex` frame format: pure-Rust encode +
  decode, no `unsafe` by default, no C linkage. ~30 % looser
  than zstd-1 but ~2-3× faster encode — the right trade for a
  hot recorder path.
- **Manifest** — hand-rolled key/value text. Ten fields written
  once; a serialisation framework would be a tax.
- **x86 disassembly** — `iced-x86` (Linux-only, recorder side).
- **ELF parsing** — `object` 0.32 for vDSO symbol scanning.
- **Zero `*-sys` crates** in the dep tree.

## Quick start

Format-only usage (cross-platform):

```rust
use bs_replay_engine::format::{
    Event, Manifest, TraceReader, TraceWriter, version::FormatVersion,
};

let manifest = Manifest {
    format_version: FormatVersion::V1,
    build_id: build_id_hex,
    kernel_release: "6.6.42".into(),
    cpu_features: vec!["sse2".into()],
    engine_version: bs_replay_engine::VERSION.into(),
    initial_env: vec![],
    initial_cwd: "/tmp".into(),
    initial_args: vec![],
    recorded_at: None,
};

let mut w = TraceWriter::create(&dir, &manifest)?;
w.write_event(Event::Marker { tag: 0, data: 1 })?;
w.take_checkpoint(b"snapshot-bytes".to_vec())?;
w.finish()?;
```

For the full record + replay surface, prefer the high-level
helpers in [`bs-replay-driver`](../bs-replay-driver):
`record_program(...)` and `replay_program(...)`.

## Tests

`cargo nextest run -p bs-replay-engine`. ~80+ tests on Darwin
(format roundtrip × event variants, validator, checkpoints,
replay-seek, event-cursor, properties × 5, syscall_capture × 13).
Linux adds the seccomp / ptrace_driver / instrs / signals /
thread_sched / vdso_patch / record_session / replay_child suites
and the `recorder_smoke` integration test.

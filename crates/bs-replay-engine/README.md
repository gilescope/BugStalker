<!-- markdownlint-disable MD041 -->
# bs-replay-engine

Trace format + read/write/validate engine for BugStalker's Phase 5
time-travel debugging.

A trace is a directory of lz4-frame-compressed rkyv-archived
[`Segment`] files plus opaque-payload [`Checkpoint`] snapshot
files plus a hand-rolled key/value text manifest. The engine
reads, writes, and validates that shape; the *recorder* (sub-phase
3B — seccomp-bpf user-notify on Linux) is downstream work.

## Status

Phase 5 sub-phase 3A — **trace format and storage**. Shipped in
full. See [`doc/phase-5-overview.md`](../../doc/phase-5-overview.md)
for the synthesis and [`doc/plans/phase-5-time-travel.md`](../../doc/plans/phase-5-time-travel.md)
for the original plan.

| Capability                             | Surface                                      |
| -------------------------------------- | -------------------------------------------- |
| Append events, auto-rotate at 16 MB    | `TraceWriter::write_event / take_checkpoint` |
| Read manifest + segments + checkpoints | `TraceReader::open`                          |
| Walk events sequentially               | `TraceReader::cursor / cursor_at`            |
| Binary-search seek by event index      | `TraceReader::segment_for_event`             |
| Replay-anchor lookup                   | `TraceReader::find_checkpoint_at_or_before`  |
| Top-to-bottom doctor                   | `format::validate / validate_with`           |
| 6 proptest properties                  | round-trip, rotation, seek, manifest         |

## Encoder choice

- **Records** — `rkyv` 0.8: zero-copy archive reads at replay time
  dominate the trace's lifecycle (write once, replay many).
- **Compression** — `lz4_flex` frame format: ruzstd 0.8 on
  crates.io is decoder-only; lz4_flex ships encode + decode,
  no-unsafe-by-default, no C linkage. ~30 % looser than zstd-1
  but ~2-3× faster encode, which is the right trade for a
  hot recorder path.
- **Manifest** — hand-rolled key/value text. Ten fields written
  once; a serialisation framework would be a tax.
- **Zero `*-sys` crates** in the dep tree.

## Quick start

```rust
use bs_replay_engine::format::{
    Manifest, TraceWriter, TraceReader, Event,
    version::FormatVersion,
};

let m = Manifest {
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

let mut w = TraceWriter::create(&dir, &m)?;
w.write_event(Event::Marker { tag: 0, data: 1 })?;
w.take_checkpoint(b"snapshot-bytes".to_vec())?;
w.finish()?;

let r = TraceReader::open(&dir)?;
for ev in std::iter::from_fn(|| r.cursor().next().transpose()) {
    println!("{:?}", ev?);
}
```

## Tests

`cargo nextest run -p bs-replay-engine`. Unit + integration +
proptest, ~80 cases including the determinism property
(`record_replay_event_sequence_is_identical`) and the seek-equivalence
property (`seek_to_equals_cursor_at`).

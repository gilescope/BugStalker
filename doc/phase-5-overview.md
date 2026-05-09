# Phase 5 — Time-travel debugging: implementation overview

The canonical plan is `doc/plans/phase-5-time-travel.md`. This
document reflects what's actually *shipped* in-tree against that
plan, the architecture the work landed in, and where the next
contributor should pick up.

## Session status

Phase 5 sub-phase 3A — trace format and storage — is complete.
Sub-phases 3B (recorder) and Tier 2 (real fork-checkpoint
mechanism) ship as wire-format-and-orchestration only; the
kernel-touching parts await their respective Linux test paths.
The next contributor can pick up at any of the
matched-to-environment options below.

### Cross-platform verification

Build + tests green on both supported development hosts:

| Host                   | Command                                    | Result      |
| ---------------------- | ------------------------------------------ | ----------- |
| macOS aarch64 (Apple)  | `cargo nextest run -p bs-replay…`          | 144 / 144   |
| Linux x86_64 (NixOS)   | `cargo test -p bs-replay…`                 | 144 / 144   |

The Linux run also exercises the real `/proc/cpuinfo` enumerator
(`bs_replay_driver::host_features()`); on the verification host
it returned 140 unique CPU feature flags. macOS exercises the
`sysctlbyname` path against `hw.optional.*`. Both ends emit the
same Linux-canonical feature names so traces recorded on one
host can have their CPU-feature manifest checked against the
other without a translation table.

Phase 5 trades multi-month sub-phases against what was tractable
to implement and test from a macOS-arm64 development host. The
result is a load-bearing trace engine + driver layer that's
independent of the kernel-touching pieces, ready for them to plug
in when a Linux test path is available.

## Phase 6 dependency status

The Phase 5 plan deliberately makes Tier 1 reverse-step display and
sub-phase 3H depend on Phase 6's processor-trace substrate. That
dependency is now active, not theoretical:

- Phase 6 step 114 shipped the `bs-perf` crate scaffold and Linux
  cycles+IP `perf_event_open` event opener.
- Phase 6 step 115 shipped the mmap data-ring drain and parser for
  cycles samples plus loss accounting.
- Phase 6 step 116 shipped the `.debug_line` PC-to-source resolver
  boundary (`bs_perf::decoder::SourceResolver`) with load-bias-aware
  PC lookup and a per-PC cache.
- Phase 6 step 117 shipped the pure in-memory aggregation model
  (`bs_perf::aggregator::PerfData`) for cumulative counts, last-run
  counts, unresolved samples, and bounded per-stop hot-line history.
- Phase 6 step 118 shipped the UI-facing overlay view layer
  (`bs_perf::overlay`) so console, TUI, and DAP surfaces share the
  same source-row projection, hot-line selection, sample-share math,
  unresolved-sample visibility, and bounded history numbering.
- Phase 6 step 119 shipped the serde-free Rust DAP shape/handler
  layer (`bs_perf::dap`) for `bs/perfOverlay`,
  `bs/perfOverlayEnable`, `bs/perfOverlayDisable`, and
  `StoppedEvent.body.bs_perf`. JSON wiring into the active DAP
  server remains a boundary concern, matching the Phase 5 replay
  DAP pattern.
- Phase 6 step 120 wired the active DAP server boundary behind the
  root `perf` feature. Default builds now acknowledge perf overlay
  custom requests as unavailable; `--features perf` routes them
  through `bs_perf::dap` over the session aggregate state. The live
  collector still has to populate that state.
- Phase 6 step 121 added the first live cycles collector path:
  `--features perf` on Linux opens cycles+IP sampling for the
  focused task on resume, drains the ring at the next stop, resolves
  sampled PCs through the main executable's line table with PIE load
  bias from `/proc/<pid>/maps`, and records unresolved/lost samples
  explicitly.
- Phase 6 step 122 expanded that live collector from the focused
  task to the debugger's current thread snapshot. It opens one
  cycles+IP monitor/ring per known TID on resume, tolerates
  per-thread open failures, drains every active ring at stop, and
  aggregates all samples into the same `PerfData` stop summary.
  New-thread live onboarding during the run remains pending.
- Phase 6 step 123 added thread coverage accounting at stop. The DAP
  session compares sampled TIDs with the debugger's current attached
  thread snapshot, reports visible unsampled threads through
  `unavailable`, and exposes `unsampledThreadCount` in both
  `bs/perfOverlay` and `StoppedEvent.body.bs_perf`.
- Phase 6 step 124 added the pure-Rust Intel PT host capability
  probe (`bs_perf::linux::intel_pt`). It detects the `intel_pt` PMU
  type in sysfs, reads `perf_event_paranoid`, and returns structured
  availability/permission/missing-PMU diagnostics. PT packet capture
  and decode are separate boundaries below.
- Phase 6 step 125 projects that PT probe into the DAP perf overlay
  boundary. `bs/perfOverlay` and `bs/perfOverlayEnable` now include
  `intelPt` diagnostics (`status`, `pmuType`, `perfEventParanoid`,
  and structured unavailable `reason`) when the root `perf` feature
  is enabled.
- Phase 6 step 126 added the pure Intel PT `perf_event_attr`
  builder. `build_intel_pt_attr(pmu_type)` defines the future PT
  event-open contract and records the planned 64 MiB AUX trace buffer
  plus 4 MiB data-ring sizing, without opening events or decoding
  packets yet.
- Phase 6 step 127 added the Intel PT event opener boundary.
  `IntelPtMonitor` owns the disabled PT perf fd and supports
  reset/enable/disable.
- Phase 6 step 128 added the Intel PT data/AUX mmap boundary.
  `IntelPtMonitor` can now map the PT perf data ring and AUX trace
  buffer, drain visible AUX bytes into owned contiguous buffers after
  capture stops, and surface AUX overruns before decode.
- Phase 6 step 129 added the raw Intel PT capture owner.
  `IntelPtCapture` groups the PT monitor, data ring, and AUX buffer,
  starts/stops capture windows, and returns data-ring metadata plus raw
  AUX packet bytes for the decode boundary.
- Phase 6 step 130 added the first Intel PT instruction decode
  boundary behind `--features intel-pt`. `bs_perf::pt_decode` accepts
  raw AUX bytes plus file-backed executable mappings, optionally takes
  CPU family/model/stepping for libipt errata, and returns bounded
  decoded instruction IPs. Default builds keep the same typed boundary
  but return `Unsupported` without pulling libipt.
- Phase 6 step 131 added the decoded-PT source attribution boundary.
  `SourceResolver::resolve_decoded_pt_trace` maps decoded instruction
  IPs to source frames and preserves decode sync/error/truncation stats;
  `PerfData::record_resolved_pt_trace` ingests those resolved
  instructions into the same line counters and unresolved-sample path as
  cycles sampling.
- Phase 6 step 132 added the DAP-session decode-image snapshot. When
  Intel PT is available, the Linux perf session snapshots executable
  file-backed `/proc/<pid>/maps` entries into `IntelPtDecodeConfig`
  sections and reports `intelPt.decodeImageSectionCount` plus
  `intelPt.decodeImageUnavailable` in the DAP diagnostic body.
- Phase 6 step 133 added the opt-in Intel PT live collector path.
  `bs/perfOverlayEnable` accepts `intelPt: true` / `precise: true`;
  the Linux DAP perf session opens one `IntelPtCapture` per sampled TID,
  drains AUX bytes at stop, and attempts
  `pt_decode -> SourceResolver -> PerfData` attribution when the build
  supports decode. Cycles sampling remains the default and survives PT
  capture/decode failures.
- Phase 6 Linux x86 verification on an AMD host hardened the cycles
  opener to retry with `precise_ip = 0` when PEBS-style
  `precise_ip = 2` is rejected by the PMU. `bs-perf` tests, the root
  `perf` feature, and the root `intel-pt` feature now typecheck on
  Linux x86 with the host's `LIBCLANG_PATH` set for bindgen.
- Phase 6 still needs PT-capable Linux runtime verification on actual
  Intel PT hardware and any follow-up fixes from that run. That means
  Phase 5 can record/replay and expose reverse navigation over trace
  events, but source-level Tier 1 reverse display and 3H PT-assisted
  recording remain blocked on validating and hardening the Phase 6 PT
  tier on real hardware, then feeding precise PCs into replay events.
- Phase 5 now emits coarse `Event::PcMarker` records at syscall
  boundaries. That gives replay/reverse consumers a source-resolvable
  PC without changing the archived `Event::Syscall` layout. It is not
  the final Tier 1 experience: PT decode still needs to supply the
  fine-grained instruction PCs between syscalls.

## Architecture

Three workspace crates under `crates/`:

```text
bs-replay-engine/                ← format layer
    src/format/
        manifest.rs              ← key/value text manifest
        version.rs               ← magic + FormatVersion
        event.rs                 ← Event enum (Marker | Syscall | Signal)
        segment.rs               ← Segment{header, events} archive root
        checkpoint.rs            ← trace-internal Checkpoint files
        trace_writer.rs          ← TraceWriter (auto-rotate, take_checkpoint)
        trace_reader.rs          ← TraceReader (lazy-cached seek tables)
        event_cursor.rs          ← EventCursor (sequential walk)
        validator.rs             ← validate / validate_with doctor
    tests/                       ← format_roundtrip, validator,
                                   checkpoints, replay_seek,
                                   event_cursor, property_roundtrip

bs-replay/                       ← Tier 2 ring orchestrator
    src/ring.rs                  ← CheckpointMechanism trait,
                                   CheckpointRing<M>,
                                   MockCheckpointMechanism
    src/linux/fork_checkpoint.rs ← stub for real fork(2) work
    src/darwin/checkpoint.rs     ← stub for mach_vm_remap work

bs-replay-driver/                ← integration seam (sub-phase 3I)
    src/replayer.rs              ← TraceReplayer (open/walk/seek)
    src/reverse.rs               ← ReverseDebugger (Tier 1 navigation)
    src/host.rs                  ← /proc/cpuinfo enumerator
    src/capture.rs               ← capture_host_manifest writer-side
    src/dap.rs                   ← bs/replay* request shapes + handlers
    src/bin/replay-doctor.rs     ← support CLI
```

Dependency direction (one-way, no cycles):

```text
bs-replay-engine                 ← no Phase-5-internal deps
bs-replay                        ← no Phase-5-internal deps
bs-replay-driver  ── depends on bs-replay-engine
                  (could depend on bs-replay later for Tier 2 wiring)
```

## User story

```text
$ # Record a trace via the engine API. (Recorder itself is sub-phase
$ # 3B work — wire format is in place, seccomp wiring lives there.)
$ # … program runs; events hit disk under ./trace.bs/

$ # Validate the trace from the host that wrote it.
$ replay-doctor --check-host --build-id $(b2sum target/debug/bin) ./trace.bs
trace OK (3 segments, 1024 events)
$ echo $?
0

$ # On a different host, the same command surfaces what's wrong:
$ replay-doctor --check-host --build-id 00000000 ./trace.bs
error: [build-id-mismatch] manifest build_id ab… disagrees with expected 00000000
error: [host-feature-missing] host lacks CPU feature `avx2` used by recording
$ echo $?
1
```

Programmatic flow inside a debugger:

```rust
use bs_replay_driver::{TraceReplayer, ReverseDebugger};

// Open + verify the trace can be replayed on this host.
let mut r = TraceReplayer::open("./trace.bs")?;
r.check_replayability(
    &bs_replay_driver::host_features().unwrap_or_default(),
    Some(&host_build_id),
)?;

// Seek to the latest checkpoint at-or-before our target event.
let cp = r.find_checkpoint_at_or_before(target_event_idx)?;
//   …restore process state from cp.payload…
r.seek_to(cp.map(|h| h.event_index).unwrap_or(0));

// Drive forward through events (each is owned-deserialized).
while let Some(ev) = r.next_event()? {
    // …apply syscall result, deliver signal, etc…
    if r.position() == target_event_idx {
        break;
    }
}

// Or, hand the same replayer to the reverse-step UX layer:
let mut rdb = ReverseDebugger::new(r);
rdb.add_breakpoint(some_event_idx);
rdb.rcontinue()?;   // walk backward to nearest breakpoint
```

DAP-side:

```rust
use bs_replay_driver::dap::*;
let resp = replayer.dap_checkpoint_list(&ReplayCheckpointListRequest::default())?;
//   → resp.checkpoints: Vec<CheckpointSummary>

let resp = replayer.dap_jump(&ReplayJumpRequest {
    target: JumpTarget::EventIndex { event_index: 42 },
})?;
//   → resp.event_index, resp.restore_from_checkpoint

let resp = replayer.dap_timeline(&ReplayTimelineRequest::default())?;
//   → resp.total_events + sparse waypoints (UI scrubber)
```

## Status of each plan sub-phase

| Plan section                                | Status                       | Why / what's pending                                          |
| ------------------------------------------- | ---------------------------- | ------------------------------------------------------------- |
| §"Tier 1 — Intel PT reverse step"           | navigation + syscall-boundary PC markers shipped | Display is coarse until Phase 6 PT capture supplies fine-grained PCs |
| §"Tier 2 — Checkpoint-based replay"         | Linux + Darwin shipped       | Linux fork(2) + Darwin mach_vm_remap-style capture/restore both functional |
| §"Tier 3 — Clean-room record-and-replay"    | full record→replay shipped   | record_program + replay_program + replay-record + replay-load CLIs end-to-end |
| Sub-phase 3A: trace format                  | shipped                      | manifest, segments, checkpoints, validator, properties        |
| Sub-phase 3B: syscall record                | PTRACE-only architecture     | Step 71 corrects the NOTIF+TRACESYSCALL deadlock by switching record to PTRACE-only (NOTIF stays for replay). step_until_event handles syscall + signal + instruction-trap + ptrace-event stops as one event-loop. record_program in driver. |
| Sub-phase 3C: replay                        | full replay path + ptraced loop | shim + replay_child (NOTIF + SCM_RIGHTS handover) + replay_program in driver + replay-load CLI + bidirectional smoke test. Steps 94-95: PTRACE_SEIZE option + multiplexed recv_notif/waitpid loop. |
| Sub-phase 3D: non-deterministic instrs      | record + replay wired, RDRAND/RDSEED + CPUID end-to-end | step_until_event emits Event::InstructionTrap; vDSO patcher + opt-ins; PTRACE_SETREGS replays the recorded result into RAX/EDX:EAX/EBX/ECX (step 97); ReplayOptions::patch_vdso (step 98). Step 101: RDRAND/RDSEED dest register captured + replayed (3-word result vector). Step 110: CPUID-input-aware synthesis — `cpuid_synthesised(eax_in, ecx_in)` runs native CPUID with the trapped tracee's `(rax, rcx)` and masks RDRAND/RDSEED feature bits (leaf 1 ECX bit 30, leaf 7.0 EBX bit 18) so libc/openssl startup probes see real host features minus the non-deterministic ones, falling back to syscall-based randomness instead of native instructions. `--disable-cpuid` is now usable on real programs. |
| Sub-phase 3E: signals                       | record + PC-precise replay   | step_until_event emits Event::Signal via PTRACE_GETSIGINFO; replay path: kill(2) (non-ptraced) / PTRACE_SETSIGINFO (ptraced, content-precise) / single-step rendezvous (step 100, PC-precise within a 64-instruction cap). |
| Sub-phase 3F: multi-thread serialisation    | single-CPU pin shipped       | PMU-based instr-retired counts wait on 3H PT integration      |
| Sub-phase 3G: aarch64 port                  | record + Tier 2 + vDSO trampoline cross-arch | data/syscall_aarch64.tbl (step 66); regs_aarch64 + UserRegsAarch64 + PTRACE_GETREGSET/SETREGSET (step 102); record_session arch dispatch + Linux build hygiene (step 103); proc_regs Tier 2 register-restore on aarch64 + cross-arch hygiene (step 105); aarch64 vDSO trampoline payload (step 109) — `MOVZ X8 + SVC #0 + RET` 12-byte sequence with arch-conditional `syscall_nr_for_vdso` and the merged target-symbol list. All five Phase 5 crates + tests now cross-compile to aarch64-unknown-linux-gnu and the existing `test-arm64` CI job covers them. Aarch64 instruction-trap classifier (CNTVCT_EL0 / MRS / AT) still pending — and largely academic absent a triggering mechanism (aarch64 has no PR_SET_TSC analogue). |
| Sub-phase 3H: PT-assisted recording         | not started                  | Needs PT-capable Linux verification/hardening; Phase 6 currently has cycles event + per-known-thread mmap rings + thread coverage accounting + PC-to-source resolver + aggregation + overlay/DAP view models + Intel PT host/DAP diagnostics + PT event fd opener + PT data/AUX mmap + raw PT capture owner + feature-gated PT instruction decode + decoded-PT source attribution + DAP-session decode-image snapshots + opt-in PT live collector path |
| Sub-phase 3I: BugStalker driver integration | full pipeline + 3 CLIs + DAP record handler | record_program / replay_program + replay-record + replay-load + replay-doctor binaries. `bs/replayRecord` DAP handler landed in step 108 (last "stub" item closed); JSON wiring at the DAP-server boundary is the only remaining concern. |
| §"DAP integration"                          | shapes + handlers (incl. record) | JSON wiring is the DAP server's concern (one `From` per type). `bs/replayRecord` joined `bs/replayCheckpointList`, `bs/replayJump`, `bs/replayTimeline`, `bs/replayCapture`, `bs/replayRestore`, `bs/replayLoad` in step 108 — every shape in the plan's request set is now backed by a handler. |
| §"Pure-Rust policy"                         | upheld                       | rkyv + lz4_flex + iced-x86 + object; **zero C deps in Phase 5** |

## Public API surface — quick reference

### `bs_replay_engine::format`

| Type / fn                                   | Role                                                   |
| ------------------------------------------- | ------------------------------------------------------ |
| `Manifest` (with `recorded_at`)             | Trace-level metadata; serde-free key/value text format |
| `validate(dir)`                             | Doctor that walks a trace and reports findings         |
| `validate_with(dir, opts)`                  | Adds host-feature + build-id checks                    |
| `ValidationOptions`, `Diag`, etc.           | Options + report types; `DiagKind` strings are stable  |
| `Event` (Marker, Syscall, Signal)           | Recorded event vocabulary; additive variant ordering   |
| `Segment`, `SegmentHeader`                  | Archive root for one rotated segment                   |
| `Checkpoint`, `CheckpointHeader`            | Trace-internal snapshot file                           |
| `TraceWriter`                               | Append events; `take_checkpoint`; auto-rotate          |
| `TraceReader`                               | Open trace; segment + checkpoint enumeration           |
| `TraceReader::cursor / cursor_at`           | Walk events (yields owned `Event`s)                    |
| `TraceReader::segment_for_event`            | Binary-search event → (seg_idx, offset)                |
| `TraceReader::find_checkpoint_at_or_before` | Replay-seek anchor                                     |
| `EventCursor`                               | Sequential walk; supports forward step + seek          |
| `FormatVersion`, `TRACE_MAGIC`              | Version newtype + `b"BSREPLAY"` magic                  |

### `bs_replay`

| Type / fn                          | Role                                                    |
| ---------------------------------- | ------------------------------------------------------- |
| `CheckpointMechanism` trait        | Platform-specific take/kill of a checkpoint             |
| `CheckpointRing<M>`                | Capacity-bounded FIFO with drop-oldest-via-kill         |
| `MockCheckpointMechanism`          | Test-time mechanism; no kernel side effects             |
| `MAX_CHECKPOINTS = 32`             | Default ring capacity (plan invariant)                  |

### `bs_replay_driver`

| Type / fn                            | Role                                                        |
| ------------------------------------ | ----------------------------------------------------------- |
| `TraceReplayer`                      | Owned wrapper around `TraceReader` + position counter       |
| `TraceReplayer::next_event`          | Step forward one owned event                                |
| `TraceReplayer::seek_to / position`  | Jump without yielding / report playhead                     |
| `TraceReplayer::check_replayability` | build-id + CPU-superset check before replay                 |
| `ReverseDebugger`                    | Tier 1 navigation: `step / rstep / run_forward / rcontinue` |
| `host_features()`                    | `/proc/cpuinfo` enumerator (Linux); `Unsupported` elsewhere |
| `capture_host_manifest(build_id)`    | Writer-side stamp of host environment                       |
| `dap::*Request / *Response`          | `bs/replay*` custom DAP request shapes                      |
| `replay-doctor` (binary)             | CLI wrapping `validate_with`                                |

## Test coverage

```text
bs-syscall-spec        :  14 tests (macro-generated curated 31, build-time
                                    long-tail 260+, sort/dedupe/contradiction
                                    invariants)
bs-syscall-macro       :   5 tests (DSL grammar coverage, reference shape,
                                    subset membership)
bs-replay              :   9 tests (ring orchestration, mock mechanism)
bs-replay-engine       :  79 + N tests (format roundtrip × event variants,
                                    validator, checkpoints, replay-seek,
                                    event-cursor, properties × 5,
                                    syscall_capture × 13, plus Linux-only
                                    seccomp/ptrace_driver/instrs/signals/
                                    thread_sched suites)
bs-replay-driver       :  45 tests (replayer, reverse, host, capture,
                                    DAP handlers, replay-doctor CLI)
                        ───
                        Darwin run: 165+ green (Linux-only modules
                                    cfg-gated out)
                        Linux run : superset including seccomp install
                                    smoke, ptrace driver layout asserts,
                                    PR_SET_TSC fork test, sched_setaffinity
                                    round-trip
```

Properties (proptest, 64 cases each):

- `record_replay_event_sequence_is_identical`
- `record_replay_survives_arbitrary_rotations`
- `manifest_text_roundtrip`
- `checkpoint_seek_is_at_or_before`
- `rotation_threshold_does_not_change_event_sequence`
- `seek_to_equals_cursor_at`

The properties earned their keep — `manifest_text_roundtrip`
caught two real parser bugs (`=` in env keys; `value.trim()`
eating leading whitespace) in step 10.

## Pure-Rust policy upheld

Plan §"Pure-Rust policy" promised zero C dependencies in Phase 5.
The original ruzstd-at-Fastest plan was swapped for `lz4_flex`
(ruzstd 0.8 on crates.io is decoder-only); `rkyv 0.8` for the
trace events handles zero-copy reads; the manifest is hand-rolled
text. Result:

```text
$ cargo tree -p bs-replay-engine 2>&1 | grep -i 'sys =\|-sys '
(no output — no `*-sys` crates pulled in)
```

## Where the next contributor starts

Pick the sub-phase whose blocker matches your environment:

- **Have a Linux runner with ptrace?** Implement
  `ForkCheckpointMechanism` in `crates/bs-replay/src/linux/fork_checkpoint.rs`
  — `Checkpoint::take` does fork(2) + SIGSTOP + ptrace seize.
  The `CheckpointMechanism` trait is already wired up, so
  `CheckpointRing<ForkCheckpointMechanism>` works the moment
  `take` returns a real PID.

- **Want to land Tier 3 record?** Sub-phase 3B in the plan
  is the sequence: seccomp-bpf user-notify → ptrace driver →
  syscall-args extraction → write `Event::Syscall`. Wire format
  is in place; the writer just needs callers.

- **Want the Tier 1 display layer?** Validate and harden Phase 6's
  Intel PT live collector on PT-capable Linux hardware, then add a
  `pc: Option<u64>` field to the relevant `Event` variants (additive
  — no version bump per the variant-ordering rule) so replay
  navigation can render source-level locations.

- **DAP server integration?** The request/response types in
  `bs_replay_driver::dap` are JSON-ready; one `From` impl per
  type lands them in any DAP framework.

## Cross-references

- Canonical plan: `doc/plans/phase-5-time-travel.md` (which this
  doc reflects against).
- Phase 6 perf-overlay: `doc/plans/phase-6-perf-overlay.md` —
  blocks Tier 1 PT-driven reverse-step display and 3H assisted
  recording.
- Phase 8 testing: `doc/plans/phase-8-testing.md` — the Phase 5
  property in §"Property testing" is implemented; the
  differential rr oracle is deferred.

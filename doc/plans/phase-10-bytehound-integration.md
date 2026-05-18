# Phase 10 — Bytehound integration

A debugger that knows where execution is but not where memory came
from is half-blind. Bytehound — Jan Bujak's pure-Rust(-ish) memory
profiler — already records every allocation and deallocation with a
backtrace, on every architecture we care about (AMD64, AArch64, ARM,
MIPS64). This phase wires that record into BugStalker so the
debugger can answer "what's on the heap right now?", "where was this
pointer allocated?", and "when was it freed?" — at every stop, in
post-mortem, and across reverse-step boundaries.

The design treats bytehound as the upstream the way Phase 5 treats
`rr`: an external tool we cooperate with by reading its on-disk and
on-wire formats. We do not link against it.

Cross-platform from day one: Linux is the primary target,
Darwin/aarch64 is a co-equal target. Bytehound's macOS port is
in flight upstream; this phase is designed against the format and
the IPC, not the platform-specific shim, so the Darwin path opens
the moment upstream lands `DYLD_INSERT_LIBRARIES`-style attach.
Until then, Tier 1 (post-mortem `.dat` replay) works on Darwin the
day this phase ships — `.dat` files generated on a Linux box are
analysable on a Mac.

## Why this is its own phase

A naïve "shell out to bytehound" implementation ships in a weekend
and is useless. It would not understand BugStalker's address-space
model, would not survive process restart in time-travel mode, would
not feed the Phase 6 overlay, and would not expose any UX above
`heap.json`. Doing it well needs:

- A Rust parser for the bytehound `.dat` event stream — read-only,
  zero-copy where possible, addressable by event-index.
- A live-mode IPC against the bytehound preload library so heap
  state is queryable mid-run.
- A timeline splice that sits next to Phase 5's `Event` stream so
  reverse stepping and heap queries share one notion of "now".
- Source-line attribution that goes through the same
  `.debug_line` decoder Phase 6 already uses.
- DAP shapes a UI client can drive.

These are five distinct subsystems. Hence a phase.

## Why bytehound (and not something else)

Surveyed alternatives:

| Tool | Verdict |
| ---- | ------- |
| `heaptrack` (KDE) | C++, KDE-flavoured, GUI-coupled. Dump format is a candidate import path (bytehound exports it) but the runtime is not a fit. |
| `valgrind massif` / `DHAT` | Stop-the-world. ~20× slowdown. Unusable inside an interactive debugger session. |
| `jemalloc` profiling | Coarse. Allocator-locked. AArch64 jemalloc story is fragile. |
| `LD_PRELOAD` rolled in-house | Six months of work to match bytehound's unwinder cost; no upside. |
| `tracy` memory hooks | Excellent live profiler; no .dat-style post-mortem; clock-synchronisation with Phase 5 is harder than bytehound's monotonic event index. |

Bytehound wins on: licence (Apache-2.0/MIT), architecture coverage
(AArch64 included), unwinder cost (its tailor-made unwinder is
typically an order of magnitude cheaper than libunwind-driven peers),
and on having both a streaming socket and a stable `.dat` format.

## Integration in three tiers

Mirroring Phase 5: tier order = ship order.

| Tier | Mechanism | Cost during run | Window | Pairs with |
| ---- | --------- | --------------- | ------ | ---------- |
| 1 | Post-mortem `.dat` replay | nil | full session | nothing — ships standalone |
| 2 | Live `LD_PRELOAD` + IPC | 5–15 % wall-clock | live | breakpoint stops |
| 3 | Timeline splice with Phase 5 | as Phase 5 | as Phase 5 | reverse step / Tier 2/3 replay |

The phase's killer feature — heap-aware reverse stepping — is
explicitly Tier 3, not the headline. Tiers 1 and 2 ship first because
they are useful on their own and de-risk the parser and IPC.

## Tier 1 — Post-mortem `.dat` replay

The user has already run their program under `LD_PRELOAD=libbytehound.so`
once and has a `memory-profiling_*.dat`. They open it from inside
BugStalker:

```text
(bs) heap load memory-profiling_43210.dat
loaded 18 412 events; 14 532 alive at end-of-trace
(bs) heap top --by-size
       bytes  count  site
   142.3 MiB    142  src/server.rs:184  Vec::with_capacity
    18.1 MiB     12  src/cache.rs:42    HashMap::insert (resize)
   ...
(bs) heap leaks
   18 412 events    14 532 alive    142.3 MiB still allocated
   first leak: src/server.rs:184  142 × Vec::with_capacity (15.3 KiB each)
(bs) heap from-site src/server.rs:184
   142 events between t=0.31s and t=11.42s
   peak live: 142   peak bytes: 142.3 MiB   freed: 0
```

### Implementation

1. **`.dat` parser** (`bs-heap::format`). Bytehound's format is a
   stream of typed records (`AllocFull`, `AllocPartial`,
   `Free`, `Unknown`, `Marker`, `MemoryDump`). We parse it
   straight — no full-file load — using the same streaming pattern
   `bs-replay-engine::format::trace_reader` uses.
2. **In-memory tree** (`bs-heap::post_mortem`). Stack frames are
   interned; live allocations are stored as a sorted-by-address
   B-tree. `heap top` is an aggregation pass; `heap leaks` is the
   post-trace residual.
3. **DWARF crossover**. Bytehound records PCs only. We map PCs to
   `(file, line)` via the same `gimli` decoder Phase 6 already
   carries. Inlined-frame handling: identical to Phase 6's
   `.eh_frame_hdr` → inline-tree lookup.
4. **Symbol resolution**. The `.dat` file embeds `/proc/self/maps`
   snapshots. We map module + offset → symbol via the existing
   BugStalker symbol cache; rust-mangle-tree (Phase 2) demangles.

### Effort

~3 weeks. Format parser is the long pole; the rest reuses existing
infrastructure.

## Tier 2 — Live attach via the platform preload mechanism

BugStalker spawns the debuggee with the preload library and a side
channel:

```text
$ bs run --heap-track ./my-server
[bs] LD_PRELOAD=libbytehound.so MEMORY_PROFILER_OUTPUT=… ./my-server
   (Darwin: DYLD_INSERT_LIBRARIES=libbytehound.dylib + DYLD_FORCE_FLAT_NAMESPACE=1)
[bs] heap-track active (output: /tmp/bs-heap-43210.dat)

(bs) break src/server.rs:200
(bs) run
breakpoint hit at src/server.rs:200
(bs) heap top
       bytes  count  site
    32.1 MiB    142  src/server.rs:184  Vec::with_capacity
   ...
```

### Mechanism

Bytehound writes to a file (`MEMORY_PROFILER_OUTPUT`) or streams to
a socket (`MEMORY_PROFILER_OUTPUT_TYPE=tcp`). We tail the file with
the platform's file-event API — `inotify` on Linux, `kqueue`
(`EVFILT_VNODE`) on Darwin — through a thin abstraction in
`bs-heap::format::tail`. We do not interpose ourselves between the
preload and its output. At every breakpoint stop:

1. The tailer says "new bytes available" → drain to the parser's
   buffer.
2. The parser updates the in-memory tree (same one Tier 1 builds).
3. `heap top` / `heap leaks` answer from the tree.

The debuggee continues the moment the user types `continue`; the
parser thread keeps draining concurrently.

### Platform notes

- **Linux**: `LD_PRELOAD=libbytehound.so` is the standard mechanism.
  Works on glibc and musl. SUID/SGID binaries silently strip
  `LD_PRELOAD` — same caveat as every other `LD_PRELOAD`-based
  tool; we surface a clear warning when the launched binary's
  inode shows the SUID bit.
- **Darwin**: `DYLD_INSERT_LIBRARIES=libbytehound.dylib` plus
  `DYLD_FORCE_FLAT_NAMESPACE=1` (so symbol interposition works).
  System Integrity Protection strips both env vars when launching
  Apple-signed binaries (e.g. `/usr/bin/zsh`); we error early
  rather than appear to attach silently. Bytehound's Darwin port
  is upstream-WIP at the time of writing; this phase is designed
  to drop in the moment it lands and is testable on Linux from
  day one.

### Why not direct IPC

Bytehound supports a TCP streaming mode (`bytehound server
--listen :8080`). We considered piggy-backing on it. Rejected: the
file path is simpler, has no auth surface, survives BugStalker
restart, and matches Tier 1's parser exactly. One code path; two
sources (file vs. socket) added only if a real need surfaces.

### Cost during run

Bytehound's preload imposes 5–15 % wall-clock overhead depending on
allocation rate. We add nothing on top — the parser runs in
BugStalker, not the debuggee.

### Effort

~2 weeks. Mostly inotify + parser-state-machine glue.

## Tier 3 — Timeline splice with Phase 5

Phase 5 records syscalls, signals, and instruction traps as
`Event::*` variants on a global event-index axis. Bytehound records
allocs/frees on a separate axis (its own monotonic event id). To do
heap-aware time travel, the two axes must meet.

### The anchor protocol

At record start, BugStalker emits a `MARKER` record into the
bytehound stream (bytehound's `Marker` record type is documented and
unused-by-default). The marker carries the Phase 5 trace's `event_index`
at the moment it was emitted. We do this at every Tier 2 checkpoint
boundary — so the bytehound stream gets one anchor per checkpoint.
Anchors are sparse but sufficient: between two anchors the bytehound
event id is interpolatable against the Phase 5 event index by the
ratio of allocation events to total events. Imprecision is bounded
by the inter-checkpoint gap (median ~5 s of trace).

For exact alignment within a checkpoint, the parser holds the
*delta-from-anchor* for every allocation; the lookup
"heap state at Phase 5 event_index N" walks back from the
nearest-after anchor and replays freed allocations.

### Heap-aware reverse stepping

Three new commands, all Phase-5-conditional (refuse with a clear
error if no replay session is active):

```text
(bs) rstep-alloc
   stepped backward to last allocation event;
   now at src/cache.rs:142 (4 KiB allocated)
(bs) rstep-free 0x7f...3a40
   stepped backward to the free of 0x7f...3a40;
   now at src/cache.rs:208
(bs) heap rstep-uaf 0x7f...3a40
   pointer was freed at event 18 412 (src/cache.rs:208)
   first read at event 18 593 (src/handler.rs:42)
   180 events between free and use-after-free site
```

### Implementation

`bs-heap::replay_link` wraps `bs_replay_engine::driver::TraceReplayer`
with a side-by-side bytehound iterator. Both advance to the same
event_index together; the heap state is queryable at any
playhead position.

### Limits — the same as Phase 5's

- Tier 1 of Phase 5 (PT decode without checkpoint) loses memory
  state between PT samples. Heap state inherits the same window.
- Tier 2 of Phase 5 has best-effort determinism. Allocations replayed
  from a fork that diverges (different system clock, different
  network read) may produce different addresses. We anchor to call
  sites + size, not addresses, to survive this; a fresh address on
  replay is treated as the same allocation if its (site, size, order)
  matches.
- Tier 3 of Phase 5 is exact-deterministic. Heap replay is exact.

### Effort

~4 weeks. The marker-anchor protocol, the bidirectional iterator,
and the three commands.

## Phase 6 synergy — combined gutter overlay

Phase 6 paints cycle-counts on the source margin. With bytehound
loaded, the same gutter shows allocation hot-spots:

```text
123  fn handler(req: Request) -> Response {
124      let buf = Vec::with_capacity(req.size);     ← 4.2 ms · 142 MiB · 1.4 K allocs
125      let parsed = parse(&buf);                   ← 18.4 ms · 0 B · 0
126      respond(parsed)
127  }
```

Two columns, same renderer. The gutter is rendered once at-stop;
neither subsystem touches the debuggee.

DAP custom request `bs/heapOverlay` mirrors `bs/perfOverlay`:

```text
request:  { command: "bs/heapOverlay", arguments: { source } }
response: { body: { lines: [{ line, bytes, allocCount, freeCount }, ...] } }
```

Toggle: `bs/heapOverlayEnable` / `bs/heapOverlayDisable`. Console
keybinding `H` (alongside Phase 6's `p`/`P`).

### Effort

~1 week. The decoder + aggregator are reused; new piece is the
column merge in the renderer.

## Phase 11 handoff — precise trace providers

Phase 10 must not depend on Intel PT specifically. Heap timelines
depend on ordered, source-attributed execution events, and those can
come from Phase 6 Intel PT today or from Phase 11's vendor-neutral
precise-trace provider model later.

The important contract is the provider's precision:

- Intel PT and any future verified AMD packet trace can advertise
  instruction-exact events.
- AMD LBR Stack / AMD Processor Trace fallback advertises
  branch-exact events: enough for branch-bound heap correlation and
  checkpoint rendezvous, not enough to promise every in-block
  instruction.
- Apple Processor Trace and ARM CoreSight ETM join through the same
  decoded-event interface when their platform backends land.

Phase 10's `heap at`, `rstep-alloc`, `rstep-free`, and `heap rstep-uaf`
commands should surface that precision in their responses. If the
selected trace provider is branch-exact, the command can still move to
the closest safe branch/checkpoint anchor, but the UI must say that it
landed at branch precision rather than exact instruction precision.

## Crate layout

```text
crates/
└── bs-heap/
    ├── Cargo.toml
    └── src/
        ├── lib.rs
        ├── format/
        │   ├── mod.rs
        │   ├── reader.rs       # streaming .dat parser
        │   ├── records.rs      # AllocFull, AllocPartial, Free, Marker
        │   └── tail/
        │       ├── mod.rs      # FileTailer trait
        │       ├── linux.rs    # inotify backend
        │       └── darwin.rs   # kqueue/EVFILT_VNODE backend
        ├── post_mortem/
        │   ├── mod.rs
        │   ├── tree.rs         # interned-frame allocation tree
        │   └── queries.rs      # top, leaks, from_site
        ├── live/
        │   ├── mod.rs
        │   ├── spawn.rs        # LD_PRELOAD wiring at child launch
        │   └── tail_thread.rs  # parser thread driving from inotify
        ├── replay_link/
        │   ├── mod.rs
        │   ├── anchor.rs       # marker-record protocol
        │   └── reverse.rs      # rstep-alloc / rstep-free / rstep-uaf
        ├── overlay.rs          # Phase 6 hook
        └── dap.rs              # bs/heap* request shapes
```

Five subdirectories matches the five subsystems called out above.

## DAP integration

Custom requests follow the `bs/heap*` family — same naming style
Phase 5 uses for `bs/replay*`:

| Request | Returns |
| ------- | ------- |
| `bs/heapLoad` | `{ totalEvents, aliveAtEnd, recordedAt }` |
| `bs/heapTop` | `[{ bytes, count, site, file, line }, ...]` |
| `bs/heapLeaks` | `[{ site, count, bytes }, ...]` |
| `bs/heapFromSite` | `{ count, peakLive, peakBytes, freed }` |
| `bs/heapAt` | `{ live: [...] }` (heap state at an event_index) |
| `bs/heapOverlay` | per-line counts (mirror of `bs/perfOverlay`) |
| `bs/rstepAlloc` | new playhead position |
| `bs/rstepFree` | new playhead position + originating site |

Wire shapes live in `crates/bs-heap/src/dap.rs`; serde-only at the
DAP server boundary, like Phase 5.

## Console UX

Six new commands. All under a single `heap` namespace; the time-travel
ones live under the existing `r*` family for consistency with Phase 5.

```text
heap load <path>           # Tier 1
heap top [--by size|count|site]
heap leaks
heap from-site <file:line>
heap at <event_index>      # Tier 3
rstep-alloc                # Tier 3
rstep-free <ptr>           # Tier 3
heap rstep-uaf <ptr>       # Tier 3
```

## Specifications

- Bytehound source — <https://github.com/koute/bytehound>. Pinned by
  commit hash in CI; the `.dat` format is read straight from the
  source.
- `.dat` record layout — `bytehound/cli-core/src/loader.rs` (reader)
  and `bytehound/preload/src/event.rs` (writer). Mirror in Rust;
  no FFI.
- Heaptrack export format — for future cross-tool import
  <https://invent.kde.org/sdk/heaptrack>.
- Bytehound book — <https://koute.github.io/bytehound/>. End-user
  documentation. Useful for cross-checking semantics of `Marker`
  records and the streaming protocol.
- DWARF 5 §6.2 `.debug_line` — PC → (file, line). Same crossover
  Phase 6 already uses.
- `inotify(7)` — <https://man7.org/linux/man-pages/man7/inotify.7.html>.
  Tail-watching mechanism for live mode on Linux.
- `kqueue(2)` and `EVFILT_VNODE` — Apple Developer Library and
  FreeBSD man pages. Tail-watching mechanism for live mode on
  Darwin.
- `dyld(1)` — `man dyld`, plus the loader source at
  <https://github.com/apple-oss-distributions/dyld>. Authoritative
  on `DYLD_INSERT_LIBRARIES` semantics, SIP behaviour, and
  flat-vs-two-level namespace.

## Invariants

```rust
// Format parser: every record's declared length matches what it
// consumed.
debug_assert_eq!(record_end - record_start, declared_len);

// Allocation tree: total live bytes never goes negative.
debug_assert!(self.live_bytes >= 0);

// Allocation tree: a Free record always references an allocation
// the tree has seen, OR is flagged as "unknown" (preload missed
// the alloc — pre-attach allocations).
debug_assert!(self.tree.contains(ptr) || record.flags.unknown);

// Anchor protocol: marker timestamps strictly increase.
debug_assert!(new_marker.event_index > prev_marker.event_index);

// Anchor protocol: at most one marker per Phase 5 checkpoint.
debug_assert!(self.markers.len() <= self.phase5_checkpoints);

// Overlay query: per-line counts are non-decreasing through a
// session (allocations don't un-happen after they're recorded).
debug_assert!(new_alloc_count >= old_alloc_count);

// Reverse step: rstep-alloc lands on a PC the trace recorded as
// the entry to the allocator (i.e. the PC matches the bytehound
// frame's leaf).
debug_assert_eq!(playhead.pc, expected_alloc_site_pc);
```

None of these run inside the debuggee. They run only in BugStalker's
parser thread and replay driver — same architectural rule as Phase 5
and Phase 6: instrumentation lives outside the tracee.

## Non-goals

- Not a profiler GUI. Bytehound's web UI is excellent; we are not
  trying to replace it. Use `bytehound server` for visualisation
  workflows; use BugStalker's heap commands when you are *already*
  in the debugger.
- Not a leak detector. We surface allocations alive at end-of-trace;
  classifying them as "leaks" is a human judgement.
- Not a replacement for `valgrind --tool=memcheck`. Bytehound tracks
  what was allocated; it does not detect uninitialised reads or
  out-of-bounds accesses.
- Not for jemalloc-by-default crates without explicit opt-in.
  Bytehound's jemalloc support is AMD64-only and requires the
  `jemallocator` crate. We surface a clear error rather than
  pretend.

## Pure-Rust policy

Bytehound itself is a separate process. BugStalker does not link it
in. We bring in:

- **`bs-heap`** — pure Rust. No C deps. Just `gimli` (already a
  Phase 6 dep), `inotify`/`rustix` for tail-following, and our own
  format parser.
- **Bytehound binary** — external; the user installs it. Same model
  as Phase 5's `rr`-as-CI-oracle.
- **`libbytehound.so` runtime** — the user's debuggee loads it via
  `LD_PRELOAD`. It is C-and-Rust; that is the user's process, not
  ours. Our policy applies to BugStalker's binary.

`cargo build --features heap-track` is pure Rust.

## Effort breakdown

| Sub-phase | Description | Weeks |
| --------- | ----------- | ----- |
| 10A | `.dat` format parser + invariants | 2 |
| 10B | Post-mortem queries + console commands | 1 |
| 10C | Live attach: spawn + tail (inotify + kqueue) + parser thread | 2 |
| 10D | Marker-anchor protocol + bidirectional iterator | 2 |
| 10E | `rstep-alloc` / `rstep-free` / `rstep-uaf` | 2 |
| 10F | Phase 6 overlay merge | 1 |
| 10G | DAP shapes + handlers | 1 |
| 10H | Documentation + cross-tool tests | 1 |
| 10I | Bytehound version pinning + CI dev-dep | 1 |
| | **Total** | **13** |

The Darwin port of bytehound itself is *not* in this phase's budget;
it is upstream work. When upstream lands, sub-phases 10C–10E
become exercisable on Darwin without further BugStalker effort.
Tier 1 (10A + 10B) is platform-portable from the day it ships.

## Acceptance criteria

- Tier 1 (`heap load`, `heap top`, `heap leaks`, `heap from-site`)
  works against a bytehound `.dat` recorded on the same machine.
- Tier 2 (`bs run --heap-track`) launches a debuggee with the preload
  attached, drains the file via `inotify`, and answers `heap top`
  at every breakpoint stop.
- Tier 3 fuses with an active Phase 5 replay: `heap at <event_index>`
  returns the correct live set; `rstep-alloc` lands on a recorded
  allocation; `rstep-free <ptr>` lands on the matching free.
- Phase 6 overlay shows a heap column alongside the cycle column on
  the same source view.
- DAP custom requests round-trip via the AI-bot scripting surface
  (Phase 9) and produce the same answers as the console.
- Differential test in CI: a recorded `.dat` is compared against a
  freshly-decoded one; per-site totals match within 0 bytes.
- Pure-Rust build succeeds (`cargo build --features heap-track`)
  with `cargo deny` confirming no new C deps in the BugStalker
  binary.

## Open questions

- **Bytehound version pinning.** The `.dat` format is stable enough
  for `bytehound` to ship 5+ years of releases without breaking it,
  but we should pin to a known commit and bump deliberately.
  Resolution: vendor the `cli-core` record types in `bs-heap::format`
  and gate the version with a CI smoke test against the bytehound
  binary at the pinned commit.
- **AArch64 unwinder edge cases.** Bytehound's tailor-made unwinder
  is documented to be excellent on AMD64; AArch64 coverage is real
  but less battle-tested. We treat it as "Tier 2 best-effort on
  AArch64 until validated"; same-tier on AMD64.
- **Darwin upstream timing.** Bytehound's macOS port is in flight
  upstream. Until it lands, Tier 1 (post-mortem `.dat` replay)
  works on Darwin against Linux-recorded files; Tier 2 / Tier 3
  on Darwin gate on the upstream merge. We pin the bytehound
  commit independently per platform so a Darwin-broken commit
  doesn't block the Linux build.
- **Streaming mode.** Should we support bytehound's TCP streaming as
  a third source (alongside `.dat` file + `inotify`-tail)? Default
  no; revisit only if a real workflow demands it.
- **Heaptrack import.** Bytehound exports to Heaptrack format; the
  reverse is non-trivial. Out of scope; users who have Heaptrack
  files convert them with `heaptrack_print --json` then load via
  Tier 1 once we add a JSON adapter.

The bytehound link makes BugStalker the first interactive debugger
on Linux that can answer "where did this byte come from?" inline
with reverse stepping. Phase 5 gave us *when*; Phase 6 gave us *how
expensive*; this phase gives us *where it came from*.

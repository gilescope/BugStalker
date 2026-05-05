# Phase 6 — Performance overlay

When stopped at a breakpoint, show per-line cost since the last stop.
The architecture must impose **zero observable slowdown** on
"play to next breakpoint".

## The architectural rule

> Hardware writes; debuggee runs naked; decoding waits for the stop.

- No software instrumentation injected into the debuggee.
- No single-step counting (would slow execution by orders of magnitude).
- No inline trace decoding during execution.
- The CPU's PMU writes samples to a kernel ring buffer the debuggee
  cannot perceive. A separate debugger thread drains the ring on its
  own timer. Decoding into `(file, line)` heat-maps happens at-stop.

The cost the debuggee pays during free execution is whatever the PMU
imposes: typically 1–3 % CPU for cycles sampling, 1–5 % for Intel PT.
That is *the* cost — there is no debugger-side overhead during the
"play" window.

## Crate layout

```text
crates/
└── bs-perf/
    ├── Cargo.toml
    └── src/
        ├── lib.rs
        ├── ring.rs            # cross-platform ring buffer abstraction
        ├── linux/
        │   ├── mod.rs
        │   ├── perf_event.rs  # perf_event_open wrapping
        │   ├── intel_pt.rs    # behind `intel-pt` feature
        │   └── arm_spe.rs     # behind `arm-spe` feature, future
        ├── darwin/
        │   ├── mod.rs
        │   └── kperf.rs       # private API binding
        ├── decoder.rs         # PC → (file, line) via .debug_line
        ├── aggregator.rs      # heat-map data structure
        └── overlay.rs         # render hooks
```

## Linux back-end

### Cycles + IP sampling (universal tier)

Use `perf_event_open(2)` with:

```c
attr.type = PERF_TYPE_HARDWARE
attr.config = PERF_COUNT_HW_CPU_CYCLES
attr.sample_freq = 1000          // 1 kHz default, tunable
attr.freq = 1
attr.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME
                  | PERF_SAMPLE_CPU | PERF_SAMPLE_CALLCHAIN
attr.exclude_kernel = 1
attr.exclude_hv = 1
attr.precise_ip = 2              // PEBS for low-skid sampling on Intel
attr.disabled = 1                // enable when target is launched
```

Open one event per debuggee thread (or one per CPU with
`PERF_FLAG_FD_NO_GROUP` and tid-filter). `mmap` a 256-page
(1 MB) ring buffer per event. Track new threads via the existing
thread-management plumbing in
`src/debugger/debugee/tracee/`.

### Intel PT (precise tier)

Behind cargo feature `intel-pt`. Open the `intel_pt` PMU type
(discover via `/sys/bus/event_source/devices/intel_pt/type`). Buffer
size: 64 MB AUX area + 4 MB data area per CPU, large enough to capture
~5 s of trace at 1 GHz.

Decode at-stop with `libipt-rs` (binding to Intel's C `libipt`) plus
`iced-x86` (pure Rust) for instruction info. We deliberately do NOT
use `libxed` (Intel's C disassembler that ships with libipt) — the
pure-Rust `iced-x86` is mature, fast, and aligns with the project's
pure-Rust policy. Decoded events carry exact PC sequences; we
aggregate them into `(file, line) -> cycle_count` via `.debug_line`.

`libipt` is the only C dependency in this entire phase. It is gated
behind the `intel-pt` cargo feature; the default `bs-perf` build does
cycles sampling only and is pure Rust. See `## Pure-Rust policy` at
the end of this document.

PT requires:

- Linux ≥ 4.1 with `CONFIG_INTEL_PT=y`
- Intel CPU ≥ Broadwell (Gen 5) for the PT PMU
- `kernel.perf_event_paranoid <= 1` or `CAP_SYS_ADMIN`

Detection: if any precondition fails, log an info message and fall
back to cycles sampling. PT remains opt-in for the user; the default
is sampling.

### Ring buffer drain

A single background thread (`bs-perf-collector`) wakes via
`epoll_wait` on the perf fds. On wake:

1. Read the ring's tail pointer.
2. Iterate over `PERF_RECORD_SAMPLE` records.
3. For each: increment `aggregator[pc].cycle_count`; record tid,
   timestamp, and (for PT) instruction-level deltas.
4. Advance the head pointer atomically.

The thread runs at default priority. Tunable: `BS_PERF_DRAIN_HZ` (env
var or config) for poll frequency (default 100 Hz).

The aggregator is a `DashMap<Pc, SampleCounters>` keyed by raw PC
(no resolution into file/line until at-stop). Cheap insert and
update; resolution is the expensive step we defer.

## Darwin back-end

`kperf` is a private framework. The `samply` project demonstrates
viable usage on Apple Silicon and Intel Macs without root, modulo
SIP-restricted entitlements.

### Cycles + IP sampling

Bind to `kperfdata.framework`:

- `kpc_set_counting()` — enable PMC counters.
- `kpc_set_thread_counting()` — per-thread counters.
- `kperf_sample_set_period_us()` — set sampling interval.
- `kperf_sample_set_action_func()` — register PC-capture action.

The exact symbol set varies between macOS 13/14/15. `bs-perf` ships
a small shim that detects the macOS version at runtime and binds the
appropriate symbols via `dlsym`. If the binding fails (Apple changed
the API again), log "perf overlay unavailable on this macOS version"
and disable the feature. Tests skip with that message.

### No PT-equivalent

ARM SPE / BRBE are not exposed by Darwin. Apple's PMU has cycle and
instruction counters but no full-execution-trace facility accessible
from userspace. Document this gap in the overlay's UI.

### Coarse fallback

If kperf bindings fail, use `proc_pid_rusage()` to report whole-process
cycle counts at each stop. Useful for "this run cost X cycles" but not
heat-mapping. Surfaced as the per-stop summary line; no gutter.

## Pure-Rust policy

This phase has one C dependency: Intel `libipt` for Processor Trace
decoding. Everything else is pure Rust.

- Cycles + IP sampling tier (universal): pure Rust. Uses `rustix`
  (preferred) or `perf-event-open-sys` for `perf_event_open(2)`
  bindings; ring buffer drain is pure Rust.
- Intel PT tier: `libipt` (C) gated behind cargo feature `intel-pt`;
  paired with `iced-x86` (pure Rust) for instruction decoding.
- Darwin `kperf` tier: pure Rust FFI to Apple private symbols via
  `dlsym`; no C library bundled.
- ARM SPE tier (future): expected to be pure Rust against
  `perf_event_open` for the SPE PMU type.

### Long-term: pure-Rust PT decoder

A pure-Rust replacement for `libipt` is in scope as a separate,
longer-term effort. Intel PT's binary format is documented in Intel
SDM Volume 3 Chapter 36. Writing a pure-Rust decoder is estimated at
~2 months engineer-time and would land as a new workspace crate
`crates/bs-pt-decoder/`. Once shipped, the `intel-pt` cargo feature
no longer pulls C code; the feature flag remains as a build-cost
gate (PT decoding is bulky regardless of language).

Until that crate exists, the `intel-pt` feature carries a clear
disclaimer in its docs that opting in introduces C linkage.

### Default cargo features

```toml
[features]
default = []
perf = ["dep:bs-perf"]                          # pure Rust
intel-pt = ["perf", "bs-perf/intel-pt"]         # introduces libipt (C)
arm-spe = ["perf", "bs-perf/arm-spe"]           # future, pure Rust
```

A user running `cargo build` with no flags gets zero C dependencies
from Phase 6. Opting into `--features intel-pt` is the single
explicit step that introduces C linkage.

## Decoder

PC-to-line resolution uses BugStalker's existing DWARF
`.debug_line` parser (in `src/debugger/debugee/dwarf/`). Cache the
result per-PC: subsequent samples at the same PC reuse the cached
`(file, line)` mapping.

Inlined frames matter: a sample at PC inside an inlined function
should attribute to *both* the inlined call site and the inline
function definition. Use the existing
`DW_TAG_inlined_subroutine` data to produce a primary attribution
and an inlined-from attribution.

## Aggregator and decay

```rust
pub struct PerfData {
    /// Cumulative samples since attach.
    pub cumulative: HashMap<(FileId, Line), SampleCount>,
    /// Samples attributed to the most recent run-to-stop.
    pub last_run: HashMap<(FileId, Line), SampleCount>,
    /// Per-stop totals.
    pub stops: VecDeque<StopSummary>,
}

pub struct StopSummary {
    pub run_cycles: u64,
    pub run_wall_ns: u64,
    pub top_lines: Vec<(FileId, Line, SampleCount)>,
}
```

The aggregator clears `last_run` on `continue`; `stops` keeps the last
N (default 32) for history navigation.

## UI

### TUI

New panel: source view with a left gutter colour-coded by sample
density.

- Cold lines: default background.
- Warm lines: shaded blue-to-yellow.
- Hot lines: red.
- The hottest line in the last run gets a `►` marker.

Toggle with key `p`. Default off; opt-in.

Cycle counts shown as a margin column on demand (key `P`).

### Console

`perf` command:

```text
(bs) perf
last run: 3.2M cycles, 1.4 ms wall

src/handler.rs
   42  | let req = parse(input);            [ 220k cy ]
   43  | match req {
   44  |     Get(path) => {
   45  |         let body = fs::read(&path);  [ 2.1M cy ]  ◄ hottest
   46  |         Response::ok(body)
   47  |     }
   48  | }

(bs) perf history
stop #1: 3.2M cy   1.4ms     hot=src/handler.rs:45
stop #2: 280k cy  130 us     hot=src/handler.rs:46
...
```

`perf line src/handler.rs:45` prints the per-stop history for one line.

### Per-stop summary line

After every breakpoint hit, the status line shows:

```text
stopped at src/handler.rs:46 — run cost 3.2M cy / 1.4 ms / hot src/handler.rs:45
```

## Cargo features

```toml
[features]
default = []
perf = ["dep:bs-perf"]
intel-pt = ["perf", "bs-perf/intel-pt"]
arm-spe = ["perf", "bs-perf/arm-spe"]   # future
```

Default builds carry no perf code at all. Users opt in.

## Test plan

- `crates/bs-perf/tests/sampling.rs` — Linux only: launch a
  CPU-bound test binary, collect samples for 100 ms, assert non-zero
  PC distribution concentrated in the expected function.
- `crates/bs-perf/tests/intel_pt.rs` — Linux + PT-capable CPU: assert
  exact instruction count between two markers.
- `crates/bs-perf/tests/darwin_kperf.rs` — Darwin only: minimal smoke
  test of kperf binding; skips with reason if bindings fail.
- `tests/debugger/perf_overlay.rs` — end-to-end: attach BugStalker to
  a test debuggee, set a breakpoint, run, verify the overlay aggregator
  has data for the run and resolves to expected lines.
- `tests/debugger/perf_overlay_no_slowdown.rs` — regression: time a
  run-to-breakpoint with overlay on vs off, assert overhead <= 5 %.

## Acceptance criteria

- Linux cycles sampling tier works on x86_64 and aarch64.
- Intel PT tier works on Broadwell+ Intel with `perf_event_paranoid <= 1`.
- Darwin kperf tier works on at least macOS 14 and 15 (current as of
  this writing).
- Overhead measured at < 5 % wall-time on a tight CPU-bound debuggee
  with overlay enabled.
- Graceful degradation: on a platform/configuration with no perf
  back-end, the `perf` command prints "perf overlay unavailable on
  this platform: `<reason>`" and BugStalker continues normally.
- TUI heat-map rendering correct on a representative real-world
  debuggee (e.g. `ripgrep` searching a large file).

## Effort estimate

~6 weeks engineer-time.

- Linux cycles sampling + decoder + aggregator: ~2 weeks.
- TUI + console UI: ~1 week.
- Intel PT integration (libipt-rs binding work, decoder): ~2 weeks.
- Darwin kperf binding: ~1 week (samply provides the recipe).
- Tests, docs, release: ~3 days.

## Risks

- **Apple changes kperf**: catastrophic for Darwin support but
  contained. Detect at runtime, degrade gracefully, file an issue.
- **`perf_event_paranoid` defaults vary**: many distros set it to 2
  or 3 by default. Document the requirement clearly. Provide
  detection-and-helpful-error path.
- **PT decoder is large and slow**: `libipt` decoder is in C and not
  fast. For long traces (multi-GB AUX buffers), decoding may take
  seconds at-stop. Surface progress in the UI; allow cancellation.
- **Inlined frames misattribution**: aggressive optimisations make
  attribution coarse. Document and accept.

## DAP integration

Per-stop summary (cycles, wall-time, hottest line) attaches to the
`StoppedEvent` via a custom `body.bs_perf` field:

```text
{ event: "stopped", body: {
    reason: "breakpoint",
    threadId: N,
    bs_perf: { runCycles: 3_200_000, runWallNs: 1_400_000,
               hot: { source, line, sampleShare: 0.65 } }
}}
```

Heat-map data via custom request `bs/perfOverlay`:

```text
request:  { command: "bs/perfOverlay", arguments: { source } }
response: { body: { lines: [{ line, sampleCount, cycleCount }, ...] } }
```

VSCode renders the gutter via the existing inline-decoration API
keyed off these counts.

Toggling the overlay: `bs/perfOverlayEnable` /
`bs/perfOverlayDisable`. Honours the same `p`/`P` keybindings in TUI.

## Specifications

- `perf_event_open(2)` — <https://man7.org/linux/man-pages/man2/perf_event_open.2.html>. Authoritative.
- `perf_event.h` UAPI header — <https://github.com/torvalds/linux/blob/master/include/uapi/linux/perf_event.h>. Struct layouts, `PERF_SAMPLE_*`, `PERF_TYPE_HARDWARE` constants.
- Linux kernel perf-security guide — <https://docs.kernel.org/admin-guide/perf-security.html>. `kernel.perf_event_paranoid` thresholds.
- Intel® 64 and IA-32 Architectures Software Developer's Manual, Volume 3, Chapter 36 — Intel Processor Trace — <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>.
- Intel® libipt — <https://github.com/intel/libipt>. Decoder API and `doc/howto_libipt.md`.
- Intel® XED — <https://github.com/intelxed/xed>. Disassembler used inside libipt.
- DWARF 5 §6.2 — `.debug_line` section. PC → (file, line) resolution.
- ARM Architecture Reference Manual — Statistical Profiling Extension (SPE) — <https://developer.arm.com/documentation/ddi0487/latest/>. Future aarch64 path.
- ARMv9 Branch Record Buffer Extension (BRBE) — same source.
- AMD IBS PPR (Processor Programming Reference) — <https://www.amd.com/en/support/tech-docs>. Niche AMD path.
- Apple's `kperf` is undocumented; pin to a specific commit of `samply` — <https://github.com/mstange/samply> — for the binding shape.
- macOS PMU sampling reference (community) — <https://github.com/zhuowei/MacM1Compete>.
- `wholesym` — <https://github.com/mstange/wholesym>. Symbolication used by `samply`; useful for split-debuginfo handling.
- ELF `.eh_frame_hdr` and `.eh_frame` — <https://refspecs.linuxfoundation.org/LSB_5.0.0/LSB-Core-generic/LSB-Core-generic/ehframechpt.html>. Inlined-frame attribution.

## Invariants

The collector and decoder threads enforce the following invariants at runtime
(via `debug_assert!`). They guard the ring buffer protocol, sampling
correctness, decoder progress, and aggregator monotonicity. None of these
assertions run inside the debuggee; they run only in BugStalker's own threads.

```rust
// Ring buffer invariant: tail never overtakes head.
debug_assert!(self.tail <= self.head);

// Each `PERF_RECORD_SAMPLE` size matches the popcount of `sample_type`.
debug_assert_eq!(record_size, expected_size_for_sample_type(self.sample_type));

// Sampled PCs lie within a mapped executable region of the debuggee.
debug_assert!(self.is_mapped(pc),
    "sampled PC 0x{:x} not in any mapped region", pc);

// Aggregator counters monotonic-non-decreasing during a session.
debug_assert!(new_count >= old_count);

// Sample frequency in valid range (Linux clamps but we guard ourselves).
debug_assert!((1..=10_000).contains(&self.frequency_hz));

// Ring overflow is detectable: drain caught up or PERF_RECORD_LOST seen.
debug_assert!(self.bytes_lost == 0 || self.observed_lost_record);

// Intel PT preconditions.
debug_assert!(self.cpu_features.contains(CpuFeature::Pt));
debug_assert!(self.kernel_supports_intel_pt());

// PT decoder advances strictly forward through the trace buffer.
debug_assert!(self.pt_decoder.position() >= prev_position);

// Inlined-frame attribution: the inlining tree is a DAG, depth-bounded.
debug_assert!(inline_depth <= MAX_INLINE_DEPTH);
```

The no-slowdown architectural rule (debuggee runs naked) means none of these
asserts run *in* the debuggee — they run only in BugStalker's collector and
decoder threads. This matters because `debug_assert!` panicking would abort
our process, which is acceptable; aborting the debuggee would not be.

## Non-goals

- Not a profiler. We are not competing with `samply`, `perf record`,
  or `cargo flamegraph`. Use those for production profiling.
- Not always-on observability. The overlay starts at debugger attach
  and stops at detach.

The Intel PT trace window we capture here is *also* the substrate for
free reverse-stepping. That is covered in
`doc/plans/phase-5-time-travel.md` — Phase 6 produces the trace, Phase 5
exposes time-travel UX over it.

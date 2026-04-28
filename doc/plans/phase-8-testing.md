# Phase 8 — Testing strategy

A cross-cutting plan for *how we know it works*. Each preceding phase
has its own per-feature test plan; this document is the umbrella —
the test pyramid, the external corpora we lift (license-vetted), the
submodule layout, the CI matrix, the differential oracles.

BugStalker's current test inventory (4 743 lines under `tests/`):

| File | Lines | Coverage |
| ---------------------------- | ----- | ----------------------------- |
| `tests/debugger/variables.rs` | 2947 | value rendering — the heaviest file |
| `tests/debugger/watchpoint.rs` | 405 | watchpoints |
| `tests/debugger/main.rs` | 336 | top-level harness |
| `tests/debugger/breakpoints.rs` | 292 | breakpoints |
| `tests/debugger/steps.rs` | 225 | stepping |
| `tests/debugger/unwind.rs` | 147 | stack unwinding |
| `tests/debugger/multithreaded.rs` | 124 | thread management |
| `tests/debugger/signal.rs` | 115 | signal handling |
| `tests/debugger/io.rs` | 108 | I/O capture |
| `tests/debugger/symbol.rs` | 23 | symbol resolution |
| `tests/debugger/tokio.rs` | 21 | tokio runtime smoke |
| `tests/dap/*` | — | DAP protocol |
| `tests/darwin_smoke.rs` | — | Darwin port |

That is solid integration coverage. What it lacks: external corpora,
fuzz harnesses, differential oracles, property testing, dedicated
perf-regression and replay-determinism suites. Phase 8 fills those.

## License — we are MIT

`LICENSE` confirms BugStalker is MIT (Copyright 2026 Derevtsov
Konstantin). That governs what test code can live in-tree:

| License | Verdict |
| ---------------------- | --------------------------------------- |
| MIT, Apache-2.0, BSD, ISC | safe to vendor or submodule |
| LLVM Exception (Apache + LLVM) | safe |
| Mozilla Public License 2.0 | safe (file-level copyleft only) |
| LGPL-2.1+ | submodule fine; no static-link in release; read-only inspiration |
| GPL-2/GPL-3 | binary oracle only (CI tool, not vendored) |

Engineers must verify license before importing any external test
fixture. CI gate: `cargo deny check licenses` runs against the full
dependency graph including dev-dependencies; new additions must pass.

## The pyramid

Six layers, narrower as you climb:

1. **Unit tests** — pure functions, no debuggee. Each crate's
   `src/**/tests`. Fast, run on every save.
2. **Integration tests** — debugger drives a real debuggee binary.
   The existing `tests/debugger/*.rs` pattern. ~minutes.
3. **Property tests** — `proptest`/`arbitrary` random inputs against
   invariants. Run on PR; bounded iteration count.
4. **Differential tests** — same input through us *and* an oracle
   (`rustc-demangle`, `rr`, lldb, gdb); assert agreement.
5. **Fuzz tests** — `cargo fuzz`; long-running, separate CI job.
   Crashes are P0.
6. **Soak / regression** — long-haul runs of real workloads
   (BugStalker debugging itself, Cargo, ripgrep) checking for memory
   leaks, perf regressions, determinism drift.

Plus one orthogonal layer: **`cargo bench`** with `criterion`
across hot-path crates (`rust-mangle-tree`, `bs-perf`,
`bs-replay-engine`, BugStalker render path, attach-cold). Nightly
CI archives reports and posts a comment on any PR that regresses a
benchmark by >10 % vs the trailing 7-day median. This is the
project's performance-regression budget — there is no separate
budget doc; the bench suite is the budget, the CI alert is the
enforcement.

**Pulled forward in Phase 1 batch T:** the BugStalker render-path
bench (`benches/render_value.rs`) and the attach-cold bench
(`benches/attach_cold.rs`) now have real bodies (not Phase 0
placeholders) plus per-PR regression gating in `earthly +smoke`
via `Performance has regressed` from criterion. The full nightly
trailing-7-day-median scheme still lands here in Phase 8; the
between-PR check exists in the meantime so the project has
real-numbers regression detection from Phase 1 onward.

## Cross-cutting infrastructure

### `cargo nextest` everywhere

Memory note: `cargo nextest` is 20× faster than `cargo test
--test-threads=1` on Darwin once `task_for_pid` cache is warm.
Make `nextest` the default test runner; add an `Earthfile`
`+test` target that uses it. Update `Makefile` to point at
`nextest` for development.

### Test fixtures live in `tests/fixtures/`

Conventions:

```text
tests/
├── fixtures/
│   ├── debuggees/             # tiny rust programs; one per scenario
│   │   ├── stdlib_render/
│   │   │   ├── Cargo.toml
│   │   │   └── src/main.rs
│   │   ├── async_await/
│   │   ├── dyn_trait_chain/
│   │   ├── rc_cycles/
│   │   └── replay_corpus/
│   ├── corpus/                # data-only (mangled symbols, traces)
│   │   ├── mangled_v0/
│   │   ├── mangled_legacy/
│   │   ├── recorded_traces/
│   │   └── dwarf_pathological/
│   └── oracles/               # checked-in snapshots of oracle output
└── ...
```

The `debuggees/` directory has a `Cargo.toml` per fixture — each
compiles independently; tests build them via `cargo build --manifest-path
tests/fixtures/debuggees/<name>/Cargo.toml`. Existing `examples/`
crate already does similar; merge/align with this layout.

### Test traits and the harness

A `bs-test-harness` crate (`crates/bs-test-harness/`) abstracts the
launch-debuggee-set-breakpoint-render-assert flow that
`tests/debugger/variables.rs` open-codes today. Migrate the existing
4 743 lines incrementally; do not rewrite at once.

```rust
let mut h = TestHarness::launch("stdlib_render")?;
h.break_at("src/main.rs", 42)?;
h.run()?;
let v = h.var("my_vec")?;
assert_eq!(v.summary(), "Vec<i32>[3] = [1, 2, 3]");
```

### CI matrix

GitHub Actions matrix:

| Axis | Values |
| -------------- | ------------------------------------------------ |
| OS | linux-x86, linux-aarch64 (qemu), darwin-aarch64 |
| Rust | stable, MSRV (1.70 for `rust-mangle-tree`) |
| Linker | lld (default), wild (when available) |
| Mangling | legacy, v0 |
| PT | available, unavailable |
| Features | default, all-features, perf-only, replay-only |

Total cells: ~36; not all combinations meaningful. Define a curated
short-list (10–12 cells) that runs on every PR; full matrix nightly.

The 16-core x86 NixOS box (`192.168.1.137`, alias `x86`) is the
canonical Linux test host for CI runners that need PT and root
access.

## External test corpora — license-vetted

For each corpus: source, license, integration mode (vendor /
submodule / binary oracle), and which phase consumes it.

### Phase 1 (stdlib coverage)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| rustc debuginfo tests | `rust-lang/rust:tests/debuginfo/` | Apache-2.0 OR MIT | partial copy | The canonical Rust pretty-printer suite. ~150 test files (`.rs` + `.gdb`/`.lldb` script). Copy under `tests/fixtures/rustc-debuginfo/` with attribution; do not submodule the entire rust repo (1 GB+). Convert `.gdb`/`.lldb` script expectations to BugStalker assertions. |
| `rust-gdb`/`rust-lldb` test outputs | rustc CI | Apache-2.0 OR MIT | reference | Run them, snapshot outputs as ground truth for "what would lldb show?" |
| `gimli-rs` DWARF fixtures | `gimli-rs/gimli` | Apache-2.0 OR MIT | submodule (test-only) | Pathological DWARF inputs for parser robustness |

### Phase 2 (`rust-mangle-tree`)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| `rustc-demangle` test corpus | `rust-lang/rustc-demangle:tests/` | Apache-2.0 OR MIT | copy | Differential baseline. The crate's tests are short; copy in. |
| `rustc` mangling tests | `rust-lang/rust:tests/codegen/symbols/` | Apache-2.0 OR MIT | partial copy | v0 production tests; copy specific `.rs` files we need. |
| `addr2line` test binaries | `gimli-rs/addr2line:fixtures/` | Apache-2.0 OR MIT | submodule | Real-world binaries with mixed legacy/v0 symbols. |
| `rust-demangle.c` corpus | `LykenSol/rust-demangle.c` | MIT | copy | Same input, different language — useful third oracle. |
| Self-generated corpus | `nm` over `target/debug/bugstalker`, `cargo`, `ripgrep`, `bat` | MIT (output of MIT/Apache compilers on MIT/Apache code) | regenerable | Build script regenerates before fuzzing. |

### Phase 3 (dyn Trait + async)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| Cliff Biffle `lildb` tests | `cliffle/lildb` | Apache-2.0 OR MIT (verify) | partial copy | Async await-trace corpus; the original implementation we are matching. |
| `tokio-rs/tokio` test programs | `tokio-rs/tokio:examples/` | MIT | submodule (read-only) | Real-world async programs — harness drives BugStalker against tokio's example binaries. |
| `futures-rs` fixtures | `rust-lang/futures-rs` | Apache-2.0 OR MIT | submodule | Lower-level future composition tests. |
| Coroutine state-machine corpus | self-generated from rustc nightly | n/a | regenerable | Compile a battery of `async fn` shapes; assert state-machine detection. |

### Phase 4 (visualisers)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| LLDB `data-formatter` tests | `llvm/llvm-project:lldb/test/API/functionalities/data-formatter/` | Apache-2.0 with LLVM Exception | partial copy | LLDB's pretty-printer regression suite — patterns translate to our derive macro tests. |
| Visual Studio Natvis samples | Microsoft published samples | MIT (verify per file) | reference | Compare expressivity; do not vendor closed-license. |
| Wasm SDK examples | `bytecodealliance/component-model` | Apache-2.0 with LLVM Exception | submodule | WIT contract patterns. |

### Phase 5 (perf overlay)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| `samply` test workloads | `mstange/samply` | Apache-2.0 OR MIT | submodule | Reference for cycles-sampling correctness. |
| `libipt` PT decoder tests | `intel/libipt` | BSD-3-Clause | submodule | PT-trace decoder test corpus. |
| `wholesym` test fixtures | `mstange/wholesym` | Apache-2.0 OR MIT | submodule | Symbol resolution under split-debuginfo. |
| `perf` testsuite | `torvalds/linux:tools/perf/tests/` | GPL-2 | **NOT VENDORED** — binary oracle only | Run `perf record` on the same workload, compare aggregated cycle attribution at the file/line level (not source-coupled). |

### Phase 6 (time travel)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| `gVisor` syscall test corpus | `google/gvisor:test/syscalls/` | Apache-2.0 | submodule | The gold standard for syscall coverage. ~1000 test programs covering the entire Linux syscall surface. We use this to validate Tier 3B coverage. |
| `Firecracker` seccomp-bpf tests | `firecracker-microvm/firecracker:tests/` | Apache-2.0 | submodule | Seccomp filter authoring patterns and tests. |
| `criu` checkpoint tests | `checkpoint-restore/criu` | LGPL-2.1 | submodule (read-only) | Memory checkpoint patterns for Tier 2 fork-replay; LGPL means we don't link `libcriu` into release builds. |
| `syzkaller` syscall corpus | `google/syzkaller` | Apache-2.0 | submodule (corpus dir only) | Adversarial syscall sequences — fuzz our recorder against them. |
| Linux kernel selftests | `torvalds/linux:tools/testing/selftests/` | GPL-2 | **NOT VENDORED** — binary oracle | Run them under our recorder, ensure record-replay round-trip preserves their pass/fail outcome. |
| `rr` itself | `rr-debugger/rr` | GPL-2 | **CI ORACLE ONLY** | Apt-installed in CI; never linked, never read. Differential test compares syscall logs after normalisation. |

### Phase 7 (linker contract)

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| Wild's own integration tests | (user's project) | (TBD) | submodule once published | Joint test suite with `bs-debug-sections`. |
| `lief` binary parser fixtures | `lief-project/LIEF` | Apache-2.0 | submodule | ELF/Mach-O/wasm parsing edge cases for our section reader. |
| Object format conformance | `gimli-rs/object` | Apache-2.0 OR MIT | submodule | Parser test cases. |

### Cross-cutting

| Corpus | Source | License | Mode | Notes |
| ------ | ------ | ------- | ---- | ----- |
| `bytehound` test programs | `koute/bytehound` | MIT OR Apache-2.0 | reference | Heap profiler — workloads with interesting allocation patterns. |
| Real-world Rust binaries | `ripgrep`, `bat`, `cargo`, `bugstalker` itself | MIT/Apache | regenerable | Soak test target. Build in CI; run smoke tests against. |

## Submodule strategy

A few rules to keep `git submodule update --init` cost manageable:

1. Submodule **only what we read**, never the whole upstream
   repo. Use `git submodule add --depth=1` with a tag pin.
2. Pin to a specific tag, not a branch. Update via PR, not auto-bump.
3. Document the pin in `tests/fixtures/SUBMODULES.md` with the
   reason for that version.
4. Mirror submodule URLs to a vendor remote when possible — supply-
   chain risk on test fixtures is real (CLAUDE.md note about
   adversarial prompt injection from upstream sources applies here
   too).
5. For >100 MB upstream repos, use `git submodule add --filter=blob:none`
   to lazy-fetch.
6. CI caches the submodule tree by SHA — running tests does not
   re-clone every job.

Layout under `tests/fixtures/submodules/`:

```text
tests/fixtures/submodules/
├── gimli/                     # gimli-rs/gimli
├── addr2line/                 # gimli-rs/addr2line
├── rust-demangle-c/           # LykenSol/rust-demangle.c
├── tokio-examples/            # subset of tokio-rs/tokio
├── futures-rs/                # rust-lang/futures-rs
├── llvm-data-formatters/      # subset of llvm-project (LLDB tests)
├── samply/                    # mstange/samply
├── libipt/                    # intel/libipt
├── wholesym/                  # mstange/wholesym
├── gvisor-syscalls/           # google/gvisor (test/syscalls only)
├── firecracker-tests/         # firecracker-microvm/firecracker
├── criu/                      # checkpoint-restore/criu
├── syzkaller-corpus/          # google/syzkaller (corpus only)
└── lief/                      # lief-project/LIEF
```

## Differential testing

Three oracles, three phases:

### `rustc-demangle` oracle (Phase 2)

- For every mangled symbol in our corpus: parse with
  `rust-mangle-tree`, then format `Display` and `Display + #`.
- Compare byte-for-byte against `rustc-demangle::demangle().to_string()`.
- Differences are bugs in `rust-mangle-tree`; pin a specific
  `rustc-demangle` version and update the pin deliberately.
- Acceptance: 100 % match across the union of all listed corpora.

### lldb / `rust-gdb` oracles (Phases 1, 3)

- Run our renderer and `rust-lldb` (or `rust-gdb`) against the same
  fixture binary at the same breakpoint.
- Normalise both outputs (strip addresses, strip implementation-
  specific formatting) and assert structural equivalence.
- Where they differ, the *intentional* differences are recorded in
  `tests/oracles/divergences.md` with rationale (e.g. "we render
  `Mutex<T>` as `T [locked]`, lldb shows raw struct — this is on
  purpose").
- Run on every PR; surface unexpected divergences as test failures.

### `rr` oracle (Phase 6 Tier 3)

- Record the same workload through `rr` and our engine.
- Both produce a syscall log; normalise (drop timestamps, normalise
  fd numbers, sort thread interleavings consistently).
- Assert the logical sequence matches.
- Engineers do not read `rr` source; this is a binary-output
  comparison.
- Acceptance: 100 % match on a curated test corpus of ~50 small
  programs covering the recorded syscall surface.

## Property testing

Crate `bs-test-harness` exposes property generators:

```rust
proptest! {
    #[test]
    fn render_arbitrary_struct_does_not_panic(s in arb_rust_struct()) {
        let _ = render(&s);  // must not panic
    }

    #[test]
    fn demangle_arbitrary_v0_input_terminates(input in any::<Vec<u8>>()) {
        let _ = rust_mangle_tree::parse(&input_as_str);  // must not loop
    }
}
```

Property targets per phase:

- Phase 1: every value renderer terminates and emits valid UTF-8
  for any random byte input interpreted as the type.
- Phase 2: parser is panic-free, terminates within depth limit,
  Display roundtrips through parse-display-parse.
- Phase 3: niche detection never produces a different variant from
  the DWARF-discriminant path when both apply.
- Phase 4: visualiser interpreter never crashes on adversarial spec
  encoding.
- Phase 5: ring drain never reads past tail pointer regardless of
  producer interleaving.
- Phase 6: record→replay never produces a different PC trace from
  the recorded one (the property is determinism itself).

## Fuzzing

`cargo fuzz` per crate that takes external input:

| Crate | Fuzz targets |
| ---------------------- | ----------------------------------- |
| `rust-mangle-tree` | `parse_random`, `parse_real_binary` |
| `bs-debug-sections` | `parse_random_section_header` |
| `bs-replay-engine` (record format) | `read_corrupted_trace` |
| `bs-viz-spec` | `decode_random_spec_bytes` |
| `bs-viz-host` (wasm) | covered by wasmtime's own fuzzing — gate at instantiation |

CI: short fuzz on every PR (1 minute per target); long fuzz nightly
(1 hour per target). Crash regressions automatically opened as P0
issues with the input attached.

## Soak / regression

A nightly job runs the soak suite:

- **Self-debug**: BugStalker debugs `bugstalker` debugging
  `target/debug/cargo` (recursive). Asserts: no memory leaks
  measured by valgrind/ASAN, no panic, runs to completion.
- **`ripgrep` watchpoint hunt**: set a watchpoint on a known
  variable in ripgrep, run a search across `linux/` source tree,
  assert watchpoint fires the expected number of times.
- **Perf regression**: time `tests/debugger/variables.rs` end-to-
  end with overlay off. Compare to baseline; alert on >10 %
  regression.
- **Replay determinism soak** (Phase 6): record a 30-minute tokio
  HTTP server workload; replay 100 times; assert identical PC
  trace at every breakpoint each time.

Soak failures are not blocking PR merges but are P1 issues opened
automatically with the failing run's logs attached.

## Per-phase test deliverables

| Phase | Adds | Modifies |
| ------- | ---------------------- | ---------------------- |
| 1 stdlib | `tests/fixtures/rustc-debuginfo/`, ~12 new test functions in `variables.rs` | extend existing assertions for atomics, pin, cow, etc |
| 2 mangle | `crates/rust-mangle-tree/tests/` (corpus + differential + property + fuzz), corpus submodules under `tests/fixtures/submodules/` | none |
| 3 dyn/async | `tests/debugger/dyn_trait.rs`, `tests/debugger/async_await.rs`, `tests/fixtures/debuggees/dyn_trait_chain/`, `tests/fixtures/debuggees/async_await/` | extend `niche` tests in `variables.rs` |
| 4 visualisers | `crates/bs-viz-host/tests/`, `tests/debugger/wasm_visualizers.rs`, derive-macro UI tests via `trybuild` | none |
| 5 perf | `crates/bs-perf/tests/`, `tests/debugger/perf_overlay.rs`, `tests/debugger/perf_overlay_no_slowdown.rs` | none |
| 6 time-travel | `crates/bs-replay-engine/tests/`, `tests/debugger/replay_*.rs`, gVisor + criu + syzkaller submodules, rr CI oracle | none |
| 7 linker | `tests/fixtures/wild-linked/`, section round-trip tests in `crates/bs-debug-sections/tests/`, version-skew tests | extend `tests/debugger/symbol.rs` for accelerated lookup |

## Determinism requirements

Tests must be deterministic. Specifically:

- No `std::time` in assertions — use logical clocks or recorded
  timestamps from fixtures.
- No `HashMap` iteration order in assertions — use `BTreeMap` or
  sort before comparing.
- No process IDs or addresses in golden output — strip or normalise.
- No "first" / "last" stack-frame ordering that depends on
  scheduling — sort by deterministic key (function name).
- Random seeds in property tests are derived from the test name
  (via `proptest`'s `Config::with_cases`).

A failed determinism test on a passing CI run with no code change
is an automatic P0 — it indicates flakiness, which corrodes the
test suite over time.

## Coverage targets

Per crate, after the phase that introduces it lands:

| Crate | Line coverage target |
| ---------------------- | -------------------- |
| `rust-mangle-tree` | 95 % (small surface, high-stakes) |
| `bs-debug-sections` | 90 % |
| `bs-viz-spec`, `bs-viz-host` | 85 % |
| `bs-perf` | 80 % (platform-gated paths inflate denominator) |
| `bs-replay-engine` | 85 % MVP, 90 % at full Tier 3 |
| BugStalker integration paths | maintain or improve current coverage |

Coverage measured via `cargo llvm-cov`; reported in CI; PRs that
drop coverage by >2 % get a maintainer ping (not auto-blocked).

## Release gauntlet

Before tagging a release:

1. Full CI matrix passes (all 36 cells).
2. All differential oracles match (or recorded divergence list
   updated and reviewed).
3. Soak suite ran clean within the last 7 days.
4. Coverage thresholds met.
5. Fuzz corpora unchanged or expanded; no recent crashes.
6. Manual smoke on the three platforms BugStalker supports.

Release notes call out test-coverage changes alongside feature
changes.

## What is *not* tested

Honest gaps:

- We do not test against rustc nightly's bleeding edge — too
  unstable. Pin to nightly-N for known-quirks features (e.g. v0
  default).
- We do not test `--release` debuginfo (most users debug `--debug`).
  Add a smoke test for `-Cdebuginfo=2 --release` per release;
  do not gate every PR on it.
- We do not test cross-compiled debuggees (e.g. ARM debuggee on
  x86 BugStalker). Same-architecture only.
- We do not formally prove anything; this is an empirical
  test suite.

## Effort estimate

Phase 8 itself is mostly carried by other phases — every phase doc
already includes its tests in its effort budget. The cross-cutting
infrastructure (harness crate, fixture layout, submodule plumbing,
CI matrix definition) is:

| Item | Effort |
| -------------------------------------- | -------- |
| `bs-test-harness` crate + migration | 3 weeks |
| `tests/fixtures/` reorganisation | 1 week |
| Submodule additions (per phase, lazy) | 1 week amortised |
| CI matrix expansion (GitHub Actions) | 1 week |
| Differential oracle harness (3 oracles) | 2 weeks |
| Property + fuzz scaffolding | 1 week |
| Soak job + dashboard | 1 week |
| `cargo deny` license gate | 2 days |

Total: ~10 weeks engineer-time, spread across the lifetime of all
other phases. Lay the foundation in parallel with Phase 1; expand
incrementally.

## Risks

- **Submodule rot.** Pinned tags become abandoned; corpora bit-rot.
  Mitigate: pin specific versions; nightly job re-fetches and
  re-runs to detect upstream removal.
- **GPL contamination via test corpora.** Engineers occasionally
  forget. CI gate: `cargo deny` license check; new submodules
  reviewed for license at addition time.
- **Test flakiness from threading.** Strict determinism rules above;
  zero-tolerance for flaky tests in CI.
- **CI cost explosion.** Full matrix is expensive. Curate the
  per-PR subset; keep nightly comprehensive but compute-bounded.
- **Oracle drift.** `rustc-demangle` and `lldb` evolve; pinned
  versions become stale. Maintainer-owned quarterly sweep updates
  pins and reviews recorded divergences.

## Specifications

- Cargo manifest reference — <https://doc.rust-lang.org/cargo/reference/manifest.html>. Workspace member layout.
- Cargo workspace docs — <https://doc.rust-lang.org/cargo/reference/workspaces.html>.
- `cargo test` reference — <https://doc.rust-lang.org/cargo/commands/cargo-test.html>.
- `cargo nextest` book — <https://nexte.st/>. The runner we standardise on (20× faster on Darwin per project memory).
- `proptest` book — <https://altsysrq.github.io/proptest-book/>. Property testing.
- `cargo-fuzz` book — <https://rust-fuzz.github.io/book/cargo-fuzz.html>. Fuzz harness.
- `arbitrary` crate — <https://docs.rs/arbitrary/>. Structured fuzzing inputs.
- `cargo-deny` book — <https://embarkstudios.github.io/cargo-deny/>. License gate.
- `cargo-llvm-cov` — <https://github.com/taiki-e/cargo-llvm-cov>. Coverage measurement.
- GitHub Actions documentation — <https://docs.github.com/en/actions>. CI matrix runner.
- LLVM testing infrastructure guide — <https://llvm.org/docs/TestingGuide.html>. Reference for the LLDB data-formatter test patterns we lift.
- Debug Adapter Protocol specification — <https://microsoft.github.io/debug-adapter-protocol/specification>. For `tests/dap/*` coverage.
- SPDX license identifiers — <https://spdx.org/licenses/>. License field syntax in `Cargo.toml` and `cargo-deny` config.
- Reproducible Builds specification — <https://reproducible-builds.org/specs/>. Soak-test target: same input → same output.
- `trybuild` — <https://docs.rs/trybuild/>. Compile-fail UI tests for the `#[derive(DebugView)]` proc-macro (Phase 4 Tier A).
- `cargo-mutants` — <https://mutants.rs/>. Mutation testing; nightly job that catches under-tested invariants.

## Invariants

These are properties of the test infrastructure itself (not the system under test),
enforced at PR time.

```rust
// Determinism: hash of test output stable across runs of the same test.
debug_assert_eq!(test_output_hash(run1), test_output_hash(run2),
    "non-deterministic test output");

// Submodule pin is a tag, not a branch.
debug_assert!(submodule_pin.is_tag(),
    "submodule {} pinned to branch — must be a tag", submodule_name);

// License gate clean.
debug_assert!(license_check_passed());

// Fixture compiles under MSRV when applicable.
debug_assert!(fixture.compiles_under_msrv());

// Coverage delta within tolerance.
debug_assert!(coverage_delta >= -0.02,
    "coverage dropped by {:.1}%, exceeds 2% threshold", coverage_delta * -100.0);

// Test fixtures are self-contained — no network access during build.
debug_assert!(fixture.network_access_count == 0);

// `bs-test-harness` cleans up debuggee processes on drop.
debug_assert!(self.spawned_pids.is_empty(),
    "harness leaked {} debuggee processes", self.spawned_pids.len());

// CI matrix cells are curated, not full Cartesian product.
debug_assert!(per_pr_matrix_size <= 12);
```

Most of these checks are not literal `debug_assert!` calls in test code — they are
conditions the CI pipeline enforces (license gate, coverage threshold, submodule
check, harness cleanup). The `debug_assert!` syntax above is *aspirational
expression* — wherever a check can sit inline in test-harness code, it does;
wherever it can only sit in a CI step, the corresponding step exists in
`.github/workflows/`.

## Non-goals

- Not 100 % coverage of every edge case. The pyramid is shaped: heavy
  unit + integration; targeted property + fuzz; selective soak.
- Not a compatibility shim for old debugees (rustc < 1.81 — see
  Phase 1 note about `call_debug_fmt` minimum).
- Not user-acceptance testing. Beta releases handle that.
- Not Windows. Out of scope.

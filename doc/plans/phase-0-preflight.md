# Phase 0 — Pre-flight

Infrastructure work that does not fit any feature phase but blocks all of them.
Workspace layout, CI matrix, licensing, logging conventions, and bench scaffolding
must be in place before the first feature PR merges.

## Phase dependency graph

```text
                 Phase 0 (pre-flight)
                       │
       ┌───────────────┼───────────────┐
       │               │               │
   Phase 8          Phase 1        Phase 2
   (testing)        (stdlib)     (rust-mangle-tree)
   (parallel        (parallel       │
    with all)        with 2)        │
                                    ▼
                                 Phase 3
                            (dyn Trait + async)
                                    │
                       ┌────────────┼────────────┐
                       ▼                         ▼
                   Phase 4                   Phase 6
                (visualisers)            (perf overlay)
                       │                         │
                       └────────────┬────────────┘
                                    ▼
                                Phase 7
                          (linker accelerators)
                                    │
                                    ▼
                                Phase 5
                            (time travel)
                            tier 1: needs Phase 6 PT
                            tier 2: independent
                            tier 3: independent
```

Recommended start order:

1. Phase 0 (this doc) ships first; everything below relies on it.
2. Phase 8's `bs-test-harness` extraction and CI matrix start in parallel with Phase 0.
3. Phase 1 and Phase 2 are parallel (independent feature work).
4. Phase 3 must wait for Phase 2 (needs the parsed-AST API).
5. Phases 4, 6 are parallel after Phase 3 lands.
6. Phase 7 needs the spec format from Phase 4 Tier A and the vtable list from Phase 3.
7. Phase 5 Tier 1 needs Phase 6's PT integration; Tiers 2 and 3 are independent.

## Workspace conversion

BugStalker is currently a single crate at the repo root. Phase 1+ adds workspace
members under `crates/`. The conversion is mechanical but must land before any new crate.

- Promote root `Cargo.toml` to a `[workspace]` with `members = [".", "crates/*"]`.
- Add `[workspace.package]` for shared metadata (license, repository, authors, edition).
- Add `[workspace.dependencies]` for shared deps (`gimli`, `object`, `tracing`,
  `proptest`, `arbitrary`, `rustix`).
- Move existing `tests/` to remain at the root crate; new crates get their own `tests/`.
- The `examples/` crate already exists; align with new layout.
- `target/` stays at the workspace root.

Concrete first PR: workspace conversion + one trivial new crate
(e.g. `crates/bs-test-harness/` empty stub) to validate the layout.

## `bs-test-harness` extraction

`tests/debugger/variables.rs` (2 947 lines) hard-codes the
launch-debuggee → set-breakpoint → render → assert flow. Phase 8 promises to extract
this into a `bs-test-harness` crate. Migrate incrementally — extract the harness,
port one test, port the rest over time. Do not big-bang rewrite.

## CI matrix expansion

The Phase 8 doc lists the matrix axes (OS, Rust channel, linker, mangling,
PT availability, cargo features). Phase 0 work is the concrete
`.github/workflows/*.yml` to instantiate it.

- `ci.yml` — per-PR curated subset (10–12 cells); fast feedback.
- `ci-nightly.yml` — full matrix (~36 cells), runs nightly on a schedule.
- `ci-fuzz.yml` — fuzz harness on schedule. Phase 0 ships the
  workflow shape; **registered fuzz targets are deferred to Phase 8**
  (`bs-test-harness`, `rust-mangle-tree`, `bs-replay-engine`). Until a
  target lands the job exits cleanly so the schedule stays green.
- `ci-soak.yml` — soak suite on schedule.
- `ci-bench.yml` — bench suite (see Cargo bench placement below).

Each workflow needs Linux x86, Linux aarch64 (qemu), and Darwin aarch64 runners.
The 16-core x86 NixOS box (`192.168.1.137`, alias `x86`) is the canonical
Linux-with-PT host.

## Earthfile, Makefile, Nix flake

Existing files at repo root:

- `Earthfile` — extend with `+test`, `+bench`, `+lint`, `+fuzz` targets so CI
  cells are reproducible locally.
- `Makefile` — point developer entry points at `cargo nextest`
  (per project memory: 20× faster on Darwin).
- `flake.nix` / `flake.lock` — already present; nightly Rust + qemu +
  libipt-rs build deps are **deferred to Phase 6** (when the
  `intel-pt` feature is first exercised); Phase 0 leaves the flake
  untouched.

## License and SPDX policy

BugStalker is MIT (LICENSE file at root, "Copyright (c) 2026 Derevtsov Konstantin").

- Every new file in this project gets a single-line SPDX header:
  `// SPDX-License-Identifier: MIT`
  (or `# SPDX-License-Identifier: MIT` for shell/yaml).
- New workspace crates publish to crates.io as `MIT OR Apache-2.0` (dual-license,
  broader downstream adoption). Confirm with project owner before publishing.
- `cargo deny check licenses` runs in CI on every PR; a `deny.toml` lists allowed
  licenses (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`,
  `Unicode-DFS-2016`, `Apache-2.0 WITH LLVM-exception`, `MPL-2.0`).
- GPL-licensed dependencies in `dev-dependencies` are explicitly flagged in
  `deny.toml` advisories — they may exist for differential testing only
  (e.g. `rr` as a CI binary oracle).

## Logging convention

> **Phase 0 status:** `doc/logging.md` ships in Phase 0; the convention
> is enforced for **new** workspace crates. Migration of the existing
> root `bugstalker` crate from `log` + `env_logger` to `tracing` is
> **deferred** to the first phase that meaningfully edits the affected
> modules (likely Phase 1 or Phase 3).

All BugStalker crates use `tracing` with `target` = crate name. Convention:

- `tracing::debug!` — development noise.
- `tracing::info!` — user-relevant single-line events
  ("attached to PID 12345", "loaded symbol table from /path").
- `tracing::warn!` — visible degradation
  ("vtable resolution falling back to slow path: linker accelerators absent").
- `tracing::error!` — things that prevent forward progress.

`RUST_LOG` filter format: `bugstalker=info,bs_perf=debug,rust_mangle_tree=warn`.
Each new workspace crate registers under its own name.

No `eprintln!`/`println!` from library code — only the binary entry point and
CLI handlers may print.

## `cargo bench` placement

> **Phase 0 status:** workspace-level `benches/render_value.rs` and
> `benches/attach_cold.rs` ship as criterion placeholders so
> `cargo bench` produces a baseline today. Per-crate benches
> (`crates/rust-mangle-tree/benches/parse.rs`,
> `crates/bs-perf/benches/decode.rs`,
> `crates/bs-replay-engine/benches/record_replay.rs`) are **deferred**
> to the phase that adds the corresponding crate (Phase 2 / Phase 6 /
> Phase 5 respectively).

Each crate that has a hot path adds a `benches/` directory using `criterion`
(dev-dep):

- `crates/rust-mangle-tree/benches/parse.rs` — parse throughput on a
  representative corpus.
- `crates/bs-perf/benches/decode.rs` — PC→(file,line) resolution rate.
- `crates/bs-replay-engine/benches/record_replay.rs` — record overhead,
  replay throughput.
- `benches/render_value.rs` — render N varied stdlib values.
- `benches/attach_cold.rs` — attach to a 100 MB binary, time to first
  usable variable.

`ci-bench.yml` runs nightly on `x86`, archives criterion's `target/criterion/`
reports, and posts a PR comment if any benchmark regresses by >10 % vs the
trailing 7-day median.

This is in lieu of a formal performance regression budget doc — the bench suite
is the budget, the CI alert is the enforcement.

## Definition of done (applied to every phase)

**Each phase must be production-ready before the next begins.** No
parallel-stream feature development. The criteria below apply uniformly
to phases 1–8; Phase 0 itself ships when its `## Acceptance criteria`
(below) are met.

A phase is production-ready when:

- [ ] All `## Acceptance criteria` listed in the phase doc pass.
- [ ] All `## Invariants` from the phase doc are encoded as
      `debug_assert!` at the relevant call sites, and have at least one
      test that would fail if the assert were removed and the invariant
      violated.
- [ ] DAP integration ships per the phase's `## DAP integration`
      subsection — all listed standard extensions and `bs/*` custom
      requests implemented, with at least one DAP-client integration
      test under `tests/dap/`.
- [ ] Test coverage targets met (per-crate thresholds in
      `phase-8-testing.md`).
- [ ] `cargo fuzz` ran on each new crate that takes external input
      and produced no new crashes for at least 1 hour on `x86`.
- [ ] Soak suite ran clean within the last 7 days against the phase's
      deliverables.
- [ ] `cargo bench` baselines committed for any new hot path; no
      pre-existing benchmark regressed by >10 %.
- [ ] User-facing docs updated: `CHANGELOG.md`, `README.md` if
      surface-relevant, `website/` if commands or features changed.
- [ ] `cargo deny check` passes (licenses + advisories).
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
      clean.
- [ ] No known P0/P1 bugs against the phase's surface.
- [ ] Released to crates.io (for publishable workspace members) and
      tagged on the BugStalker repo with a release-notes entry.
- [ ] Post-release dust settles: at least one week of real-world use
      with no regression reports before the next phase begins.

The "post-release dust settles" item is non-negotiable. Shipping a
phase, then immediately starting the next, is how integration debt
compounds. The week of quiet exposes anything the test suite missed.

### Implication for the dependency graph

The graph above shows *logical* dependencies, not concurrent work
streams. A phase higher in the graph cannot ship before a phase below
it that it depends on, but the production-ready rule tightens this:
phases ship one at a time even when the graph permits parallelism.
"Phase 1 and Phase 2 are parallel" in the graph means *either could go
first* — not *both run concurrently*.

The single exception is Phase 8 infrastructure (test harness, CI
matrix, fuzz scaffolding): this lays down in parallel with Phase 1
because it has no user-visible surface. It is the *plumbing* that
lets every other phase ship production-ready, so it predates the
sequential-shipping rule.

## Effort estimate

| Item                                 | Effort  |
| ------------------------------------ | ------- |
| Workspace conversion                 | 2 days  |
| `bs-test-harness` stub + first port  | 3 days  |
| CI matrix workflows                  | 1 week  |
| Earthfile/Makefile/flake updates     | 2 days  |
| `cargo deny` + license headers       | 2 days  |
| Logging convention rollout           | 1 day   |
| Bench scaffolding (per-crate stubs)  | 2 days  |

Total: ~3 weeks engineer-time. All before any feature phase begins.

## Acceptance criteria

- `cargo build` and `cargo nextest run` work at the workspace level.
- CI matrix runs to completion on at least one Linux and one Darwin runner.
- `cargo deny check` passes.
- Every existing source file has an SPDX header.
- `cargo bench` produces reports for every listed benchmark, even if just
  a placeholder.

## Non-goals

- Not a feature phase. Phase 0 ships only infrastructure.
- No user-visible behaviour changes.
- Not a hard prerequisite for all of Phase 1 — small features can land without
  the full Phase 0 in place — but the workspace conversion specifically must
  precede any new crate.

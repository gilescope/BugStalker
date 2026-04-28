# Rust Debugging Manifesto

> *"Everything is connected, the death of one beetle in Berlin can affect the trajectory of a satellite over Mars."*
> — Dirk Gently, ish

A program at runtime is described by four independent and complementary
sources of truth. A debugger that ignores any of them is leaving information
on the table.

| Source | What it tells you |
| ------------ | ------------------ |
| **DWARF** | Layout — sizes, offsets, niches, line tables, register location |
| **v0 mangling** | Identity — full type of every monomorphisation, vtable, closure, shim |
| **The debuggee itself** | Intent — its own `Debug::fmt`, its own iterators, its own state |
| **The linker** | Aggregation — vtable index, monomorphisation table, coroutine map, build-id |

Mainstream Rust debuggers (lldb-with-Python, gdb-with-Python) lean almost
entirely on the first. BugStalker already exploits the third in a way nobody
else does, via `call_debug_fmt` (`src/debugger/call/fmt.rs:407`). The work
ahead is to wire in the second, expose all three to crate authors through
a sandboxed extension surface, and co-design the fourth with the Wild
linker (Linux/Mac/wasm).

This document is the umbrella. Per-phase implementation plans live under
`doc/plans/`.

---

## Current state, honestly

Inventory of `src/debugger/variable/value/parser.rs:411–691` plus the
`RenderValue` trait at `src/debugger/variable/render.rs:58`, cross-checked
against rustc's `src/etc/{lldb,gdb}_providers.py`.

| Type | BugStalker | rustc Python printers |
| ----------------------------------------- | ---------- | --------------------- |
| `String`, `&str`, `Vec`, `VecDeque` | yes | yes |
| `HashMap`/`HashSet` (hashbrown) | yes (custom `HashmapReflection`) | yes, fragile |
| `BTreeMap`/`BTreeSet` | yes | yes |
| `Rc`/`Arc`/`Weak` | yes | yes |
| `Cell`/`RefCell` | yes | yes |
| `uuid::Uuid`, `SystemTime`, `Instant` | yes | no |
| TLS variables (multi-version, Darwin) | yes | partial |
| Calling user `Debug::fmt` in debuggee | yes | no |
| `Mutex`/`RwLock`/`MutexGuard` | raw | raw |
| Atomics (`AtomicI32`, ...) | raw `UnsafeCell<T>` | raw |
| `Duration`/`Range`/`Pin`/`Cow` | raw | raw |
| `PathBuf`/`OsString`/`CString`/`MaybeUninit`/`NonNull` | raw | raw |
| `Box<T>` smart unwrap | no | no |
| async state machines / futures | no | no |
| tokio runtime introspection | no | no |
| `dyn Trait` concrete-type recovery | no | no |
| User-extensible visualisers | no | yes (Python) |
| RFC 3191 `#[debugger_visualizer]` | no | partial (gdb) |

### Things BugStalker is already ahead on

- **`call_debug_fmt`** runs the user's own `core::fmt::Debug::fmt` inside
  the debuggee via mmapped `Formatter` + ptrace/Mach. Tracks three
  rustc `Formatter` layouts (1.81 → 1.87+). Strictly more powerful than
  the Python printers — it gets `anyhow::Error`, `eyre::Report`, every
  third-party `impl Debug` for free.
- **TUI / console split with a unified `RenderValue` / `ValueLayout`**
  abstraction (`render.rs`, `ui/generic/variable.rs`,
  `ui/tui/components/variables.rs`) — both surfaces consume the same
  layout enum.
- **Niche-encoded enums** are decoded via `DW_TAG_variant_part`/
  `DW_AT_discr` rather than the legacy `RUST$ENCODED$ENUM$` name hack
  the Python printers still carry.

### Latent bugs and obvious holes

- `DW_TAG_reference_type` falls through unhandled at
  `src/debugger/debugee/dwarf/type.rs:635`. rustc happens to emit
  `DW_TAG_pointer_type` for `&T` most of the time so this rarely bites,
  but it is a correctness hole.
- No specialisations for `Mutex`, `RwLock`, atomics, `Duration`,
  `Range*`, `Pin`, `Cow`, `Box`, `MaybeUninit`, `NonNull`, `CStr`,
  `CString`, `OsStr`, `OsString`, `PathBuf`, `Path`.
- Hard `LEN_GUARD = 10_000` / `CAP_GUARD = 10_000` at
  `src/debugger/variable/specialization/mod.rs:40` silently truncate
  large collections.
- `char` validation workaround in `parser.rs:187` (`WAITFORFIX` for
  rust-lang/rust#113819) silently replaces invalid `char` with `'?'`.
- No tests for `Mutex`, `RwLock`, `MaybeUninit`, `PathBuf`, `OsString`,
  `CString`, `Range`, `Pin`, `Cow`, `NonNull`, async, tokio.

---

## The four pillars

### Pillar 1 — `rust-mangle-tree`

A new workspace crate that parses Rust v0 mangled symbols (RFC 2603) into
a borrowed AST. `rustc-demangle` only exposes `Display`; for vtable-driven
type recovery and async frame attribution we need the parsed tree.

- Crate path: `crates/rust-mangle-tree/`
- `no_std + alloc`, single-pass, zero-copy AST
- Public surface: `parse(s: &str) -> Result<Symbol<'_>, ParseError>`
  with structured access to crate name, path segments, generic args,
  impl self-type, impl trait, closure coords, drop-glue/shim flags.
- Both `_R…` (v0) and `_ZN…` (legacy) under one `Symbol` enum — mixed
  binaries are the norm on stable.
- Differential testing vs `rustc-demangle` `Display` output.
- Fuzz harness from commit one — adversarial symbols never panic.

Detailed plan: `doc/plans/phase-2-rust-mangle-tree.md`.

### Pillar 2 — Wasm visualisers

Crate authors should be able to ship a pretty-printer with their crate.
Python is the wrong answer (full host process access, autoload of
`.debug_gdb_scripts` from a malicious binary is a known vector).
Wasmtime + the component model gives capability-typed sandboxing.

- WIT-defined `debuggee` capability interface (read memory, deref symbol,
  type-of, field-offset).
- Discovery via two channels: `~/.config/bugstalker/visualizers/*.wasm`
  for ad-hoc, and an extension/cohabitation of RFC 3191
  `#[debugger_visualizer]` for crate-shipped visualisers.
- Visualisers expose lazy iterators for large collections — no eager
  materialisation of 10k-element `Vec`s.
- Behind a cargo feature `viz-wasm`. Default builds carry the hardcoded
  specialisations only.

Detailed plan: `doc/plans/phase-4-wasm-visualizers.md`.

### Pillar 3 — Linker accelerators

A linker has whole-program visibility. It already builds the indices a
debugger has to rebuild from scratch at attach time. With Wild as the
co-designed linker, BugStalker reads sorted accelerator sections in
O(log n) rather than scanning all symbols.

Architectural rule: **accelerator sections are additive, never
required.** BugStalker works with `lld`, `mold`, `gold`, and any other
linker. Wild gives faster startup and richer features; other linkers
fall back to existing scan paths.

Sections under design (joint `bs-debug-sections` crate, versioned):

- `.bs_buildid` — content hash for cache invalidation
- `.bs_viz_index` — sorted index over `.bs_viz_spec` (Pillar 2)
- `.bs_vtables` — sorted `(vtable_addr → impl_self_type, trait)`
  (Pillar 1 fast-path)
- `.bs_monos` — generic monomorphisation index
- `.bs_coroutines` — async state-machine variant table with
  source coords
- `.bs_paths` — path-prefix table for source files

Detailed plan: `doc/plans/phase-7-linker-contract.md`.

### Pillar 4 — Performance overlay and time travel

When stopped at a breakpoint, show per-line cost since the last stop.
The architectural rule that makes this possible without slowing
"play to next breakpoint" is:

> **The debuggee runs naked. The CPU's PMU writes to a kernel ring.
> Decoding waits for the stop.**

No software instrumentation in the debuggee, no single-step counting,
no inline trace decoding.

- Linux: `perf_event_open` cycles+IP sampling for the universal tier;
  Intel PT for exact per-line cycle counts when available.
- macOS: kperf private APIs (the `samply` pattern). No PT-equivalent.
- New crate: `crates/bs-perf/` wrapping the platform back-ends.
- TUI source view gets a heat-map gutter; console gets a margin column.
- Per-stop summary: "this run cost X cycles, Y wall-time, Z % on line N".

The same Intel PT trace buffer is the substrate for **reverse step**
and time-travel debugging. Three tiers, all in scope, all planned
upfront:

1. PT-window reverse step (free with Phase 5; ~2 weeks UX work).
2. `fork(2)` checkpoint replay (Linux + Mach equivalent on Darwin;
   ~4 weeks).
3. **A first-party clean-room MIT/Apache record-and-replay engine**
   — no rr/Pernosco GPL contamination, no rr-wrapper compromise.
   Nine sub-phases, ~12 months single-engineer to full coverage,
   each sub-phase independently shippable starting at month 5
   (single-threaded Linux MVP).

We commit to building the engine ourselves. rr is used only as a CI
test oracle for differential validation; engineers do not read its
source.

Detailed plans: `doc/plans/phase-5-perf-overlay.md` and
`doc/plans/phase-6-time-travel.md`.

---

## Roadmap, tier-ordered

### Tier 1 — parity (close the obvious gaps)

Plugs the holes mainstream printers also leave. No new architecture
required; specialisations bolted into the existing
`parse_inner_with_modifiers` chain.

- Fix `DW_TAG_reference_type` fall-through.
- `Mutex`/`RwLock` show inner `T` directly + lock-state badge.
- Atomics render as the bare scalar.
- `Duration` → human, `SystemTime` → ISO-8601, `Instant` → relative.
- `Range`/`RangeInclusive` → `a..b` / `a..=b`.
- `Pin<P>` → transparent deref of `P`.
- `Cow<'_, T>` → `Borrowed(...)` / `Owned(...)`.
- `Box<T>` → smart-unwrap deref.
- `MaybeUninit<T>`, `NonNull<T>`, `CString`/`CStr`/`OsString`/`OsStr`/
  `PathBuf`/`Path` — string-decode where bytes are valid.
- `Vec<u8>` / `&[u8]` — utf-8 probe with hex-dump fallback.
- Watch-expression format specifiers: `:x`, `:b`, `:p`, `:[N]`, `:y`,
  `:s`, `:c`.

Detailed plan: `doc/plans/phase-1-stdlib-coverage.md`.

### Tier 2 — surpass (the v0 dividend)

Things no mainstream Rust debugger does today. All depend on
Pillar 1 (`rust-mangle-tree`).

- **Vtable-driven `dyn Trait` resolution.** Read vtable pointer,
  resolve symbol at that address, demangle, extract `<Concrete as Trait>`,
  fetch DWARF for `Concrete`, render recursively. Headline feature.
- **Niche-resilient `Option`/`Result`.** Apply Rust's known niche rules
  (`Option<NonNull<T>>` → null check, etc.) bypassing the DWARF
  discriminant ambiguity that bites the Python printers
  (rust-lang/rust#62839).
- **`Rc`/`Arc` cycle detection.** Visited-set walk with depth cap;
  back-edges marked `[cycle to 0x…]`.
- **Async await-trace.** Walk generator/coroutine state-machine,
  decode active variant via discriminant, read `DW_AT_decl_file`/
  `DW_AT_decl_line` of variant members for source coords.
  Combined with vtable recovery for `Pin<Box<dyn Future>>` this
  produces an await-trace next to the stack-trace.

Detailed plan: `doc/plans/phase-3-dyn-trait-and-async.md`.

### Tier 3 — extensibility

Pillar 2 lands. After this point, every gap is a wasm module away.

### Cross-cutting — testing

Test strategy across all phases — six-layer pyramid (unit /
integration / property / differential / fuzz / soak), license-vetted
external corpora (rustc debuginfo tests, gimli, gVisor syscall
suite, samply, libipt), submodule layout under `tests/fixtures/`,
differential oracles (`rustc-demangle`, `lldb`, `rr`-as-binary),
CI matrix, determinism requirements.

`rr` is a *binary* CI oracle only — never linked, never read.
`gVisor`'s syscall corpus (Apache-2.0) anchors Phase 6 syscall
coverage. `cargo deny` enforces the license gate so GPL never
sneaks in via dev-dependencies.

Detailed plan: `doc/plans/phase-8-testing.md`.

### Tier 4 — frontier

- **Time-travel debugging.** PT-window reverse step (cheap),
  `fork(2)` checkpoint replay (moderate), and a first-party clean-room
  record-and-replay engine (the year-scale commitment). Tier 1 ships
  in week 2; clean-room single-threaded Linux MVP at month 5;
  multi-threaded at month 7; full cross-platform at month 10. See
  `doc/plans/phase-6-time-travel.md` for the nine sub-phases.
- tokio runtime introspection (task tree, parked vs runnable).
  Locate `OwnedTasks` / `LocalSet` by symbol; walk task list; decode
  each future via vtable. Stable across tokio minor versions if keyed
  off well-named symbols.
- Type-state visualisation for builder patterns: surface
  `<Builder as State<Connected>>` as `state: Connected` rather than a
  `PhantomData` field.
- TUI graph view for cyclic data structures.
- Profile-driven collection bounds replacing the hard `LEN_GUARD`.
- `DebuggerView` trait support if/when rust-lang/rust#65564 lands.

---

## Architectural rules

1. **Debuggee runs naked.** No software instrumentation in the
   debuggee for any feature. Hardware sampling and symbol-table reads
   only. `call_debug_fmt` is the single sanctioned exception and runs
   only when the user explicitly invokes `vard`/`argd`.
2. **Visualisers run sandboxed.** Wasm only, capability-typed access
   to debuggee memory through a WIT-defined interface. No host I/O.
3. **Demangler never panics.** Adversarial mangled symbols return
   `Err(ParseError)`. Fuzzed continuously.
4. **Niche enums use Rust knowledge first.** Apply the language's known
   niche rules directly; treat DWARF discriminant info as confirmation,
   not authority.
5. **Per-platform features gated explicitly.** `intel-pt` is a Linux-x86
   cargo feature; kperf is macOS-only; perf-overlay graceful-degrades
   to "unavailable" on platforms without a back-end. Tests skip with a
   message, never falsely pass.
6. **Bounds visible to the user.** When BugStalker truncates a
   collection, paginates samples, or caps recursion, it says so in the
   output.
7. **DWARF correctness bugs are ours to dodge.** rust-lang/rust#125147
   (negative discriminant `DW_FORM_data*`), #62839 (niche enum
   ambiguity), #134479 (v0 ICE with `generic_const_exprs`) — work
   around them, file upstream, do not block on fixes.
8. **Invariants are encoded as `debug_assert!`.** Every cross-component
   invariant we believe to hold gets an explicit assert at the call
   site. Release builds compile them out so there is zero runtime cost.
   The asserts pay for themselves the first time someone ports
   BugStalker to a new platform, upgrades a kernel, or refactors a
   parser — they catch the divergence at its source rather than as a
   confusing failure three layers downstream. Per-phase invariants are
   listed in the corresponding `doc/plans/phase-N-*.md` under
   `## Invariants`.
9. **Pure Rust by default; C dependencies are explicit, narrow, and
   gated.** BugStalker's runtime is pure Rust to the largest extent
   possible. Where a system interface (Linux syscalls, Mach ports,
   Apple `kperf`) requires FFI, we prefer `rustix` over `nix` so the
   syscall wrapper itself is pure Rust without `libc`. Where a domain
   library has no mature pure-Rust replacement, we gate it behind an
   optional cargo feature so the default build is pure Rust:
   - Intel `libipt` (PT decoder) — behind `intel-pt`. Default
     `bs-perf` build does cycles sampling only and is pure Rust.
     Long-term: write a pure-Rust PT decoder from Intel SDM Vol 3
     Ch 36 as a separate `crates/bs-pt-decoder/` project.
   This is the *only* sanctioned C dependency. Everything else,
   including replay-trace compression (`ruzstd`,
   <https://github.com/KillingSpark/zstd-rs>) and `wasmtime`, is
   pure Rust.
   We do not depend on unstable Rust features for any runtime path.
   Unstable items (`core::ptr::DynMetadata`, the `Coroutine` trait,
   `DebuggerView` if it lands) appear only as documentation
   references for layouts we reverse-engineer via DWARF + raw-memory
   inspection — not as imported APIs. dev-tooling (`cargo-fuzz`'s
   libfuzzer, valgrind) is not bound by this rule; only artefacts
   linked into BugStalker's release binary.
10. **Errors aim for rustc-level kindness.** Every user-visible error
    message identifies (a) what failed, (b) where — file path, byte
    offset, symbol name, or other locator, (c) what was expected vs.
    what was found, and (d) where possible, how to fix it. No silent
    fallbacks; degradation is visible. The `tracing::warn!` channel
    is for "we recovered but the user should know"; `tracing::error!`
    is for "we did not recover." Per-crate `Error` types implement
    `std::error::Error` with full source-chain support and have
    `Display` impls that read like a rustc diagnostic. The
    debugger's job is to make hard problems easier; opaque errors
    do the opposite.
11. **Configuration is layered, explicit, and discoverable.** All
    tunable values follow one model: built-in default → config file
    (`~/.config/bugstalker/config.toml`) → environment variable
    (`BS_*` namespace, double underscore for nesting) → CLI flag.
    Every knob is listed in a single config schema with type,
    default, and one-sentence description. No crate invents its own
    `MY_CRATE_FOO=` env var. The `bugstalker config show` subcommand
    prints the resolved configuration with the layer each value came
    from. Tunables proposed across the phases (`LEN_GUARD`,
    `MAX_RENDER_DEPTH`, `BS_PERF_DRAIN_HZ`, sample frequency,
    visualiser fuel/memory caps, max checkpoints, format-spec
    defaults) all live under this scheme.
12. **DAP integration is a feature commitment, not a bonus.** Every
    user-visible feature ships with a Debug Adapter Protocol
    exposure or it does not ship. Standard DAP requests
    (`stepBack`, `variables`, `evaluate`) are extended where they
    fit; custom requests under the `bs/*` namespace are added where
    they don't. Each phase doc carries a `## DAP integration`
    subsection enumerating the protocol additions. The justification
    is reach: if a feature works only in BugStalker's TUI/console,
    VSCode and IDE users do not see it, which permanently caps the
    feature's adoption.
13. **Each phase ships production-ready before the next starts.**
    No parallel-stream feature development. The dependency graph in
    `doc/plans/phase-0-preflight.md` shows *logical* dependencies,
    not concurrent work streams; the recommended start order is
    sequential. Phase N's "Definition of done" checklist (Phase 0)
    must be satisfied — released, documented, DAP-integrated, tested,
    fuzzed, soak-tested, no known P0/P1 bugs, post-release dust
    settled — before Phase N+1 begins. The reason is honest: parallel
    feature streams in a debugger compound integration debt; one
    polished phase is more valuable to users than two half-complete
    ones. The single exception is Phase 8 infrastructure (test
    harness, CI matrix, fuzz scaffolding), which has no user-visible
    surface and lays down in parallel with Phase 1 to support every
    subsequent phase.

---

## Non-goals

- C/C++ pretty-printing. Out of scope. Use lldb if you need it.
- Cross-language debugging. Out of scope.
- Always-on production observability. BugStalker is interactive.
  `tokio-console` already exists for the always-on case; we are not
  competing with it.
- A hosted Python interpreter. Ever.

Time-travel debugging (reverse step, record-and-replay) is *in* scope
and planned upfront — see `doc/plans/phase-6-time-travel.md`. Three
tiers there: free reverse-step over the Intel PT window (combines with
Phase 5); checkpoint-based replay using `fork(2)`; and a first-party
clean-room MIT/Apache record-and-replay engine (no rr/Pernosco GPL
contamination, no rr-wrapper compromise — built by us, ~12 months to
full coverage, sub-phases shippable from month 5).

---

## Open questions

- **RFC 3191 cohabitation strategy.** Extend the upstream
  `#[debugger_visualizer]` attribute to recognise `wasm_file`, or
  define a parallel `bs_visualizer_wasm` section? The latter ships
  faster; the former is the right long-term answer.
- **Apple Silicon perf parity.** kperf is undocumented and changes
  across macOS releases. How aggressive should we be about chasing
  parity vs accepting a feature-degraded experience on Mac?
- **`async fn` vs closure namespace ambiguity (rust-lang/rust#104830).**
  v0 uses the same `C` tag for both. Detection has to fall back to
  DWARF generator-shape patterns. How robust can we make this without
  a compiler change?
- **Vtable cache invalidation across `dlopen`.** BugStalker has dyld
  rendezvous wired (recent darwin work). Cached vtable→type entries
  must be invalidated on shared-library load/unload events. With
  Pillar 3's `.bs_vtables` section, invalidation extends to per-DSO
  section maps.
- **MSRV for `rust-mangle-tree`.** Keep low (1.70?) to make adoption
  by `samply`/`addr2line`/etc plausible.

---

## References

### v0 mangling

- RFC 2603 — <https://rust-lang.github.io/rfcs/2603-rust-symbol-name-mangling-v0.html>
- v0 reference — <https://doc.rust-lang.org/rustc/symbol-mangling/v0.html>
- v0 nightly default — <https://blog.rust-lang.org/2025/11/20/switching-to-v0-mangling-on-nightly/>
- `rustc-demangle` — <https://docs.rs/rustc-demangle>
- `rust-demangle.c` — <https://github.com/LykenSol/rust-demangle.c>
- purplesyringa nutshell — <https://purplesyringa.moe/blog/rusts-v0-mangling-scheme-in-a-nutshell/>

### Rust pretty-printer state of the art

- rustc Python printers — `src/etc/{lldb,gdb}_providers.py` + `rust_types.py` in rust-lang/rust
- CodeLLDB — <https://github.com/vadimcn/codelldb>
- Cliff Biffle, lildb — <https://cliffle.com/blog/lildb/>
- Cliff Biffle, async decl coords — <https://cliffle.com/blog/async-decl-coords/>

### RFCs and tracking issues

- RFC 3191 `#[debugger_visualizer]` — <https://rust-lang.github.io/rfcs/3191-debugger-visualizer.html>
- rust-lang/rust#62839 — niche enum DWARF ambiguity
- rust-lang/rust#125147 — `DW_AT_discr_value` form bug
- rust-lang/rust#73524 — async backtraces
- rust-lang/rust#65564 — `DebuggerView` trait proposal
- rust-lang/rust#1563 — `dyn Trait` debug repr (open since 2012)
- rust-lang/rust#104830 — async/closure v0 namespace ambiguity
- rust-lang/rust#134479 — v0 ICE with `generic_const_exprs`

### Performance and tracing

- `perf_event_open(2)` — <https://man7.org/linux/man-pages/man2/perf_event_open.2.html>
- Intel PT — <https://github.com/intel/libipt>
- `samply` — <https://github.com/mstange/samply>
- `tokio-console` — <https://github.com/tokio-rs/console>

---

## Specifications index

Each phase doc carries its own `## Specifications` section with the
standards, RFCs, manpages, and architecture references that phase
depends on. This is the cross-cutting subset — standards every phase
touches at least somewhere — and a pointer to the per-phase lists.

### Cross-cutting standards

- **DWARF Debugging Information Format Version 5** —
  <https://dwarfstd.org/doc/DWARF5.pdf>. Read by every phase that
  touches type info, line tables, variants, or split debug.
- **The Rust Reference, type layout** —
  <https://doc.rust-lang.org/reference/type-layout.html>. Authoritative
  for layout-level decisions.
- **The Rustonomicon** — <https://doc.rust-lang.org/nomicon/>. Pin,
  exotic sizes, transmutation rules, vtable mention.
- **rustc dev guide** — <https://rustc-dev-guide.rust-lang.org/>.
  Behavioural source of truth where the formal spec is silent.
- **ELF System V gABI** —
  <https://refspecs.linuxfoundation.org/elf/gabi4+/contents.html>.
  Section semantics on Linux.
- **Mach-O `loader.h`** —
  <https://github.com/apple-oss-distributions/dyld/blob/main/include/mach-o/loader.h>.
  Section semantics on Darwin.
- **WebAssembly Core Specification** —
  <https://webassembly.github.io/spec/core/>. Module and custom-section
  format.

### Per-phase specs

| Phase | Doc | Notable specs |
| ----- | ----------------------------------- | ----------------------------------- |
| 1 | `doc/plans/phase-1-stdlib-coverage.md` | DWARF 5 §5.7.10/§6.2; Rust Reference type layout; IEEE 754-2019; rust-lang/rust#62839, #113819, #125147 |
| 2 | `doc/plans/phase-2-rust-mangle-tree.md` | RFC 2603; Itanium C++ ABI; RFC 3492 (Punycode); rustc Book v0 reference |
| 3 | `doc/plans/phase-3-dyn-trait-and-async.md` | DWARF 5 variant_part; rustc dev guide async/await; Cliff Biffle decl-coords; rust-lang/rust#62839, #73524, #1563, #104830 |
| 4 | `doc/plans/phase-4-wasm-visualizers.md` | RFC 3191; Wasm Component Model; WIT; Wasm Custom Sections; ELF/Mach-O section refs |
| 5 | `doc/plans/phase-5-perf-overlay.md` | `perf_event_open(2)`; Intel SDM Vol 3 Ch 36 (PT); libipt; ARM SPE/BRBE; AMD IBS PPR; DWARF .debug_line |
| 6 | `doc/plans/phase-6-time-travel.md` | `seccomp_unotify(2)`; `ptrace(2)`; `userfaultfd(2)`; `prctl(2)`; `vdso(7)`; io_uring whitepaper; Intel SDM Vol 1; zstd RFC 8478; mozilla/rr paper |
| 7 | `doc/plans/phase-7-linker-contract.md` | ELF gABI; Mach-O loader.h; Wasm Custom Sections; BLAKE3 spec; FxHash; `gimli-rs/object` |
| 8 | `doc/plans/phase-8-testing.md` | Cargo manifest; `cargo nextest`; `proptest`; `cargo-fuzz`; `cargo-deny`; SPDX; DAP spec |

---

## Dependency policy index

Quick table of every external crate this project plans to take a
runtime dependency on, sourced from the per-phase docs. C/C++
indicates we link C code via FFI (gated behind a cargo feature);
all other rows are pure Rust.

| Crate | Used by | Pure Rust? | Notes |
| ---- | ------- | ---------- | ----- |
| `gimli` | Phases 1, 3, 5, 7 | yes | DWARF parsing |
| `object` | Phase 7 | yes | ELF/Mach-O/wasm reading |
| `wasmtime` | Phase 4 (Tier B) | yes | Wasm component runtime |
| `bumpalo` | Phase 2 | yes | arena allocation |
| `rustc-hash` (FxHash) | Phases 4, 7 | yes | non-cryptographic hashing |
| `blake3` | Phase 7 | yes | build-id hashing |
| `rustix` | Phases 5, 6 | yes | preferred over `nix` for syscalls |
| `nix` | Phases 5, 6 (where rustix lacks coverage) | yes (FFI to libc only) | fallback |
| `iced-x86` | Phase 5 | yes | x86 disassembler, replaces `libxed` |
| `ruzstd` | Phase 6 | yes | pure-Rust zstd encoder + decoder; KillingSpark's `zstd-rs`. Used at `CompressionLevel::Fastest` for trace recording |
| `libipt` (`libipt-rs`) | Phase 5 | **no** — C | PT decode; gated behind `intel-pt` cargo feature; default build excludes; pure-Rust replacement planned (`bs-pt-decoder`) |
| `proptest` | all phases | yes (dev only) | property tests |
| `arbitrary` | all phases | yes (dev only) | structured fuzz inputs |
| `cargo-fuzz` (libFuzzer) | all phases | **no** — C++ (dev only) | dev tooling, not linked into BugStalker |
| `cargo-deny` | Phase 8 | yes (dev only) | license + advisory gate |
| `proc-macro2`, `syn`, `quote` | Phase 4 derive | yes (build-time) | proc-macro expansion |

Default `cargo build` produces a 100 % pure-Rust binary. The single
opt-in feature that introduces C linkage is `intel-pt` (PT decode via
`libipt`); a pure-Rust replacement (`bs-pt-decoder`) is planned, and
when it lands the feature flag becomes pure-Rust without any external-
interface change.

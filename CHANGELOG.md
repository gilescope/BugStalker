<!-- markdownlint-disable MD001 MD022 MD025 MD032 MD012 -->
# Changelog

All notable changes to this project will be documented in this file.

# [?.?.?] Unreleased

### Added

- visualisers (Phase 4 Tier-A step 12 — `bs/visualiserList` DAP):
  - New `bs/visualiserList` custom DAP request enumerates every
    Tier-A spec the loader recovered from the debuggee. Response
    body is `{ "visualisers": [{ "typeName", "summary", "origin",
    "fields": [...], "variants": [{ "name", "summary", "tag",
    "fields": [...] }] }] }` with full attribute fidelity. An IDE
    settings panel can render the list without first having to
    hit a value of any of those types.
  - `Format::as_wire_str()` returns a stable lower-case wire
    string (`"hex"`, `"iso8601"`, …) shared by the JSON response
    and the attribute syntax users write.
  - `origin` is always `"tier-a"` today; will distinguish
    `"tier-b"` (wasm) and `"built-in"` (stdlib specialisations)
    when those land.
  - Integration test
    `tests/dap/dap_stdio.rs::test_stdio_dap_visualiser_list`
    drives the full DAP wire end-to-end against `viz_demo` —
    asserts 10 visualisers, `Person.flags = format "hex"`,
    `Person._private_token` hidden, `Status::Connected` tag
    `"ok"`, `qualified::Marker` registered under the explicit
    name override.
- darwin: auto-recover DWARF for `split-debuginfo = "unpacked"`:
  - The DWARF loader (`src/debugger/debugee/dwarf/mod.rs`)
    previously had four lookup paths: `.dSYM` bundle (macOS),
    `.note.gnu.build-id` (Linux), `.gnu_debuglink` (Linux), and
    inline DWARF in the executable. None of them cover Rust's
    *default* macOS layout, where rustc emits DWARF into per-CU
    `.o` files referenced via Mach-O `N_OSO` stabs and Cargo
    doesn't run `dsymutil` afterwards. End users running
    `bs --dap` against a fresh `cargo build` saw every
    breakpoint go UNVERIFIED with no clue why.
  - New `Self::ensure_dsym_fresh(obj_path, file)` runs at the
    start of `build()` on macOS. When the executable has no
    `__debug_info` section *and* no fresh `.dSYM` bundle (or
    the bundle is older than the binary), it spawns `dsymutil
    <obj_path>` to walk the OSO stabs itself and consolidate
    the `.o` DWARF into the bundle. The existing dSYM-lookup
    path then picks it up.
  - Failures (binary not writable, dsymutil missing, etc.)
    surface via `log::warn` with a clear next-step message and
    fall through to "no debug info" rather than a silent
    UNVERIFIED.
  - New regression test
    `tests/debugger/viz.rs::debug_view_loader_recovers_dsym_for_split_debuginfo`:
    nukes the `.dSYM` bundle, attaches BugStalker against a
    raw `cargo build` binary (with the test-harness's
    pre-emptive `ensure_dsym_fresh` opted out via the new
    `BS_TEST_NO_AUTODSYM=1` env var), sets a BP, runs.
    Without the loader fix the BP would be UNVERIFIED.
  - Full debugger + dap suites: 173/173 sequentially.
- visualisers (Phase 4 Tier-A step 11 — utf8 + hexdump byte formats):
  - `format = "utf8"` and `format = "hexdump"` now apply to
    byte-array-shaped fields. Recognised shapes: `Vec<u8>` /
    `VecDeque<u8>` (via the `SpecializedValue::Vector` variant
    BugStalker normalises to), `String` / `&str` (already-
    decoded UTF-8 in their specialised variants), and plain
    `[u8; N]` / `&[u8]`-shaped `Value::Array`. Output routes
    through the existing `render_bytes` helper so shape matches
    other byte renderings in the engine: `b"hello world"` for
    utf8, the standard 16-bytes-per-row hex grid (with ASCII
    column when printable) for hexdump.
  - New `format_bytes` private helper (with public DAP alias
    `format_bytes_for_dap`). The structure-render branch and
    the template-substitute closure both try `format_scalar`
    first, fall through to `format_bytes`, then default.
    DAP path mirrors via the alias.
  - `viz_demo` gains a `Doc { body: Vec<u8>, raw: Vec<u8>,
    size: u32 }` struct with `format = "utf8"` on `body` and
    `format = "hexdump"` on `raw`. Integration test asserts
    `b"hello world"` and `00 ff 42 53 56 31` both appear in
    the with-spec render.
- visualisers (Phase 4 Tier-A step 10 — `module_path!()` integrated):
  - `bs-viz-spec` exports two new `const fn` entry points:
    `assemble_with_module_path::<N>(module, local, suffix)` and
    `assemble_verbatim::<N>(name, suffix)`. Both produce a
    `[u8; N]` byte-for-byte identical to the runtime `encode`
    output (proven by two new roundtrip unit tests).
  - `bs-viz-derive` no longer emits the spec as a literal byte
    string filled in at proc-macro time. Instead it emits a
    `static [u8; TOTAL]` initialised by the const fn at the
    *user crate's* compile time, with `module_path!()` and the
    local stringified ident plugged in. Net effect: the recorded
    `type_name` is now the fully-qualified
    `module_path!()`-prefixed form (e.g. `viz_demo::Person`)
    that the v0 demangler also produces, so exact-match lookup
    Just Works without a suffix-match fallback in the common
    case.
  - `bs-viz-sdk` exposes the const fns through a hidden
    `__internal::` module so the derive's generated code has a
    stable path to call them. Not part of the public SDK
    contract.
  - Registry's `find` gained reverse-suffix matching
    (`key.ends_with(query)` with `::` separator) so existing
    tests and debug-CLI lookups passing the local-only name
    keep resolving cleanly.
  - `viz_demo::Person` etc. now register under their full
    module-prefixed names; integration test asserts the new
    shape and that both qualified and local-only probes hit
    the same spec.
  - Full debugger + dap suites: 172/172 sequentially.
- visualisers (Phase 4 Tier-A step 9 — `name = "..."` override):
  - New `#[bs_viz(name = "fully::qualified::Path")]` type-level
    attribute. When set, overrides the recorded `type_name` in
    the spec — useful for disambiguating two crates that each
    derive `DebugView` on a type with the same local name and
    consequently hit the registry's suffix-match ambiguity bail.
    Default behaviour (no `name` attr) is unchanged.
  - `bs-viz-sdk` README rewritten to document the full attribute
    surface — type / field / variant levels — in one place.
  - `viz_demo` gains `Marker(u32)` with
    `#[bs_viz(name = "qualified::Marker")]`. Test asserts
    `view_spec_for("qualified::Marker")` resolves exactly while
    `view_spec_for("Marker")` misses, proving the override
    replaces the key cleanly.
  - True `module_path!()` integration (so the common case
    Just Works without an explicit override) is still tracked
    under "Remaining" §1 — needs a const-fn that assembles
    bytes from a runtime-known prefix.
- visualisers (Phase 4 Tier-A step 8 — per-variant enum attrs):
  - **Wire format v2.** `bs-viz-spec` bumps `VERSION` to 2 and
    appends a `num_variants + variant[]` block to each entry's
    payload. Each `VariantSpec` carries `name`, optional
    `summary`, optional `tag`, and a per-variant `Vec<FieldSpec>`.
    Pre-v2 readers reject the entry cleanly via
    `UnsupportedVersion`.
  - **`bs-viz-derive` parses variant attrs.** `#[bs_viz(summary
    = "...", tag = "...")]` on an enum variant lights up.
    Per-field overrides on variant fields use the same parser
    as struct fields.
  - **Renderer dispatches variant-first.** Both the TUI/console
    path (`ui::generic::variable`) and the DAP path
    (`data::render_value_to_string_with_viz`) prefer the
    variant-level summary over the type-level one, apply
    variant-scoped field overrides for placeholder substitution,
    and prepend a leading `[tag]` chip to the rendered enum
    (with one space before it) when the active variant carries
    a `tag = "..."` attribute.
  - `viz_demo`'s `Status` enum gets variant attributes:
    `Connected(u32)` renders as `Status [ok] ✓ Connected (port
    443)`, `Error(&str)` as `Status [err] ✗ Error: transport
    reset`. Integration test asserts both forms via the TUI
    path and the DAP `read_locals` path.
  - Full debugger + dap suites: 172/172 sequentially.
- visualisers (Phase 4 Tier-A step 7 — DAP per-callsite wiring):
  - `value_children` now takes `viz: Option<&VizRegistry>` and
    propagates it through every recursive descent. Free
    functions `read_locals(dbg)` and `read_args(dbg)` pull
    `dbg.view_registry()` themselves. The two `impl DebugSession`
    callers in `setExpression` / `evaluate` thread the registry
    via `dbg.view_registry()`. Every `render_value_to_string`
    callsite in `data.rs` migrates to the
    `..._with_viz` form.
  - Net user impact: a VS Code variables panel now shows the
    derived summary string instead of the placeholder `{...}`
    for any `#[derive(DebugView)]` type. The existing
    `render_value_to_string` shim is preserved (wraps with
    `viz: None`) so untouched callers keep their no-viz
    rendering — `control.rs::value_truthy` for instance is
    deliberately left on that path since it's a comparison
    predicate, not a UI render.
  - `tests/debugger/viz.rs::debug_view_summary_applied_at_render_time`
    now also asserts the DAP path via `data::read_locals`: `p`
    is `Person(Ada, age 36)`, `status_ok` is `Status[443]`,
    `w_i32` is `Wrap[17]`. Three different shapes (struct,
    enum, generic struct) all flow through the IDE wire
    correctly.
  - Full debugger + dap suites: 172/172 sequentially.
- visualisers (Phase 4 Tier-A step 6 — enums):
  - `#[derive(DebugView)]` now accepts enums. Step 6 only carries
    the type-level `summary` template; per-variant attributes
    (`#[bs_viz(summary = "...")]` / `#[bs_viz(tag = "...")]`
    on individual variants) are tracked under "Remaining" §2.
  - Renderer dispatches on the active variant: when a
    `RustEnum` hits the `Wrapped` branch and its enum-type has a
    registered spec, the summary template substitutes against
    the *active variant's* struct members. Placeholder `{__0}`
    resolves to the first positional field of whichever variant
    is currently active — `Status::Connected(443)` renders as
    `Status[443]`, `Status::Error("transport reset")` as
    `Status[transport reset]`. Same dispatch in the DAP path.
    No-spec / no-summary fallback is unchanged
    (`EnumType::VariantName(...)`).
  - `viz_demo` gains a `Status { Connected(u32), Disconnected,
    Error(&'static str) }` enum and two locals exercising
    different active variants; integration test asserts both
    render through the template and that the bare path is
    unchanged. Spec count is now 8.
- visualisers (Phase 4 Tier-A step 5 — tuple + unit structs):
  - `#[derive(DebugView)]` now accepts tuple structs and unit
    structs. Tuple-struct fields receive the Rust-DWARF
    convention name `__0`, `__1`, ..., so summary-template
    placeholders + per-field attribute lookup resolve cleanly.
    Newtype-style `pub struct UserId(#[bs_viz(format = "hex")]
    pub u64);` is the canonical use case. Unit structs serialise
    with an empty field list; only their type-level `summary`
    applies.
  - **Bug fix.** Summary-template substitution previously
    ignored per-field format overrides — `summary = "UserId#{__0}"`
    on a tuple struct whose `__0` carried `format = "hex"`
    substituted the raw decimal form. The substitution closure
    now consults the spec's per-field format alongside rename /
    hidden flags. Fix applies to both the TUI/console path
    (`ui::generic::variable::substitute_template`) and the DAP
    path (`dap::yadap::session::data::render_value_to_string_with_viz`).
    `format_scalar_for_dap` is the new public alias the DAP
    closure uses.
  - `viz_demo` gains `UserId(u64)`, `Point(i32, i32)`, and
    `Sentinel` (unit struct). Integration test asserts:
    7 specs recovered, tuple-struct field names round-trip as
    `__0` / `__1`, unit struct round-trips with empty fields,
    `uid` renders as `UserId#0xcafebabe` (was the bug), `pt`
    renders as `Point(10, 20)`.
- visualisers (Phase 4 Tier-A step 4 — iso8601 + duration formats):
  - `#[bs_viz(format = "iso8601")]` on integer scalars renders
    the value as a UTC ISO-8601 timestamp (Unix epoch seconds
    convention) — `1_705_322_096` → `2024-01-15T12:34:56Z`.
    Powered by `chrono::DateTime::from_timestamp`; values
    outside chrono's representable range fall back to default
    rendering.
  - `#[bs_viz(format = "duration")]` on integer scalars renders
    the value as a human-readable duration in nanoseconds:
    `5_000_000` → `5.000ms`, `1_500_000_000` → `1.5s`,
    `2_000_000_000` → `2s` (trailing-zero trim on the seconds
    branch). Negative values fall through to default rendering
    since `Duration` is unsigned.
  - `viz_demo` gains an `Event` struct exercising both formats
    inside the summary template. `tests/debugger/viz.rs` asserts
    `2024-01-15T12:34:56Z` and `5.000ms` appear in the with-spec
    render and *don't* appear in the bare render.
  - 5 new unit tests in `ui::generic::variable::format_tests`
    cover the conversion functions: ISO-8601 fixed point + Unix
    epoch + pre-epoch negative; duration unit boundaries;
    negative-duration fallback.
  - `format = "utf8"` and `format = "hexdump"` are still
    fall-through (need byte-array detection); tracked under
    "Remaining" §4.
- visualisers (Phase 4 Tier-A step 3 — generics + format=hex):
  - `#[derive(DebugView)]` now accepts generic types. The macro
    emits one spec per type *definition* (not per
    monomorphisation); the registry's `find()` strips trailing
    `<…>` (bracket-depth-aware) before searching, so
    `Wrap<i32>`, `Wrap<Vec<u8>>`, and nested
    `HashMap<K, Vec<i32>>` all resolve to the single `Wrap` /
    `HashMap` entry.
  - `#[bs_viz(format = "hex|bin|oct")]` now actually formats the
    rendered scalar (`0xff00ff` instead of `u32(16711935)`). The
    `iso8601` / `duration` / `utf8` / `hexdump` family are
    decoded into the registry but still fall through to default
    rendering pending type-specific decoders.
  - `examples/viz_demo` gains a generic `Wrap<T>` derive; the
    integration test `debug_view_summary_applied_at_render_time`
    now asserts both `w_i32: Wrap<i32>` and `w_str: Wrap<&str>`
    render through the same single `Wrap[{inner}]` template, and
    `flags: 0xff00ff` proves the format tag is honoured.
  - 4 new unit tests in `src/debugger/viz/mod.rs` for
    `strip_generic_args` and a `find_with_generics` lookup
    integration test.
- visualisers (Phase 4 Tier-A step 2 — renderer integration):
  - New `Debugger::view_registry()` borrow exposed to render-
    layer callers.
  - New `crate::debugger::viz::substitute_template` shared
    helper. `{field_name}` placeholders, `{{` / `}}` escape pair,
    `{?name}` for unknown placeholders so a typo doesn't silently
    swallow itself.
  - New `ui::generic::variable::{render_value_with_viz,
    render_variable_with_viz}` (TUI/console path) and
    `dap::yadap::session::data::render_value_to_string_with_viz`
    (DAP path). All three honour the type-level `summary`
    template, the field-level `skip` (hide field), and the
    field-level `rename = "..."`. The `format = "hex|bin|...""`
    overrides are decoded into the registry but not yet applied
    at render time — tracked under "Remaining" in ROADMAP §4.
  - Pre-existing `render_value` / `render_value_to_string` are
    one-line shims passing `None`, so call sites that haven't
    been updated keep their current rendering exactly.
  - Integration test
    `tests/debugger/viz.rs::debug_view_summary_applied_at_render_time`
    attaches BugStalker against `viz_demo`, reads `p` as a local
    after construction, and verifies all three render effects:
    summary applied, hidden field absent, renamed field uses the
    new label. Also asserts the `viz=None` path is unchanged.
- visualisers (Phase 4 Tier-A step 1 — declarative `#[derive(DebugView)]`):
  - New workspace crates: `bs-viz-spec` (wire format, MAGIC `BSV1`,
    length-prefixed entries, encode/decode), `bs-viz-derive`
    (proc-macro reading type- and field-level `#[bs_viz(...)]`
    attributes and emitting a `static [u8; N]` into `.bs_viz_spec` /
    `__DATA,__bs_viz_spec`), and `bs-viz-sdk` (façade re-export
    `pub use bs_viz_derive::DebugView; pub use bs_viz_spec::*;`).
  - Step-1 attribute surface: type-level `summary = "..."`;
    field-level `skip`, `rename = "..."`, `format = "hex|bin|oct|
    iso8601|duration|utf8|hexdump"`. Generics, enums, tuple
    structs, and the `custom` escape hatch produce a clear
    compile-error pointing at the tracking phase.
  - BugStalker side: `src/debugger/viz/` reads both section names
    at attach, decodes, and indexes by type name. Lookup falls
    back to suffix matching against `::<name>` so the renderer
    can pass fully-qualified demangled names while the macro
    stores the local-module short form (until `module_path!()`
    integration in step 2). Ambiguous suffix matches bail to
    `None` rather than mis-applying.
  - New `Debugger::view_spec_for(type_name)` and
    `Debugger::view_spec_count()` API; `bs_viz_spec` re-exported
    from `bugstalker::bs_viz_spec` for downstream embedders.
  - Reference debuggee `examples/viz_demo/` with two derives;
    integration test
    `tests/debugger/viz.rs::debug_view_specs_loaded_from_demo_binary`
    asserts both specs are recovered with full attribute
    fidelity. Renderer integration (applying summaries / per-
    field overrides at print time) is the next batch.
- variables (Phase 3 Feature D batch D1 — async `.await` source coords):
  - On a coroutine state-machine enum (`async fn` body), rustc emits
    `DW_AT_decl_file` / `DW_AT_decl_line` on the captured-locals fields
    of each `Suspend` variant; those coords are the source position of
    the `.await` the future is paused at (the Cliff Biffle technique).
    BugStalker now reads them.
  - New plumbing:
    - `Die::decl_file_line()` exposes the attribute pair on any DIE.
    - `StructureMember::decl_file_line: Option<(u64, u64)>` stores it
      for every member at parse time.
    - `RustEnumValue::await_location: Option<(PathBuf, u64)>` is filled
      at parse time when the active variant's inner struct (or the
      enumerator member itself) carries the attributes; the unit's
      file table resolves `(file_idx, line) → PathBuf`.
    - `AsyncFnFuture::await_location` carries the coords through to
      the renderer.
  - User-visible: `async backtrace` now appends a `at FILE:LINE`
    suffix to the "suspended at await point N" line whenever source
    coords are recoverable. Stripped binaries and pre-`.await` states fall back
    cleanly to the bare "await point N" form.
  - Test: `tests/debugger/tokio.rs::test_async_await_location_recovered`
    asserts every `Suspend(_)` `AsyncFn` in the ticker app reports
    `examples/tokiotiker/src/main.rs:5` — the only `.await` in the
    example.
  - The remaining open items on Phase 3D — step 5 (multi-level
    awaitee-chain walker beyond the existing `__awaitee` follow)
    and the deeper half of step 6 (re-reading the awaitee at the
    recovered concrete TypeId so the chain *continues through* the
    dyn box) — are tracked in
    `doc/plans/phase-3-dyn-trait-and-async.md`.
- async (Phase 3 Feature D step 1 — explicit coroutine type-name detection):
  - New `looks_like_coroutine_type_name(name: &str) -> bool` helper
    in `src/debugger/debugee/dwarf/type.rs` recognising all three
    rustc-emitted markers across the language's history:
    `{async_fn_env#N}`, `{coroutine_env#N}`, `{generator_env#N}`.
  - New `RustEnumValue::is_coroutine()` method (cheap, no extra
    storage) for renderers and tooling that want the type-level
    signal. Deliberately additive to — not a replacement for —
    `AsyncFnFuture::try_from`'s variant-state-name parse, which
    remains the correctness source-of-truth so a future rustc rename
    of the synthesised type can't silently drop matches.
  - 9 unit tests across two `#[cfg(test)]` modules cover every
    emitted pattern, ordinary enums, closure envs, and the
    partial-substring rejection edge.
  - **This closes Phase 3 (`dyn Trait` + niche-resilient enums +
    Rc/Arc cycles + async await-trace) substantively.**
- async (Phase 3 Feature D `poll_fn`-closure walker):
  - tokio's `select!` and `join!` macros wrap captured branch
    futures inside `poll_fn(|cx| {...})` whose closure environment
    carries the futures as captured locals. Step 5's
    variant-only scan stopped at the `PollFn` wrapper and missed
    them; this batch replaces the immediate scan with a recursive
    `collect_coroutine_seeds(struct, depth)` helper.
  - The helper skips the canonical `__awaitee` field, only admits
    `RustEnumValue`s that pass `AsyncFnFuture::try_from` (so
    non-coroutine enums captured as locals don't pollute the
    branch list), and is depth-bounded
    (`COROUTINE_SCAN_DEPTH = 6`).
  - Used in two spots: the active-variant scan (Step 5's entry
    point) now recurses; the `__awaitee = Value::Struct(...)` leaf
    branch *also* runs the scan on the awaitee struct itself, so a
    `Future::Multi` is appended after the `Custom`/dyn-box-recover
    frames when 2+ branches are found nested inside.
  - 4 new unit tests in `tokio/task.rs::tests` cover the helper:
    `__awaitee` skip, non-coroutine-enum filter, PollFn-style
    descent, depth cap.
- async (Phase 3 Feature D step 6 deeper — concrete-TypeId re-read):
  - When the awaitee is a `Pin<Box<dyn Future>>` (or sibling), the
    await-trace now walks *through* the dyn box into the concrete
    future's state machine instead of stopping at the bare
    `[→ Concrete]` annotation D2b lifted.
  - New `DynFutureLocator` + `locate_dyn_future` walker in
    `src/debugger/async/future.rs`: descends (depth-bounded) for the
    canonical fat-pointer shape, parses `Concrete` out of the
    annotation, and reads the data-pointer slot.
  - `tokio/task.rs::recover_concrete_future` looks up the concrete
    type across every loaded `DebugInformation` via
    `find_type_die_ref`, issues a `Dqe::DataCast` to read the
    pointee, and recurses into `build_chain_from_repr` when the
    result is a `RustEnumValue`.
  - `Task::backtrace` now takes `&Debugger`; three call sites
    updated (mod.rs OwnedList loop, tokio/park.rs, tokio/worker.rs).
    The `Option<&Debugger>` plumbed into the walker means callers
    without a handle still get the linear chain.
  - All failure modes degrade cleanly to the existing `Future::Custom`
    annotation: type not found, null pointer slot, parse failure,
    or non-coroutine pointee.
- async (Phase 3 Feature D step 5 — multi-branch awaitee walker):
  - New `Future::Multi(Vec<Vec<Future>>)` variant: each entry in
    `branches` is the chain rooted at one of the parallel futures
    captured by an active variant. Triggered when the variant's
    struct carries 2+ `Value::RustEnum` fields outside the
    canonical `__awaitee` — the canonical caller is
    `tokio::join!` / `tokio::select!`, but detection is generic.
  - `tokio/task.rs::future_stack` refactored to call a recursive
    `build_chain_from_repr` helper, depth-bounded
    (`MAX_BRANCH_DEPTH = 8`) so pathological nesting bails out
    cleanly with a leaf `UnknownFuture`.
  - Console renderers (`print_future` / `print_await_trace`) render
    each branch as a numbered sub-trace indented one level.
  - DAP `bs/awaitTrace` adds a new frame kind:
    `{ "kind": "multi", "branches": [[frame...], ...] }`.
    Recursion goes through the same `serialize_await_frame` helper
    so all frame kinds compose uniformly.
  - The D3b `tokio_select` / `tokio_join` tests now accept *either*
    a direct `branch_*` AsyncFn frame *or* one surfaced through a
    `Future::Multi`, since tokio's macros wrap branches in
    `poll_fn` closures whose visibility varies by rustc version.
- tests (Phase 3 Feature D batch D3b — dedicated async-await suite):
  - 5 new example debuggees under `examples/`:
    `tokio_simple_await`, `tokio_chained_await`, `tokio_select`,
    `tokio_join`, `tokio_dyn_future`. Each exercises one specific
    `.await` shape called out in the plan.
  - 5 matching tests in `tests/debugger/async_await.rs`. Each
    breaks at a `marker()` call right after the relevant `.await`
    and asserts the expected fn names appear in the futures stack
    when the root AsyncFn is `Suspend(_)`. Assertions are
    deliberately lenient about running-vs-suspended state since the
    tokio runtime picks which task to poll non-deterministically.
  - The new test module is `#[cfg(target_os = "linux")]`-gated;
    tokio runtime introspection isn't operational on Darwin (same
    limitation as the existing `tokio.rs` suite). Linux CI is the
    source of truth.
- dap (Phase 3 Feature D batch D3a — `bs/awaitTrace` custom request):
  - New BugStalker-specific JSON-RPC method exposing the same
    await-trace data the console renders, so VSCode (and any future
    scripting front-end — see Phase 9 plan) can render it in a
    parallel panel without scraping text output.
  - Optional `threadId` argument selects a specific worker / blocking
    thread; absent ⇒ the currently-focused task.
  - Response body: `{ taskId, frames: [...] }` where each frame is
    one of `kind: "asyncFn" | "sleep" | "joinHandle" | "custom" |
    "unknown"` carrying the relevant per-kind fields (state /
    awaitPoint / source.path + line for asyncFn; deadlineSec/Nsec
    for sleep; waitingForTaskId for joinHandle; concrete for
    custom). `source.path` runs through the existing
    `SourceMap::map_target_to_client` so it lines up with
    `stackTrace` paths.
- ui (Phase 3 Feature D batch D2a — `async await-trace` / `async at`):
  - New console subcommand that prints the current task's awaitee
    chain as a stack-frame list, source-coords-first. Mirrors how
    `bt` reads for synchronous frames so the user can transfer
    their mental model directly. Renders one frame per element of
    the futures stack:

    ```text
    await-trace (task id: 7):
      #0 my_app::handler at src/handler.rs:42 (await point 3)
      #1 tokio::time::Sleep (deadline 1715000000s.000000000)
    ```

  - Plumbing: `r#async::Command::AwaitTrace`,
    `AsyncCommandResult::AwaitTrace`, `print_await_trace` in
    `src/ui/generic/async.rs`, parser tokens
    `ASYNC_COMMAND_AWAIT_TRACE_SUBCOMMAND` (`await-trace`) and
    `ASYNC_COMMAND_AWAIT_TRACE_SUBCOMMAND_SHORT` (`at`),
    completion-hint entry, `HELP_ASYNC_AWAIT_TRACE_SUBCOMMAND` help
    page, parser test.
- async (Phase 3 Feature D batch D2b — `dyn Future` awaitee concrete
  type lift):
  - When the awaitee is `Pin<Box<dyn Future>>`, `Box<dyn Future>`,
    `&dyn Future`, etc., Phase 3A's vtable resolver has already
    spliced the recovered `[→ Concrete]` annotation into the inner
    fat-pointer struct's `type_ident`. D2b walks through the
    wrapping layers (depth-bounded) to find that annotation and
    surfaces it on `CustomFuture::concrete: Option<String>`.
  - Both `async backtrace` and `async await-trace` now append a
    `[→ Concrete]` suffix to `Custom` future frames whenever the
    concrete differs from the outer type — no double-print when
    the awaitee itself is the dyn fat pointer.
  - Walker uses the canonical fat-pointer member shape (`pointer` +
    `vtable`) rather than `is_trait_object()`'s by-name match,
    because the latter trips on outer wrappers like `Pin<Box<...>>`
    and stops the walk before reaching the inner annotated struct.
  - 4 unit tests in `src/debugger/async/future.rs::tests` cover the
    walker (top-level / nested-in-Pin / plain struct / depth-cap).
- variables (Phase 3 Feature C — Rc/Arc cycle detection):
  - `Rc<T>` / `Arc<T>` now eagerly deref so `var some_rc_node`
    surfaces the inner allocation inline (data, fields, recursive
    members) instead of just an address. Cyclic data structures
    (`Rc<RefCell<Node { peer: Rc<RefCell<Node>> }>>`) terminate
    gracefully with a `[cycle to 0x…]` marker on the second visit
    instead of stack-overflowing the renderer.
  - Two bounded guards layered at parse time:
    1. **Visited-set** keyed on inner allocation address. First
       visit derefs and recurses; second visit splices a
       `[cycle to 0x…]` marker into the value's `type_ident` and
       skips the deref. Catches every cyclic graph regardless of
       depth.
    2. **Recursion-depth cap** (`MAX_RENDER_DEPTH = 4`) for the
       pathological-but-acyclic case (a 100-deep linked list).
       Splices a `[depth limit 4]` marker. The cap is empirically
       stack-safe on a 2 MiB test thread; ratchets up via the
       eventual `bs/setRenderBudget` DAP request once Phase 1 F3's
       budget API generalises.
  - `ParseContext` carries the visited set
    (`RefCell<HashSet<usize>>`) and the depth counter
    (`Cell<u32>`). Each top-level value parse gets a fresh pair so
    cycles don't leak between locals.
  - New helper `eager_deref_with_cycle_check` lives next to the
    other Rc/Arc machinery in
    `src/debugger/variable/value/specialization/mod.rs`. Returns
    `Some(marker)` on bail, `None` on successful deref; callers
    splice the marker into whichever `TypeIdentity` the renderer
    actually reads.
  - Test: new `tests/debugger/variables.rs::test_rc_cycle_detection`
    breaks at `phase3_rc_cycle`'s nop, asserts:
    - the 2-node Rc cycle renders with a `[cycle to 0x…]` marker
      and doesn't stack-overflow;
    - the 100-deep acyclic chain renders with a `[depth limit 4]`
      marker;
    - `lonely_rc` (single-strong-count) still renders inline.
  - All Phase 1 + Phase 3 A/B integration tests still pass.
- variables (Phase 3 Feature B — niche-resilient Option / Result):
  - 16 fixtures + integration test
    (`tests/debugger/variables.rs::test_niche_option_recovery`) cover
    every niche pattern the plan calls out:
    `Option<&T>`, `Option<Box<T>>`, `Option<NonNull<T>>`,
    `Option<NonZero*>`, `Option<bool>`, `Option<fn(...)>` plus
    `Result<&T, ()>` and `Result<NonZero*, ()>` (one-ZST-arm
    Results). All resolve to the correct `Some`/`None` /
    `Ok`/`Err` variant.
  - Discovery: modern rustc emits a usable
    `DW_TAG_variant_part` with an artificial `u64`-typed
    discriminant member and explicit `DW_AT_discr_value` for the
    `None` arm, even for niche-encoded layouts. The existing
    `parse_rust_enum` path already reads it correctly via
    `enumerators.get(&Some(value)).or_else(||
     enumerators.get(&None))`. The plan's worry about
    "rust-lang/rust#62839 leaving consumers to guess" turned out
    to be partially out of date for our supported rustc range —
    no new niche-detection code was needed.
  - The integration test stays as a permanent regression guard:
    if a future rustc churn re-introduces ambiguity in the
    DWARF emit, this test catches it on every PR and the
    follow-up patch (apply the language rules directly per the
    plan) will land here.
- variables (Phase 3 Feature A batch A2 — `dyn Trait` concrete-type
  recovery):
  - The `dyn Trait` annotation from batch A1 now carries the
    recovered concrete type when one can be resolved:
    `alloc::boxed::Box<dyn core::error::Error, alloc::alloc::Global>
    [→ vars::phase3_dyn_trait::MyError] { data: 0x…, vtable: 0x… }`.
    The "impossible" — recovering a hidden type behind a trait
    object — is now possible.
  - **Strategy 2 (primary):** the vtable address is looked up in the
    binary's symbol table. If a `<Concrete as Trait>::{vtable}`
    symbol sits there (rustc emits these on linux / ELF), it's
    demangled with `rust-mangle-tree` and `impl_self_type()` walked
    to render the concrete type. Two-line implementation thanks to
    Phase 2's parser.
  - **Strategy 1 (fallback):** when no symbol sits at the vtable
    address (Mach-O ad-hoc builds, stripped binaries), the resolver
    reads the first 16 vtable slots and probes each as a function
    pointer. Slot 0 is `core::ptr::drop_in_place::<Concrete>` (or
    null when the concrete type has no `Drop`); slots 3+ are the
    trait's method pointers (`<Concrete as Trait>::method`). The
    same string-surgery used by strategy 2 extracts `Concrete`
    from any of them. This is what catches our darwin test
    fixture today — `MyError` is `Copy` so its drop slot is null,
    but the `<MyError as Display>::fmt` method pointer is real.
  - Plumbing: `SymbolTab` now keeps an `address → mangled-name`
    reverse index (alongside the existing demangled-name map).
    Exposed as `DebugInformation::mangled_symbol_at(addr)`.
    `ExpressionEvaluator::debugee()` exposes the `Debugee` so the
    parser-side resolver can chase the address through the right
    DWARF unit. `TypeIdentity::set_name` lets the resolver splice
    the recovered name into the rendered identity.
  - Test: `tests/debugger/variables.rs::test_dyn_trait_detection`
    now demands resolution actually fired — `boxed_err` must
    contain `[→ MyError]`. Test passes on darwin/aarch64.
- variables (Phase 3 Feature A batch A1 — `dyn Trait` detection):
  - `TypeDeclaration::Structure` carries a new `is_trait_object: bool`
    set during DWARF parsing via the `looks_like_trait_object`
    heuristic (struct name contains a `dyn` token OR the canonical
    `pointer`/`vtable` member shape rustc emits).
  - `StructValue::is_trait_object()` is the read-side equivalent;
    the renderer calls it to detect trait-object structs without
    extra plumbing through every constructor.
  - When detected, the renderer emits a single-line summary
    `<trait-name> { data: 0x…, vtable: 0x… }
    [concrete type unavailable; vtable resolution pending — Phase
    3A follow-up]` so the user knows we recognised the trait
    object even though we can't yet recover the concrete type.
  - New integration test `tests/debugger/variables.rs::
    test_dyn_trait_detection` breaks at `phase3_dyn_trait()` and
    asserts a `Box<dyn Error>` renders with the dyn / vtable /
    pending markers. `examples/vars/src/vars.rs` gains a
    `phase3_dyn_trait()` fixture with `Box<dyn Error>`,
    `&dyn Iterator<Item = u32>`, and `Arc<dyn Debug + Send + Sync>`.
  - The `Arc<dyn …>` and `&dyn …` shapes route through the smart-
    pointer / reference-deref render paths and need a follow-up
    batch to surface their detection — the current batch covers
    only the direct `Box<dyn …>` case, which is the headline
    fat-pointer struct.
  - Concrete type recovery itself (vtable address → drop-fn
    symbol → demangle → impl_self_type → TypeId resolution) is
    the next batch.
- crate (Phase 2 — rust-mangle-tree, batches A–H):
  - new workspace crate `crates/rust-mangle-tree` parsing Rust v0
    (RFC 2603) and legacy Itanium-style mangled symbols into a
    borrowed AST. `no_std + alloc`, zero runtime deps, MSRV 1.70,
    dual `MIT OR Apache-2.0`. Public API: `parse(s) -> Result<
    Symbol<'_>, ParseError>` plus the AST types `Symbol`, `Path`
    (with `CrateRoot` / `InherentImpl` / `TraitImpl` / `TraitAssoc`
    / `Nested` / `Generic` arms), `LegacyPath`, `Type`,
    `GenericArg`, `Const`, `FnSig`, `DynBound`, `AssocBinding`,
    `ClosureCoords`, `Lifetime`, `Mutability`, `Primitive`.
  - **Legacy parser** byte-for-byte against `rustc_demangle::
    demangle` over a 12 416-symbol corpus pulled from `nm` over
    the release `bs` binary and `ripgrep`. Two real-world fixes
    along the way: leading-`_` placeholder stripping for
    synthetic `_<…>` segments, and trailing LLVM thunk-decoration
    (`.<n>`, `.llvm.<n>`) preservation. Dropped the legacy
    `.` → `-` mapping (modern rustc-demangle preserves `.`).
  - **V0 parser** single-pass with back-reference offset table,
    256-deep recursion limit, RFC 3492 modified Punycode decoder.
    `Path` enum mirrors v0 productions; `Type` handles all the
    type productions (`R` / `Q` / `P` / `O` / `A` / `S` / `T` /
    `F` / `D` plus the primitive table); `Const` handles numeric
    (with `n`-prefix for negative), `b` bool, `c` char, `e` str,
    `p` placeholder, `B` back-ref.
  - **Fuzz**: 5 proptest cases (~10k random inputs per
    `cargo test` run) enforce the no-panic invariant.
    `crates/rust-mangle-tree/fuzz/` carries the cargo-fuzz scaffold
    for the nightly libFuzzer soak; Phase 8's `ci-fuzz.yml` will
    automate the run.
  - **BugStalker integration** at the two call sites the plan
    calls out: bulk demangle in `symbol::SymbolTab::new` and the
    `NamespaceHierarchy::from_mangled` rewrite. `rustc-demangle`
    is no longer a runtime dependency of the root crate (kept
    as a dev-dep on `crates/rust-mangle-tree` for differential
    testing only).
- benches (Phase 1 batch T — pulled forward from Phase 8):
  - `benches/render_value.rs` — real workload. Spawns the
    `examples/vars` debuggee through `bs-test-harness`, captures
    every Phase 1 local at the `phase1_specs_b()` breakpoint, drops
    the debugger, then times `bugstalker::ui::generic::variable::
    render_value()` over the captured `Value` set per criterion
    iteration. Median ≈3 µs on darwin/aarch64 release.
  - `benches/attach_cold.rs` — real workload. Each iteration
    spawns the `examples/hello_world` debuggee, installs a
    `bugstalker::Debugger`, sets a breakpoint, runs to the hit,
    drops. Median ≈760 ms on darwin/aarch64 release. Sample size
    reduced to 20 so wall-clock stays inside criterion's `--quick`
    budget.
  - `crates/bs-test-harness` grew the `spawn_at_breakpoint` /
    `capture_locals` / `capture_named` helpers used by both
    benches (and reusable from any future bench in the workspace).
  - `+smoke` Earthly target now greps criterion output for
    `Performance has regressed` and fails CI on a stat-significant
    slowdown. Criterion's `target/criterion/` baseline persists
    across Earthly cache mounts so the comparison is meaningful.
    First-run is a no-op (no baseline to compare against).
- ci (Phase 1 batch S — acceptance smoke):
  - new `crates/bs-smoke` workspace binary that drives the
    `examples/vars` debuggee through `bugstalker`'s public library
    API (no PTY, no rustyline races) and asserts every Phase 1
    stdlib type renders via its specialised path. 23 checks: Pin
    (boxed + ref), Range/RangeInclusive/RangeFrom/RangeTo,
    Duration (zero, ms-rounded-to-s, whole-seconds, h:m:s.ms),
    CString (utf-8, empty, hex-preview), OsString, PathBuf,
    MaybeUninit, Mutex/RwLock peel, MutexGuard/RwLockReadGuard,
    DST companions (`&CStr`, `&OsStr`, `&Path`).
  - new `+smoke` Earthfile target. Builds the workspace, runs the
    smoke binary, then runs `cargo bench --workspace -- --quick`
    so a regression that breaks the bench harness (panic / build
    error) fails CI alongside the smoke check. Numerical bench-
    regression gating waits for Phase 8 once real bench bodies
    replace the Phase 0 placeholders.
- variables (Phase 1 batch R — F4 byte-slice overrides):
  - F4 (slash-suffix format spec): `/utf8` and `/hex` now compose
    with `var` / `vard` / `arg` / `argd` on byte-slice values
    (`Vec<u8>`, `VecDeque<u8>`, `[u8; N]`). `/utf8` forces a lossy
    utf-8 render — invalid byte sequences become `\u{FFFD}` instead
    of falling through to a hex dump. `/hex` forces the
    16-bytes-per-row hex dump with ASCII column even when the bytes
    are valid utf-8. Auto-detect (the default with no spec) is
    unchanged.
  - New `pub enum ByteRenderMode { Auto, ForceUtf8, ForceHex }` in
    `bugstalker::debugger::variable::render` plus a
    `render_byte_slice_members` entrypoint so future callers (e.g.
    `&[u8]` not yet wrapped in a `VecValue`) can plug in. The
    existing `try_byte_string_preview` path stays internal and
    routes through the new helper with `Auto`.
  - `FormatSpec` enum gains `Utf8` and `BytesHex` variants;
    `apply_format_spec` dispatches both through a new
    `format_byte_slice` shared between the `Vector` / `VecDeque`
    specialised shapes and bare `Value::Array` of `u8`. Mismatched
    type-vs-spec combinations log `warn!` and fall back as before.
- workspace: root `Cargo.toml` is now a `[workspace]` (`members = [".", "crates/*"]`)
  with shared `[workspace.package]` and `[workspace.dependencies]`.
  `examples/` remains a separate workspace.
- crate: `crates/bs-test-harness/` — empty stub member. Phase 8 will port
  the launch-debuggee → set-breakpoint → render → assert flow currently
  inlined in `tests/debugger/variables.rs` into this crate.
- licensing: `// SPDX-License-Identifier: MIT` headers on every source
  file (`scripts/add-spdx.py` is the regenerator; preserves shebangs).
- licensing: `deny.toml` — cargo-deny config enforcing the licence
  allow-list listed in `doc/plans/phase-0-preflight.md`.
- bench: criterion scaffolding under `benches/`
  (`render_value.rs`, `attach_cold.rs`) wired into root `Cargo.toml` as
  `[[bench]]` entries. Per-crate benches land as later phases add the
  hot-path crates.
- ci: `.github/workflows/ci-nightly.yml`, `ci-bench.yml`, `ci-fuzz.yml`,
  `ci-soak.yml` — scheduled workflow stubs. New `cargo deny` and
  `test-macos` (macos-14) jobs in `ci.yml`.
- build: `Earthfile` `+lint`, `+bench`, `+deny`, `+fuzz` targets.
- build: `Makefile` `nt` / `nt-int` (cargo nextest), `lint`, `bench`,
  `deny`, `fuzz` targets.
- doc: `doc/logging.md` describing the `tracing` convention for
  workspace crates.
- variables (Phase 1 batch Q — stdlib coverage):
  - S1 (`[locked]` badge): `Mutex<T>` and `RwLock<T>` now report
    lock state on the futex backend (Linux, Android, FreeBSD,
    OpenBSD, DragonflyBSD, modern Windows, Hermit, wasm-atomics).
    Detection is structural: BFS the `inner` member for the first
    `u32` scalar (the futex's atomic state — peeled by S3) and
    treat non-zero as locked. macOS / iOS pthread backends and
    Win7 SRWLock conservatively report `locked = false` (no
    field-name match). Renderer prepends `[locked]` when set.
- variables (Phase 1 batch P — stdlib coverage):
  - F4 (format-spec grammar, minimum-viable cut): `/x`, `/b`, `/o`,
    `/d`, `/iso` colon-suffix specs on `var` / `vard` / `arg` /
    `argd` commands. Syntax: `var x /x`, `argd y /iso`. Spec is
    parsed as part of the print command (option A from the design
    review), not embedded in `Dqe`. Applies to top-level scalar
    integers (hex/bin/oct/dec) and to `Duration` / `SystemTime`
    (ISO-8601). `Instant` has no calendar form so `/iso` falls
    back. Type-vs-spec mismatches log a `warn!` and use the
    default render. Five unit tests on `format_iso_duration` plus
    six new parser test cases.
  - F4 syntax note: GDB-style `/x` rather than the originally
    proposed `:x` because `:` collides with `rust_identifier`'s
    `::` namespace separator inside the chumsky expression
    parser. `/[xbod]` matches GDB's existing `print/x` convention.
  - `expression::parser()` no longer hard-codes
    `then_ignore(end())`; the four existing callers keep that
    behaviour explicitly, and the print parser composes the
    optional format-spec suffix without it.
  - Deferred: `/p`, `/c`, `/s`, `/y`, `/utf8`, `/hex`, `/[N]`,
    `/[N..M]` — slice indexing in particular is really a sub-
    expression and probably belongs in `Dqe` rather than as a
    print-time format spec.
- variables (Phase 1 batch O — stdlib coverage):
  - S12/S13/S14 DST companions: `&CStr`, `&OsStr`, `&Path`. rustc
    materialises these fat references as structures with
    `data_ptr` and `length` fields, identical in shape to the
    owned counterparts' BFS targets. Routing them through the
    existing `parse_cstring` / `parse_os_string` helpers gives
    `c"hi"` / `"hi"` / `"/etc"` rendering with no new parser
    code; only the dispatcher gets new arms (suffix-matched on
    `::CStr` / `::OsStr` / `::Path` to tolerate naming variations
    across rustc versions).
- variables (Phase 1 batch N — stdlib coverage):
  - S1 (`[poisoned]` badge): `Mutex<T>` and `RwLock<T>` now read
    the `poison: poison::Flag` field (an `AtomicBool` peeled by
    S3) and surface a `[poisoned]` trailer when the lock has been
    poisoned by a panic in a previously-held critical section. The
    `SpecializedValue::Mutex` variant gained a `poisoned: bool`
    field; renderer prepends `[poisoned]` to the rendered text
    when set. The `[locked]` badge remains deferred (needs
    platform-specific `sys::Mutex` knowledge).
- variables (Phase 1 batch M — stdlib coverage):
  - F3 follow-up: truncation surfacing extended to `HashMap`,
    `HashSet`, `BTreeMap`, `BTreeSet`. New `elided: Option<u64>`
    on `HashMapVariable` / `HashSetVariable`. Iterator collects
    everything (existing behaviour) but the renderer truncates
    to `LEN_GUARD` items and surfaces a
    `[N entries] (… M more elided)` summary line for over-budget
    collections. BTreeSet inherits its inner BTreeMap's elision
    count (a BTreeSet is just a BTreeMap with discarded values).
- variables (Phase 1 batch L — stdlib coverage):
  - F3 (RenderBudget): truncation count is now surfaced in the
    rendered output. `String`, `&str`, `Vec<T>`, and the byte-string
    preview path append `(… N more elided)` when the underlying
    length exceeded the 10 000-element guard. Three-piece API: a
    new `pub struct RenderBudget` with `Default::default()`,
    `pub const LEN_GUARD` / `CAP_GUARD` (promoted from private),
    and a `guard_len_with_truncation` helper that returns
    `(clamped, Option<elided_count>)`. New fields `elided:
    Option<u64>` on `VecValue`, `StringVariable`, `StrVariable`.
    `HashMap` / `HashSet` / `BTreeMap` / `BTreeSet` truncation
    surfacing deferred (same shape, different parsers).
- variables (Phase 1 batch K — stdlib coverage):
  - S15: `alloc::rc::Weak<T>` and `alloc::sync::Weak<T>` now carry
    the strong / weak reference counts read out of `RcBox` /
    `ArcInner` at parse time. The renderer surfaces
    `0xADDR (strong=N, weak=M)` plus a `[dropped]` annotation when
    `strong == 0` (the underlying T has been dropped but the
    allocation is alive because at least one Weak handle remains).
    Detection is a name-prefix split off from the existing Rc/Arc
    dispatch; `Rc<T>` and `Arc<T>` keep their existing
    pointer-only render. `weak.deref(pcx)` continues to walk to
    the RcBox/ArcInner so structural assertions in test_shared_ptr
    still pass.
- variables (Phase 1 batch J — stdlib coverage):
  - S9: `alloc::boxed::Box<T>` smart-deref. Box arrives via the
    pointer dispatch path (rustc emits `DW_TAG_pointer_type` with a
    `Box<…>` name), so we enrich `PointerValue` with an optional
    `dereffed: Option<Box<Value>>` populated at parse time when the
    type identity starts with `alloc::boxed::Box<`. The renderer
    returns `ValueLayout::Wrapped(inner)` for boxes (pointee shown
    inline) and the existing `Referential(ptr)` for raw `*const T` /
    `&T` references. `box_d.deref(pcx)` continues to work
    independently for users who want explicit deref. Trait-object
    boxes (`Box<dyn Trait>`) defer to Phase 3 vtable resolution.
- variables (Phase 1 batch I — stdlib coverage):
  - S2: `MutexGuard<T>`, `RwLockReadGuard<T>`, `RwLockWriteGuard<T>`,
    and their `Mapped*` cousins peel through the guard's parent
    reference (or `data: NonNull<T>` for the read-guard shape) and
    surface the guarded T directly. The dispatcher is ordered so
    `MutexGuard<i32>` matches the guard arm before falling through
    to the `Mutex<…>` arm. The two libstd layouts are handled
    in one helper: read-guards use `data: NonNull<T>` directly,
    write-guards / mutex-guards walk through `lock: &Mutex<T>` →
    `data: UnsafeCell<T>` → T.
- variables (Phase 1 batch H — stdlib coverage):
  - S1 (minimum viable): `std::sync::Mutex<T>` and
    `std::sync::RwLock<T>` peel through their `data: UnsafeCell<T>`
    field to surface the inner T directly. We do not acquire the
    lock — the read may show torn state if another thread is
    mid-write, which is the expected behaviour for a debugger peek.
    `[locked]` and `[poisoned]` badges are queued for a follow-up
    batch (lock-state requires platform-specific `sys::Mutex`
    layout knowledge). DWARF emits parameterized names
    (`Mutex<i32>`) so the dispatcher matches by prefix.
- variables (Phase 1 batch G — stdlib coverage):
  - S10: `core::mem::MaybeUninit<T>` peels through the union's
    `value` arm and the transparent `ManuallyDrop` wrapper to surface
    the underlying `T` directly. Detection covers both shapes rustc
    can emit (`DW_TAG_union_type` and `DW_TAG_structure_type`); the
    DWARF name carries type parameters (`MaybeUninit<i32>`) so the
    match uses `starts_with` rather than equality. The renderer
    delegates to the inner T's layout; the `MaybeUninit<…>` wrapper
    name on the value's type identity carries the "possibly uninit"
    framing.
- variables (Phase 1 batch F — stdlib coverage):
  - S13/S14: `std::ffi::OsString` and `std::path::PathBuf` peel
    through their wrapper chain (PathBuf → OsString → Buf → Vec\<u8\>)
    to surface the underlying bytes. Valid utf-8 → plain quoted
    string (`"/tmp/foo"`); invalid → `b"\xNN…"` hex preview capped at
    32 bytes. Uses the same BFS-find length+pointer probe as S12;
    bound by `LEN_GUARD` and a 64 KiB `MAX_READ` ceiling. OsStr and
    Path (DST companions) deferred to a later batch alongside CStr —
    they need a different dispatcher hook for fat-pointer DSTs.
- variables (Phase 1 batch E — stdlib coverage):
  - S16: `Vec<u8>` / `VecDeque<u8>` (and any vector with `u8`
    elements) gets a render-layer preview: utf-8 → `b"hello"` when the
    first 1 KiB decodes cleanly, hex dump (`xx xx xx … |...hi|` rows
    of 16 bytes with ASCII column) when not. Underlying
    `SpecializedValue::Vector` is unchanged so structural test
    assertions (e.g. `test_arguments` walking the inner items by
    index) still work. Four unit tests on `try_byte_string_preview`
    cover empty / utf-8 / hex / non-u8-returns-None.
- variables (Phase 1 batch D — stdlib coverage):
  - S12: `alloc::ffi::c_str::CString` is rendered as `c"…"` when the
    inner bytes (sans trailing NUL) are valid utf-8, or as a
    `c"\xNN\xNN …"` hex preview (capped at 32 bytes, with a trailing
    `…` continuation marker) when not. The read length is bounded by both
    the existing 10 000-element `guard_len` clamp and a new 64 KiB
    `MAX_READ` ceiling so a corrupted length field can't drive a huge
    inferior-memory read. CStr (DST) and OsString/PathBuf (S13/S14)
    deferred to batch E.
- variables (Phase 1 batch C — stdlib coverage):
  - S4: `core::time::Duration` / `std::time::Duration` peels to
    `(secs, nanos)` and renders human-readable: `0s` for zero,
    `1.500ms` / `250µs` / `7ns` for sub-second, `7s` / `1m 0s` /
    `1h 1m 1.500s` for whole-second-and-above. Eight unit tests
    cover the format-shape edges.
  - S5: `SystemTime` now renders as ISO-8601 / RFC3339 UTC
    (`2026-04-27T19:48:30Z` for whole seconds, `…30.123Z` for
    millis, `…30.123456789Z` only when sub-microsecond precision is
    actually present — `chrono`'s `AutoSi` suppresses trailing zeros)
    instead of the prior `%Y-%m-%d %H:%M:%S` form.
    `Instant` renders as a wall-clock
    delta `now ± HH:MM:SS.mmm`; recovering the program-local
    monotonic-clock epoch is deferred until a TLS/symbol hook
    lands.
- doc: `src/debugger/darwin_mach.rs:564-565` — `tsd[key]` notation
  in prose comments now wrapped in backticks; was tripping
  `cargo rustdoc -- -D rustdoc::broken_intra_doc_links`.
- variables (Phase 1 batch B — stdlib coverage):
  - S6: `core::ops::Range`, `RangeInclusive`, `RangeFrom`, `RangeTo`,
    `RangeToInclusive`, `RangeFull` are rendered to canonical Rust
    source form (`a..b`, `a..=b`, `a..`, `..b`, `..=b`, `..`).
    `RangeInclusive` ranges that have already drained get an
    `[exhausted]` suffix.
  - S7: `core::pin::Pin<P>` is peeled to its pinnee. The wrapper type
    identity (`Pin<Box<T>>`, `Pin<&mut T>`) is preserved on the value;
    the rendered layout is the inner `P`'s layout. Critical-path for
    Phase 3's async stack inspection.
- variables (Phase 1 batch A — stdlib coverage):
  - F1: `DW_TAG_reference_type` and `DW_TAG_rvalue_reference_type` now
    route through the pointer parser instead of being warn-and-skipped.
    Reference-typed DIEs surface from C++ debug info reachable via FFI
    and from non-default rustc codegen flavours.
  - S3: `core::sync::atomic::Atomic*` (every `AtomicI*`/`AtomicU*` plus
    `AtomicBool`/`AtomicUsize`/`AtomicIsize`/`AtomicPtr<T>`) is now
    rendered as the bare scalar/pointer payload, peeling the outer
    wrapper and the `UnsafeCell` indirection. The `AtomicI32` / etc.
    type identity is preserved on the value.
  - S11: `core::ptr::NonNull<T>` is rendered as a plain `*T`,
    preserving the `NonNull<T>` type identity.
- spdx: example fixture sources under `examples/` are excluded from
  the SPDX header scope. Test breakpoints there are pinned by line
  number; a header would shift every breakpoint by one. The script
  `scripts/add-spdx.py` enforces the exclusion.
- debugger: experimental Linux/aarch64 support. Software breakpoints
  (`BRK #0`), general-purpose register read/write via `PTRACE_GETREGSET` +
  `NT_PRSTATUS`, and DWARF unwinding are functional. Hardware watchpoints,
  inferior function calls, and `libthread_db`-backed TLS inspection are
  stubbed and return clear errors.
- register: `RegisterMap::pc()` / `set_pc()` / `sp()` / `set_sp()`
  architecture-agnostic accessors, and `Register::PC` / `Register::SP`
  aliases.
- build: `Earthfile` with `+check`, `+build`, `+build-rel`, `+clippy`,
  `+fmt-check`, `+test`, `+all` targets. Select the target platform with
  `--BS_PLATFORM=linux/arm64` (default) or `--BS_PLATFORM=linux/amd64`.
- error: new `Error::WatchpointUnsupported` variant (returned on
  architectures where hardware watchpoints are not yet wired up).
- error: new `Error::DarwinDebuggerEntitlementMissing` variant.
  Surfaced when `task_for_pid` returns `KERN_FAILURE` from a
  binary that lacks `com.apple.security.cs.debugger`. The
  message names the offending binary and gives the exact
  `codesign -s - --entitlements tests/darwin.entitlements` line
  to fix it, instead of leaking the cryptic Mach error code.

### Changed

- `thread_db` is now an x86_64-only dependency; a thin in-tree shim
  (`debugger::thread_db_compat`) provides stubs on other architectures
  so the debugger degrades gracefully rather than failing to build.
- `src/debugger/register.rs` split into `src/debugger/register/{mod,
  x86_64,aarch64,debug}.rs`. Public surface under `debugger::register::*`
  and `debugger::register::debug::*` is preserved.
- doc: new `doc/ROADMAP.md` describing the larger-than-one-PR efforts
  the codebase is moving towards (Linux/aarch64 port progress, planned
  time-travel record-and-replay support, eventual native macOS port).
- debugger: experimental native macOS-arm64 support — pure-Mach
  Tracer (no ptrace), Mach-native `CallHelper`, software-breakpoint
  install via `mach_vm_write` + page-protection round-tripping,
  per-dylib `__TEXT.vmaddr` slide computation, dSYM bundle DWARF
  loader, eh_frame BaseAddresses with Mach-O section names, CU
  disambiguation when dsymutil's range engulfs other CUs, stray-BRK
  swallowing in dyld pages, and `Tracee::location()` fallback for
  unknown PC mappings.
- debugger/darwin: dlopen/dlclose rendezvous now wired through
  dyld's `task_dyld_process_info_notify_register` Mach-IPC port
  instead of the legacy `_lldb_image_notifier` software-breakpoint
  protocol. The new `DyldNotifyPort` allocates a receive port,
  registers it with the inferior task, and polls for `LOAD` /
  `UNLOAD` / event messages between exception receives; every
  message is replied to (`mach_msg_overwrite(SEND|RCV)` semantics
  mean dyld blocks on every kind, not just the synchronous
  events). When a load arrives and a `LinkerMapFn` BP is
  registered, the tracer suspends the task and synthesises
  `StopReason::Breakpoint(linker_map_addr)` so the existing
  refresh-deferred path runs. Unblocks
  `tests/debugger/breakpoints::test_brkpt_on_line_collision`
  and `test_deferred_breakpoint`.
- debugger/darwin: `DwarfRegistry::update_mappings` now consults
  dyld's own `infoArray` for each dylib's `imageLoadAddress`
  rather than picking the lowest-VA `proc_pidinfo` region. On
  darwin the kernel keeps a parse-time mmap of the dylib at a low
  VA in addition to the runtime slid mapping, and the old
  `min_by(start)` heuristic preferred the parse region — which
  gave a bogus slide and made every BP install in a dlopen-loaded
  dylib EFAULT.
- debugger/darwin: `Rendezvous::link_maps()` re-walks the dyld
  image list on every call, so newly `dlopen`-ed dylibs surface
  in `update_debug_info_registry`. Was a stale snapshot taken at
  `Rendezvous::new` time.
- debugger/darwin: `vm_write_word` chooses its post-write
  protection from `cur_protection & W`, not by hardcoding `R+X`.
  Earlier versions assumed the only caller was a BP install into
  text, but `Debugger::write_memory` also feeds the inferior-call
  data scratchpad (string header, vtable, Formatter struct on a
  freshly `mmap`-ed `R+W` page) — snapping that page to `R+X`
  after each scratch write made the inferior's first store into
  the buffer raise `KERN_PROTECTION_FAILURE`. We now key the
  restore off `cur_prot`: writable pages stay `R+W` (data
  scratchpad), non-writable pages restore to `R+X` (text and the
  dyld shared cache, which reports `max=R` for genuinely
  executable code so `max_protection` is *not* a usable signal).
  The trampoline page is still `R+W` from `mmap` — `CallHelper::call_fn`
  flips it explicitly via `darwin_mach::vm_protect_rx` after
  writing `BLR x8 ; BRK #0`, since darwin's W^X bars the
  inferior `mmap` from requesting `PROT_EXEC | PROT_WRITE`
  directly.
- debugger/darwin: `task_for_pid` results are cached per-pid so
  hot paths (every `read_memory_by_pid` call) don't re-enter the
  serialised kernel syscall. Without this, parallel test runs
  collapse onto the kernel's `task_for_pid` lock; with it, the
  full `tests/debugger` suite under `cargo nextest` finishes in
  ~40s wallclock vs ~800s with `cargo test --test-threads=1`.
- debugger/dwarf: `BsUnit::find_exact_place_by_pc` no longer
  panics with a usize-underflow when `binary_search_by_key`
  lands at index 0. Pre-existing on every platform but only
  reachable via the darwin Debug::fmt path that was previously
  short-circuiting before the lookup.
- The integration suite reaches **86 passed / 0 failed / 1 skipped
  (`mod tokio` Linux-gated)** on darwin/aarch64 sequentially. All
  previously-skipped categories — multi-thread, signals, TLS init
  *and* uninit, loop-step (issue #156), and full Phase-3 dyn /
  niche / async / cycles — now pass. Earlier intermediate state
  (62/75 with broad `--skip`) reached after the `task_for_pid`
  cache landed; superseded by the I-cache invalidation +
  signal-port hybrid + TLS uninit-state decoder finishes that
  followed.

### Fixed

- call: align the debuggee's RSP to 16 bytes before the inferior `CALL`
  instruction in `CallHelper::call_fn`, as System V AMD64 requires.
  Previously the trampoline kept whatever RSP the debuggee was stopped
  at; depending on which line the breakpoint landed on, RSP was often
  only 8-aligned, which caused alignment-sensitive callees (anything
  using `movaps`/`movdqa` on stack locals — `Vec::reserve`,
  `String::push_str`, …) to take an intermittent `#GP` partway through
  `Debug::fmt`. Manifested as a flaky
  `tests/debugger/variables.rs::test_debug_trait_repr_vars` regardless
  of architecture.
### Deprecated
### Breaking changes

# [0.4.5] Apr 18 2026

### Added

- debugger: added support for rustc 1.95

### Changed

- lock gimli version to 0.33.0
- dap: rename Zed extension id

### Fixed

- debugger: stepover may skip some lines in for loops

---
# [0.4.4] March 28 2026

### Added

- dap: new DAP server with remote debugging support
- dap: `Zed` extension

### Fixed

- dap: now output events send immediately

---

# [0.4.3] March 6 2026

### Added

- debugger: added support rustc 1.94
- debugger: help for subcommands (#81)

### Changed

- debugger: set MSRV to 1.89.0 (#139)
- debugger: update gimli to 0.33.0

### Fixed

- debugger: panic when capacity of VecDeq equals to 0 (#144)

---

# [0.4.2] Jan 23 2026

### Added

- debugger: added support rustc 1.93

### Fixed

- console: fixed `watch +w` command
- fix: now `BsUnit::find_exact_place_by_pc` deterministically return always first suitable place

---

# [0.4.1] Jan 19 2026

### Fixed

- debugger: unwinder no longer stops if there is no debug information in some frame of a call stack

---

# [0.4.0] Jan 5 2026

### Added
- dap: introduce DAP extension for VS Code
- dap: introduce DAP server

### Changed
- build: remove libunwind-specific test target

---

# [0.3.6] Dec 13 2025

### Added

- debugger: added support rustc 1.92

---

# [0.3.5] Nov 3 2025

### Added

- debugger: add `GlobalContext`
- debugger: added support rustc 1.91

### Changed

- debugger: use string interner
- debugger: use ecx/ccx/pcx/etc naming for different contexts
- debugger: parse DIEs on demand rather than upfront to reduce initial memory load
- debugger: reduce memory consumption for debug information representation
- debugger: reduce memory consumption for symbol tables

### Fixed

- debugger: panic when vecdeque have infinite capacity (bug in debug info)

### Deprecated
### Breaking changes

---

# [0.3.4] Sep 19 2025

### Added

- debugger: added support for rustc 1.90

### Fixed

- build: fail early at compile rather than runtime

### Deprecated

- debugger: deprecate `libunwind` support

---

# [0.3.3] Aug 9 2025

### Added

- ui: new output for `backtrace` command (with source file and line)
- debugger: add `--save-history` option
- debugger: added support for rustc 1.89

### Changed

- update `tui-realm` and `tui-realm-treeview` components
- add `PopIf::pop_if_single_el`
- update `chumsky` to a stable version `0.10.1`
- debugger: now backtrace frames contains a source file and line

### Fixed

- tui: fix panic when there is a thread with unknown first frame function in backtrace
- debugger: fix panic when when parse zero-length arrays

---

# [0.3.2] Jun 30 2025

### Added
- debugger: added support for rustc 1.88
- debugger: new `DataCast` DQE op

---

# [0.3.1] May 18 2025

### Added
- debugger: added support for rustc 1.87

### Fixed
- debugger: enable LTO and codegen-units = 1 for release build

---

# [0.3.0] Apr 26 2025

### Added

- debugger: support for `SystemTime` and `Instant` std types
- debugger: support for constant initialized TLS variables
- debugger: new `async backtrace` command (#27)
- debugger: new `async backtrace all` command (#27)
- debugger: new `async task` command (#27)
- debugger: new `async stepover` command
- debugger: new `async stepout` command
- debugger: new `trigger` command (#39)
- debugger: new `call` command
- debugger: new `vard` and `argd` commands (#47)
- docs: introduce website and update README

### Changed

- debugger: refactor `select` module
- debugger: rename watch_point -> spy_point
- debugger: refactor `TypeIdentity`
- debugger: refactor variables specialized representation
- debugger: `variable` module refactoring
- debugger: improve rustc versions resolving
- ui: refactor command parser tests
- debugger: use IndexMap instead of HashMap for storing type parameters


### Fixed

- debugger: `stepover` command can no longer step out from the current source file
- debugger: now `restart` command doesn't affect a breakpoint numbers
- console: reduce redundant output for collections (arrays, maps, etc.) (fix #52)
- console: in variables output use spaces instead of tabs
- console: better memory command output
- debugger: fix rustup toolchain command parsing
- console: don't send duplicate SIGINT signal

---

# [0.2.8] Apr 7 2025

### Added
- debugger: added support for rustc 1.86

### Fixed
- fix broken nix flake
- fix CI libunwind installation script

---

# [0.2.7] Feb 23 2025

### Added
- debugger: added support for rustc 1.85

### Changed
- use rust edition 2024

---

# [0.2.6] Jan 13 2025

### Added
- debugger: added support for rustc 1.84

### Fixed
- update github actions

---

# [0.2.5] Nov 30 2024

### Added
- debugger: added support for rustc 1.83

---

# [0.2.4] Oct 20 2024

### Added

- debugger: added support for rustc 1.82
- debugger: fix flaky ordering in `sharedlib info` command

---

# [0.2.3] Sep 8 2024

### Added

- debugger: added support for rustc 1.81

---

# [0.2.2] Jul 27 2024

### Added

- debugger: added support for rustc 1.80

---

# [0.2.1] Jun 15 2024

### Added

- debugger: added support for rustc 1.79
- chore: added nix flake

### Changed

- debugger: now can find debugee binaries with `which`

---

# [0.2.0] Jun 3 2024

### Added

- tui: added ability to select tab across both windows
- tui: now left and right windows can expand (and the opposite window,
  accordingly, collapsed)
- ui: new argument (`-t` or `--theme`) for theme switching (affects program data
  and source code output)
- ui: warning if debugee compiled with an unsupported rustc version
- debugger: the index operation is now applicable to hashmaps, hashsets,
  btreemaps and others
- debugger: now containers (hashmaps, hashsets, etc.) can be indexed by literal
  objects for advanced searching
- console: improve index operation, now index accepts literal objects
- debugger: added address operator in data query expressions
- debugger: added watchpoints over hardware breakpoints
- debugger: added canonic operator
- tui: added keymap configuration

### Changed

- tui: now current active line (in a source code window and disassemble window)
  glued to the middle of render area instead of the bottom of the screen
- console: now program data (variables and arguments) stylized with syntect
- tui: now variable and thread tabs stylized with syntect

### Fixed

- ui: possible stack overflow when switching between ui types
- debugger: panic, when value of the right bound in a slice operator was greater than the underlying container lenght
- tui: panic, when breakpoint set at memory address
- tui: async error leads to ignoring of a new commands by TUI app
- debugger: check that value of DW_ATE_UTF encoding is valid utf8 char

---

# [0.1.5] May 3 2024

### Added

- debugger: added support for rustc 1.78

### Fixed

- debugger: now tracer doesn't add new tracee to tracee_ctl if first
  tracee.wait() return exited status instead of ptrace event status

---

# [0.1.4] April 3 2024

### Changed

- console: history hints now have better highlighting (grey instead of bolt)

### Fixed

- console: now sub commands (like break remove or break info) don't clash with
  operation + argument
- debugger: updated `unwind` crate to 0.4.2, now it must support rcX releases of
  libunwind
- console: fix expression parser. Now field op, index op and slice op have the
  same priority and can be combined in any order
- console: now command parser considers spaces when finding subcommands

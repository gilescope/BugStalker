# Phase 3 — `dyn Trait` recovery, niche resilience, async await-trace

The four features in this phase are interdependent. They all consume
`rust-mangle-tree` (Phase 2). They all rely on the rule that *Rust
language knowledge beats DWARF guesswork*.

## A. Vtable-driven `dyn Trait` concrete-type recovery

The headline feature. Today, `Box<dyn Error>` renders as a fat-pointer
struct showing `pointer` and `vtable`. With v0 mangling we can resolve
the vtable symbol back to `<Concrete as Trait>` and render the
underlying value.

### Mechanism

1. Detect a fat-pointer trait-object struct in the DWARF type model.
   Existing detection lives in `src/debugger/debugee/dwarf/type.rs`
   where `Structure` carries the two pointer fields. Add a marker on
   `TypeDeclaration::Structure` so the renderer knows it is a trait
   object.
2. At render time, read the vtable pointer from the value.
3. Look up the symbol containing that address. The symbol table built
   in `src/debugger/debugee/dwarf/symbol.rs` already supports
   address-to-symbol lookup; vtables are emitted with names like
   `_RNvXs_Cs…<MyType as core::fmt::Debug>::{vtable}`. A vtable's
   linkage name appears in DWARF as the `DW_TAG_variable` whose name
   ends in `::{vtable}`.
4. Demangle the vtable symbol with `rust-mangle-tree`. The resulting
   `Path` exposes `impl_self_type()` (the `<Concrete>`) and
   `impl_trait()` (the `<Trait>`). Extract the self-type's path.
5. Resolve that path back to a DWARF type DIE. We need a
   `name → TypeId` index built once per module load, keyed on the
   stringified path produced by `rust-mangle-tree`.
6. Render the data pointer as a value of that concrete type, recursing
   through the existing `RenderValue` machinery.

### Caching

- Per-vtable-address cache: `HashMap<RelocatedAddress, ConcreteTypeId>`.
- Invalidated on `dlopen`/`dlclose` rendezvous events. BugStalker
  already wires these on Linux and Darwin (recent commits `dea5685`,
  `de56ab5`).
- Cache cold cost: one symbol lookup + one demangle + one type-name
  resolve, all O(log n). Target: < 100 µs cold, < 1 µs warm.

### Linker fast-path

When the binary is linked with Wild and carries `.bs_vtables`
(Phase 7), the symbol-lookup + demangle + name-resolve chain
collapses into a single binary search keyed on vtable address.
Cold cost drops to < 1 µs — the same as the warm cache. Cache
becomes redundant when the section is present.

BugStalker checks for the section at attach. If present, route
through it; if absent, use the slow path described above. The user
never picks; the runtime detects.

### Failure modes

- **Legacy mangling.** Vtable symbol is `_ZN…` with no generic info.
  Fallback: render fat-pointer struct as today. Surface a
  `[concrete type unavailable; binary uses legacy symbol mangling —
  rebuild with -C symbol-mangling-version=v0]` hint.
- **Stripped vtable symbol.** Some link configurations strip vtable
  symbols. Fallback: same as legacy.
- **Concrete type not in DWARF.** Possible with split-debuginfo when
  a `.dwo` is missing. Fallback: render as opaque pointer with
  recovered type *name* but no field access.
- **Dynamic linking across crates.** Vtable belongs to a `cdylib`
  loaded via `dlopen`. Rendezvous handler must register that module's
  symbols before resolution succeeds.

### Test plan

`tests/debugger/dyn_trait.rs` (new) — debugees containing
`Box<dyn Error>`, `Arc<dyn Send + Sync>`, `&dyn Iterator<Item = u32>`,
nested `Box<dyn Trait<Box<dyn Other>>>`. Assert rendered output
includes the concrete type name and field values.

## B. Niche-resilient `Option<T>` and `Result<T, E>`

DWARF cannot unambiguously express niche-encoded enums
(rust-lang/rust#62839, open since 2018). The rustc Python printers fall
back to the legacy `RUST$ENCODED$ENUM$` field-name hack. We do better
by applying Rust's known niche rules directly.

### Recognised niches

Implement in `src/debugger/variable/specialization/niche.rs` (new):

| Pattern | Niche |
| -------------------------- | --------------------------------------- |
| `Option<&T>` / `Option<&mut T>` | null pointer = `None` |
| `Option<Box<T>>` | null pointer = `None` |
| `Option<NonNull<T>>` | null pointer = `None` |
| `Option<NonZero*>` | zero = `None` |
| `Option<bool>` | byte ≥ 2 = `None` (and which value?) |
| `Option<char>` | byte pattern outside `0x0..=0x10FFFF` = `None` |
| `Option<fn(...)>` | null function pointer = `None` |
| `Option<Rc<T>>` / `Option<Arc<T>>` | null inner pointer = `None` |
| `Result<T, E>` (one ZST arm) | niche of the non-ZST arm |

### Algorithm

1. Detect `Option`/`Result` by name match (already done in
   `RustEnum` recognition).
2. If the type has a `DW_TAG_variant_part` with explicit discriminant,
   use it (existing path).
3. Otherwise, check if the inner type matches a known niche pattern.
   Apply the niche rule directly to determine the variant.
4. If no rule matches, fall back to byte-pattern comparison against
   the variant's expected layout (last-resort heuristic).

### Why this is better than DWARF

DWARF sometimes emits a single `DW_TAG_variant_part` with no
discriminant for niche enums, leaving consumers to guess. Our approach
short-circuits the guessing using language guarantees. Robust against
any future DWARF-emission churn upstream.

### Test plan

Extend `tests/debugger/variables.rs`:

- `test_option_niche_box`
- `test_option_niche_nonnull`
- `test_option_niche_nonzero`
- `test_option_niche_bool` (bit-patterns 2..255)
- `test_option_niche_char` (invalid scalar values)
- `test_result_niche_zst_err`

## C. `Rc<T>` / `Arc<T>` cycle detection

We already render strong/weak counts. Add cycle detection so a
`Rc<RefCell<Node>>` linked-list-with-back-edge does not print itself
into stack overflow.

### Mechanism

- Per-render-tree visited set: `HashSet<RelocatedAddress>` of every
  `Rc`/`Arc` inner-allocation pointer seen in the current value walk.
- On revisit, render `[cycle to 0x…]` as a leaf node with hyperlink
  metadata for the TUI to navigate to the original.
- Render-tree depth cap (configurable, default 64) as a second guard
  against pathological non-cyclic graphs.

### Display

```text
my_node = Rc<Node> @ 0x7f… (strong=2, weak=0)
  ├── data: 42
  └── next: Some(Rc<Node> @ 0x7g… (strong=2, weak=0)
        ├── data: 43
        └── next: Some([cycle to 0x7f…]))
```

Bonus: add `[strong=1]` annotation as a diagnostic — not shared, so
why is it `Rc`?

## D. Async await-trace

Render the async call stack alongside the synchronous call stack when
stopped inside a runtime poll.

### Status

- **D1 (landed, commit `98b6c18`)** — steps 3 (active-variant decode,
  already wired by Phase 1's `RustEnumValue` path) and 4 (source-coord
  recovery from `DW_AT_decl_file`/`DW_AT_decl_line` on variant member
  DIEs). `RustEnumValue.await_location` and `AsyncFnFuture.await_location`
  are populated; the existing `async backtrace` output now appends
  an `at FILE:LINE` suffix to "suspended at await point N" lines.
- **D2 (pending)** — steps 1, 5, 6, 7: dedicated coroutine-type
  detection, awaitee-chain walker, vtable cross-resolution for
  `Pin<Box<dyn Future>>`, and the dedicated `await-trace` console
  command + TUI panel.
- **D3 (pending)** — DAP `bs/awaitTrace` request + the test plan in
  `tests/debugger/async_await.rs` (simple / chained / dyn_future /
  select / join cases).

### Background

`async fn` bodies compile to a synthesised generator/coroutine enum.
Each `.await` is a state. The compiler emits the state-machine type
into DWARF; each variant's member fields carry `DW_AT_decl_file` and
`DW_AT_decl_line` pointing at the source location of the corresponding
`.await`. Cliff Biffle's `lildb` proves this works.

### Mechanism

1. **Detect a coroutine type.** DWARF type name pattern
   `{async_fn_env#0}` or `{coroutine_env#0}`, plus the usual variant
   structure. Add detection in `src/debugger/debugee/dwarf/type.rs`.
2. **Find the running future.** When stopped, walk up the stack
   looking for a frame whose first argument is `Pin<&mut F>` where
   `F` is a coroutine type. The poll function's signature is
   diagnostic.
3. **Decode the active variant.** Read the discriminant; identify
   which `Suspend<N>` variant is active.
4. **Read the source coords.** The variant's member DIEs have
   `DW_AT_decl_file` and `DW_AT_decl_line`. That is the `.await`
   we are paused at.
5. **Walk the awaitee chain.** The active variant typically holds an
   `__awaitee` field — the inner future being awaited. Recurse:
   detect its coroutine type, decode its active variant, etc.
6. **Cross with vtable recovery (Feature A).** When the awaitee is
   `Pin<Box<dyn Future>>`, use vtable resolution to find the concrete
   future type, then continue the chain.
7. **Render as a parallel stack-trace.** New TUI panel and console
   command `await-trace`.

### Display

```text
await-trace:
  #0  my_app::handler at src/handler.rs:42  (.await on db.query)
  #1  my_app::middleware::auth::check at src/auth.rs:18  (.await on token.verify)
  #2  hyper::server::conn::handle at hyper-1.0.0/src/server.rs:91
  ...
```

### Limitations and honest caveats

- **rust-lang/rust#104830**: v0 cannot distinguish `async fn` body
  closures from ordinary closures by the symbol's namespace tag.
  Detection falls back to DWARF coroutine-type-name patterns; if
  the compiler emits a different name in some future version, this
  breaks. Document the tested rustc range.
- **No column info**: `DW_AT_decl_column` is not always emitted on
  variant fields. Source location is line-precision only.
- **`impl Trait` futures appear as opaque types** (`p` in v0). The
  await-trace stops at the first opaque future unless DWARF carries
  the concrete type.
- **Inlined `await`s lose granularity**: a heavily-optimised future
  may collapse multiple `.await`s into a single state. Build with
  `--release` reduces resolution; `RUSTFLAGS="-C debuginfo=2"` does
  not fully solve it.

### Test plan

`tests/debugger/async_await.rs` (new):

- `test_await_trace_simple` — single `async fn` with one `.await`;
  break inside the awaitee, verify trace shows the `.await` line.
- `test_await_trace_chained` — three nested `async fn`s, verify
  each level present.
- `test_await_trace_dyn_future` — `Pin<Box<dyn Future>>` in the
  chain, verify vtable recovery extends the trace.
- `test_await_trace_select` — `tokio::select!` with multiple branches,
  verify the *active* branch is identified.
- `test_await_trace_join` — `tokio::join!`; verify all sub-futures
  appear.

## Tokio task tree (deferred)

Originally scoped here; moving to a follow-up phase because the
implementation pivots on tokio's internal `OwnedTasks` linked-list
layout, which is *not* covered by `rustc-mangle-tree`'s capabilities
and varies per tokio version. Track in roadmap as Phase 6.

## Acceptance criteria

- All four features land behind a single feature flag
  (`v0-features` or similar) so they can be turned off if v0 demangling
  produces an edge-case bug in the field.
- `tests/debugger/dyn_trait.rs`, `tests/debugger/variables.rs`
  niche tests, async tests all pass on Linux and Darwin.
- Cycle detection has a regression test that previously stack-overflowed
  the renderer.
- Performance: vtable resolution cache hit is < 1 µs; cold lookup
  < 100 µs measured under `cargo bench`.

## Effort estimate

~6 weeks engineer-time. Vtable resolution is ~1 week. Niche enum
handling ~1 week. Cycle detection ~3 days. Async await-trace ~3 weeks
including the TUI panel and tests. Buffer for the inevitable DWARF
version-specific hacks: ~1 week.

## DAP integration

**Vtable-resolved values** (Feature A) appear via the standard
`variables` request — the resolved concrete type is the `value`
string and `type` field. Clients see `Box<dyn Error> → MyError {
inner: "..." }` automatically; no client-side change required.

**Niche-resilient `Option`/`Result`** (Feature B): no protocol
exposure; affects `VariablesResponse` correctness only.

**`Rc`/`Arc` cycle markers** (Feature C): rendered as
`[cycle to 0x…]` in the value string; standard DAP `variables`
covers it.

**Async await-trace** (Feature D): new custom DAP request
`bs/awaitTrace`:

```text
request:  { command: "bs/awaitTrace", arguments: { threadId: N } }
response: { body: { frames: [{ source, line, column?, awaitee }, ...] } }
```

VSCode renders this as a panel parallel to the call-stack view.
Reuses DAP's `Source` shape for the source location, so existing
go-to-source affordances work.

## Specifications

- DWARF Debugging Information Format Version 5 — <https://dwarfstd.org/doc/DWARF5.pdf>. §5.7.10 (`DW_TAG_variant_part`, `DW_AT_discr`, `DW_AT_discr_value`); §3.3.8 (`DW_AT_decl_file`/`decl_line`/`decl_column`).
- The Rust Reference, type layout — <https://doc.rust-lang.org/reference/type-layout.html>. Niche optimisation, `Pin<P>` `repr(transparent)`.
- The Rustonomicon — <https://doc.rust-lang.org/nomicon/>. Pin guarantees, exotic sizes, vtable mention.
- rustc dev guide, async/await — <https://rustc-dev-guide.rust-lang.org/async-await.html>. Coroutine/state-machine desugaring.
- rustc dev guide, MIR & monomorphisation — <https://rustc-dev-guide.rust-lang.org/backend/monomorph.html>. Generic instantiation flow.
- Nightly Rust unstable book — `core::ptr::DynMetadata` — <https://doc.rust-lang.org/nightly/std/ptr/struct.DynMetadata.html>. Unstable but documents the layout we reverse-engineer.
- Rust source — `compiler/rustc_codegen_ssa/src/meth.rs` — vtable layout (drop_fn, size, align, then methods). Pin to a specific rustc commit when reading; the layout is unstable.
- Cliff Biffle, async decl coords — <https://cliffle.com/blog/async-decl-coords/>. The `DW_AT_decl_line` technique for `.await` coords.
- Cliff Biffle, lildb — <https://cliffle.com/blog/lildb/>. End-to-end demonstration that the async-decode approach works.
- rust-lang/rust#62839 — niche enum DWARF ambiguity (open since 2018).
- rust-lang/rust#73524 — async backtraces tracking.
- rust-lang/rust#1563 — `dyn Trait` debug repr (open since 2012).
- rust-lang/rust#104830 — async/closure v0 namespace tag ambiguity.
- rust-lang/rust#65564 — `DebuggerView` trait proposal.

## Invariants

These are the runtime checks we encode at vtable resolution, niche detection, cycle detection, and async chain walking.

```rust
// Vtable structure: at minimum [drop_fn, size, align] then methods.
debug_assert!(vtable_size >= 3 * pointer_size);
debug_assert_eq!(vtable_size % pointer_size, 0);

// Pin<P> is repr(transparent) over P.
debug_assert_eq!(size_of::<Pin<P>>(), size_of::<P>());

// Box<T> / NonNull<T> non-null inside Some(...).
debug_assert!(some_box_inner_ptr != ptr::null(),
    "Some(Box<{}>) had null inner ptr — niche-encoded None misclassified?", t);

// Coroutine state in range.
debug_assert!(active_variant_index < total_variant_count,
    "coroutine variant {} >= count {}", active_variant_index, total_variant_count);

// Cycle-detection visited set: addresses present iff already rendered.
debug_assert!(self.visited.contains(&addr) || !on_revisit);

// Awaitee chain bounded — not infinite.
debug_assert!(awaitee_chain.len() <= MAX_AWAIT_DEPTH);

// vtable resolution: lookup result internally consistent.
debug_assert!(resolved_concrete_type.size() <= original_pointee_layout.size_hint());

// Discriminant width matches the variant_part's discr type.
debug_assert_eq!(discr_byte_width, variant_part.discr_type.byte_width());
```

The niche-detection invariant is the most important: when DWARF and our niche rules disagree, we trust the niche rules (per architectural rule 4 in the manifesto) — the assert documents the *rare* mismatch case for triage, not as a correctness gate.

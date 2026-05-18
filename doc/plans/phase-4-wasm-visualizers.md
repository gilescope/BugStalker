# Phase 4 — Visualiser extensibility

User-extensible pretty-printers shipped by crate authors and loaded by
BugStalker. Two surfaces, aligned with the trust boundary:

- **Tier A — `#[derive(DebugView)]`** for the crate author's *own*
  types. Macro emits a declarative `TypeViewSpec` into a binary
  section; BugStalker reads it directly. No wasm runtime involved.
  Most users, most types.
- **Tier B — wasm components** for *third-party* visualisers loaded
  from `~/.config/bugstalker/visualizers/*.wasm`, or for crate authors
  who want full Rust expressivity in a sandboxable artifact. Behind a
  cargo feature `viz-wasm`.

The design rule: **trust origin determines runtime path.** Code shipped
inside the debuggee binary is already trusted (you compiled it). Code
loaded from disk at debugger startup is not, and runs in a wasm
sandbox.

## Why this split, not Python

GDB and lldb both use Python. Python is the wrong answer for three
reasons:

1. **Sandboxing.** A Python pretty-printer runs in the debugger
   process with full host access. Auto-loading scripts from a
   debuggee's `.debug_gdb_scripts` is a known prompt-injection vector.
2. **Build complexity.** Embedding CPython adds a heavy runtime
   dependency and platform-specific build pain. Wasmtime is pure Rust
   and statically linkable.
3. **Stable interface.** Python pretty-printers in lldb couple to the
   `lldb.SBValue` ABI; in gdb to `gdb.Value`. Both have churned. A WIT
   contract gives us a typed, versioned interface.

Why the Tier A / Tier B split? Because the wasm sandbox is overhead
when the visualiser is the user's own code, compiled in their own
binary, by their own toolchain. For first-party types, declarative
specs cover the 90 % case ergonomically — see the macro design below.
The wasm path stays for third-party crates and for the 10 % of cases
where declarative isn't enough.

## Crate layout

```text
crates/
├── bs-viz-spec/            # TypeViewSpec data format (Tier A)
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs          # serde-encoded spec + decoder
├── bs-viz-derive/          # #[derive(DebugView)] proc-macro
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs
├── bs-viz-sdk/             # what crate authors depend on
│   ├── Cargo.toml
│   ├── README.md
│   └── src/
│       └── lib.rs          # re-exports derive + spec types + traits
├── bs-viz-api/             # WIT for Tier B (wasm)
│   ├── Cargo.toml
│   ├── wit/
│   │   └── visualizer.wit
│   └── src/
│       └── lib.rs          # generated bindings + host glue
└── bs-viz-host/            # wasmtime runtime, behind cargo feature
    ├── Cargo.toml
    └── src/
        ├── lib.rs
        ├── loader.rs       # discovery + module instantiation
        ├── debuggee_capability.rs   # `debuggee` interface
        └── cache.rs
```

## Tier A — `#[derive(DebugView)]`

The user-facing promise: *add an attribute to a type, get a debugger
visualiser.* No wasm toolchain, no separate crate, no manual
registration. The macro reads the type definition and emits a
declarative spec into a binary section. BugStalker reads the spec at
attach and applies it.

### Quickstart

```rust
use bs_viz_sdk::DebugView;

#[derive(DebugView)]
#[bs_viz(summary = "Person({name}, age {age})")]
pub struct Person {
    pub name: String,
    pub age: u32,
    #[bs_viz(skip)]
    private_token: Vec<u8>,
    #[bs_viz(format = "iso8601")]
    pub created: std::time::SystemTime,
}
```

That's it. Rebuild. BugStalker now renders `Person` instances using the
spec. No `Cargo.toml` change beyond depending on `bs-viz-sdk`. No
runtime cost in the debuggee — the spec is data, not code.

### Supported attributes

Type-level (`#[bs_viz(...)]`):

| Attribute | Effect |
| ---------------------------- | ----------------------------------- |
| `summary = "fmt"` | Single-line summary using `{field}` placeholders |
| `expand = "default"\|"compact"\|"full"` | Default tree-expand state |
| `name = "OtherName"` | Display name override |
| `match_path = "regex"` | Match other instantiations of this generic |

Field-level:

| Attribute | Effect |
| ---------------------------- | ----------------------------------- |
| `skip` | Hide field from rendering |
| `rename = "label"` | Display under different name |
| `format = "hex"\|"bin"\|"oct"\|"iso8601"\|"duration"\|"utf8"\|"hexdump"` | Formatter override |
| `summary` | Promote this field's value as the parent's summary |
| `flatten` | Lift inner fields up one level |
| `as = "Path"` | Reinterpret-cast to a different type |
| `len_field = "n"` | For slice-like custom types: length comes from another field |

Variant-level (on enums):

| Attribute | Effect |
| ---------------------------- | ----------------------------------- |
| `summary = "fmt"` | Variant-specific summary |
| `tag = "Connected"` | Map this variant to a state-tag display |

### What the macro emits

Given the `Person` example above, `#[derive(DebugView)]` expands to:

```rust
const _: () = {
    #[link_section = ".bs_viz_spec"]
    #[used]
    static SPEC: &[u8] = &bs_viz_spec::encode_const(
        bs_viz_spec::TypeViewSpec {
            type_name: "my_crate::Person",
            summary: Some("Person({name}, age {age})"),
            fields: &[
                bs_viz_spec::Field {
                    name: "name", rename: None, hidden: false,
                    format: bs_viz_spec::Format::Default,
                },
                bs_viz_spec::Field {
                    name: "age", rename: None, hidden: false,
                    format: bs_viz_spec::Format::Default,
                },
                bs_viz_spec::Field {
                    name: "private_token", rename: None, hidden: true,
                    format: bs_viz_spec::Format::Default,
                },
                bs_viz_spec::Field {
                    name: "created", rename: None, hidden: false,
                    format: bs_viz_spec::Format::Iso8601,
                },
            ],
            // ...
        }
    );
};
```

The `.bs_viz_spec` section accumulates one entry per `#[derive]`'d
type. BugStalker walks the section at attach time and indexes by
`type_name` (the demangled v0 name from `rust-mangle-tree`).

### What the macro does NOT do

- It does not run code at debug time. The spec is pure data.
- It does not call into the debuggee for visualisation — the spec
  describes how to *interpret* the bytes, not custom logic.
- It does not embed wasm. That is Tier B.

### Custom logic escape hatch (still Tier A, no wasm)

For visualisations that go beyond declarative — a probe of a custom
slot table, a custom hash structure walk — the user can implement
the `CustomView` trait on their type:

```rust
use bs_viz_sdk::{DebugView, CustomView, ViewContext, Summary};

#[derive(DebugView)]
#[bs_viz(custom)]
pub struct SlotMap<T> {
    slots: Vec<Slot<T>>,
    free_head: u32,
    generation: u32,
}

impl<T: DebugView> CustomView for SlotMap<T> {
    fn view(&self, cx: &mut ViewContext) -> Summary {
        let live: Vec<_> = self.slots.iter()
            .filter(|s| s.is_live())
            .map(|s| cx.child("entry", &s.value))
            .collect();
        Summary::tree(format!("SlotMap[{}]", live.len()), live)
    }
}
```

Same trust model: this code is in the debuggee binary, so we trust it.
BugStalker invokes it via the existing `call_debug_fmt` machinery
(`src/debugger/call/fmt.rs`) — the same plumbing already used to call
`Debug::fmt`. The function gets a `ViewContext` synthesised in mmapped
memory, just like the `Formatter`.

The macro emits the symbol of the `view` method into the spec entry so
BugStalker knows to call it instead of applying the declarative path.

### Generic types

`#[derive(DebugView)] struct Wrap<T> { inner: T }` registers a spec for
each *monomorphisation* (just like `Debug` derive). The macro generates
one `static` per `impl<T> ... where T: ...` instantiation that the
compiler emits. BugStalker matches on the demangled monomorphised name.

### Conditional compilation

The macro is a no-op in `--release` unless `bs_viz_sdk = { version =
"...", features = ["release"] }`. By default, debug builds carry
visualiser specs; release builds do not. This matches the Rust
debugger ecosystem's existing convention that debug info is a
debug-build artifact.

## Tier B — wasm components

For third-party visualisers loaded from disk, or for crate authors
who want full Rust expressivity in a sandboxable form, the wasm
component path applies.

## WIT interface

`crates/bs-viz-api/wit/visualizer.wit`:

```wit
package bugstalker:viz@0.1.0;

interface debuggee {
    record address { value: u64 }
    record type-id { id: u64 }

    variant debuggee-error {
        bad-address(string),
        type-not-found(string),
        permission-denied,
        truncated(u32),
    }

    /// Read raw bytes from debuggee memory.
    read-bytes: func(addr: address, len: u32) -> result<list<u8>, debuggee-error>;

    /// Read an integer of given byte width (1/2/4/8). Endianness is target's.
    read-uint: func(addr: address, width: u32) -> result<u64, debuggee-error>;
    read-int: func(addr: address, width: u32) -> result<s64, debuggee-error>;
    read-pointer: func(addr: address) -> result<address, debuggee-error>;

    /// Resolve symbol containing this address; returns demangled short form.
    /// Used for vtable-based dyn Trait resolution from a visualiser.
    resolve-symbol: func(addr: address) -> result<string, debuggee-error>;

    /// Look up the runtime type at this address (for typed debug builds).
    type-of: func(addr: address) -> result<type-id, debuggee-error>;

    /// Layout queries.
    type-name: func(ty: type-id) -> string;
    field-offset: func(ty: type-id, field-name: string) -> result<u64, debuggee-error>;
    field-type: func(ty: type-id, field-name: string) -> result<type-id, debuggee-error>;
    type-size: func(ty: type-id) -> u64;

    /// Recurse into BugStalker's own renderer for a sub-value.
    render-default: func(addr: address, ty: type-id) -> string;
}

interface visualizer {
    use debuggee.{address, type-id};

    record summary {
        line: string,                 // single-line summary, e.g. `Vec[3] = [1, 2, 3]`
        children: list<child>,        // expandable sub-nodes
        is-truncated: bool,
    }

    record child {
        name: string,                 // displayed as field/index name
        value-addr: option<address>,  // None => use override-value
        value-type: option<type-id>,  // override type
        override-value: option<string>,  // pre-rendered string
    }

    /// Does this visualiser want to handle this type?
    /// Type name passed pre-demangled.
    matches: func(type-name: string) -> bool;

    /// Produce the summary. Called per-value at render time.
    render: func(addr: address, ty: type-id, budget: u32) -> result<summary, string>;

    /// For collections: lazy iterator interface so a 10k-element Vec
    /// does not materialise 10k summaries up front.
    range: func(
        addr: address,
        ty: type-id,
        offset: u32,
        count: u32,
    ) -> result<list<child>, string>;
}

world bs-visualizer {
    import debuggee;
    export visualizer;
}
```

### Notable design decisions

- **Type identity passed as opaque `type-id`.** The host owns the type
  table; visualisers cannot fabricate types.
- **`resolve-symbol` returned as demangled string.** Visualiser authors
  do not need to depend on `rust-mangle-tree` themselves; the host
  pre-demangles. Reduces wasm module size, avoids per-visualiser
  demangler version drift.
- **`render-default` recursion hook.** A visualiser for `MySmartPtr<T>`
  delegates to BugStalker's default rendering for `T` rather than
  re-implementing every primitive.
- **`budget` passed to `render`.** Visualisers must respect a
  truncation budget the user set. Surfaces the existing `LEN_GUARD`
  policy through to crate authors.

## Discovery

Two channels, in this priority order:

1. **Embedded in the binary.** Cohabit with RFC 3191
   `#[debugger_visualizer]`. The upstream attribute supports
   `natvis_file = "..."` and `gdb_script_file = "..."`. We add
   recognition for a parallel section name `.bs_visualizer_wasm`
   that crate authors populate via:

   ```rust
   #[cfg(debug_assertions)]
   #[link_section = ".bs_visualizer_wasm"]
   #[used]
   static MY_VIZ: &[u8] = include_bytes!("my_viz.wasm");
   ```

   `bs-viz-sdk` exposes a macro hiding this:

   ```rust
   bs_viz_sdk::register_wasm!("my_viz.wasm");
   ```

   Long-term: lobby for `wasm_file = "..."` to be added to RFC 3191
   itself.

2. **Local config dir.** `~/.config/bugstalker/visualizers/*.wasm`
   for ad-hoc and forked visualisers. Loaded on debugger startup.

3. **Companion crates on crates.io** (Phase 4 follow-up). A
   visualiser-only crate `bs-viz-foo` can ship its wasm as a build
   artifact and be picked up automatically when an end-user adds it
   as a dev-dependency. See "Distribution & packaging" below for
   the four viable models and the chosen one.

Discovery order: embedded → companion crate → local config dir.
Later sources override earlier ones for the same `matches()`
predicate, with a warning.

## Distribution & packaging

The on-disk / on-binary format for a visualiser is **raw wasm
bytes** — no hex, no base64, no JSON wrapping. `include_bytes!`
reads the `.wasm` file at compile time and the linker copies it
verbatim into the chosen ELF/Mach-O section. This is the smallest
representation an embedded visualiser can have: byte-for-byte the
wasm module itself.

### Why not hex / base64?

A hex-encoded module is 2× the size, base64 is 1.33×. Either also
forces a decode pass at attach time. The custom-section approach
costs neither.

### Companion-crate distribution model

End users want `cargo add bs-viz-mymap` and have visualisers light
up without modifying the parent crate. Four candidate models, with
the chosen recommendation called out:

1. **Build-script that copies into the parent's section.** Each
   `bs-viz-*` crate's `build.rs` emits a `cargo:rustc-link-arg` to
   inject its wasm bytes into the host binary's `.bs_visualizer_
   wasm` section. Heavy — Cargo doesn't expose a clean way to
   merge multiple sources into one section, so you end up with
   N parallel sections (`.bs_visualizer_wasm.0`, `.1`, …) which
   BugStalker would have to enumerate. Fragile across linkers.
2. **`pub static` exposed by the viz crate.** The viz crate
   defines `#[link_section] pub static WASM: &[u8] = include_
   bytes!(...)` itself, *as part of normal compilation*. The wasm
   ends up in the parent binary because the linker picks up the
   static when any code references it. The viz crate ships a
   tiny `inventory`-style registration call that runs on first
   use; BugStalker scans the binary's section. **This is the
   recommended primary model** — no build-script gymnastics, no
   custom Cargo metadata, just normal Rust dependency mechanics.
3. **`package.metadata.bugstalker.visualizer = "path/to.wasm"`**
   Cargo-metadata key. BugStalker reads `Cargo.toml` (via the
   `cargo_metadata` crate at attach time) and sideloads any
   advertised wasm files. Decouples the wasm from compilation but
   makes the "where do the bytes physically live" story messier
   — visualiser authors would need to publish the wasm as part of
   the crate tarball, then BugStalker has to find the on-disk
   path of an installed crate (`~/.cargo/registry/src/...`).
   Useful as a *secondary* discovery channel for visualisers that
   don't want to be linked in.
4. **Separate registry / GitHub releases.** A central index
   (`viz.bugstalker.dev`) of visualisers, fetched on demand by
   crate name. Attractive long-term (no parent-crate dep, can be
   installed without recompilation) but is a whole secondary
   ecosystem. Out of scope for Phase 4; revisit once model 2 has
   been in the field for a release cycle.

Models 2 and 3 compose: a visualiser crate can both link itself
into the parent binary (model 2, the fast path) *and* publish its
wasm as a crate file (model 3, for the case where the parent
wasn't recompiled with the viz dep but the user still wants the
viz to apply at debug time).

### Even-more-efficient host-side caching: AOT (`.cwasm`)

The on-binary bytes stay raw wasm. Once BugStalker has loaded a
module, wasmtime can serialise the AOT-compiled artifact via
`Module::serialize`; the result is ~3× the raw wasm size but loads
in single-digit microseconds vs. ~200 µs for fresh compilation.
We cache compiled modules at:

```text
~/.cache/bugstalker/cwasm/<wasmtime-version>/<host-triple>/<wasm-sha256>.cwasm
```

Cache key includes the wasmtime version and host triple so a
wasmtime upgrade or a cross-compile invalidates cleanly. The
cache is opportunistic — first attach pays full instantiation
cost, subsequent attaches read the AOT artifact. Total Phase 4
cold-attach cost target unchanged at < 100 ms for a binary with
10 visualisers.

### Compression

Not used today. Wasm typically compresses 30–50 % under zstd, but
the section is in-binary anyway and the cost is borne once at
build time; binary-size growth from `register_wasm!` is roughly
the wasm module size, which is already small (~30–80 KB for a
typical visualiser). If real-world visualisers grow into the
megabytes, revisit by adding a `register_wasm_zstd!` macro that
stores a compressed blob and decompresses at first use.

## Capability sandbox

The wasm module is instantiated in a wasmtime `Store` configured with:

- `wasi_unstable` not exposed.
- `wasi_snapshot_preview1` not exposed.
- No filesystem, network, or environment access.
- Only the `debuggee` capability defined in WIT, backed by host
  functions that proxy through BugStalker's existing memory-read
  primitives.
- 50 MB memory cap per visualiser instance.
- 100 ms wall-clock execution budget per `render` or `range` call;
  enforced with `epoch_interruption` semantics.
- Fuel limit: 10 M instructions per call.

If a visualiser exceeds budget, BugStalker logs the offender and
falls back to default rendering for this and subsequent values.

## Type-shape predicates (avoid hashbrown trap)

The Python printers break every couple of years because they hardcode
`hashbrown::raw::RawTableInner` field paths. We mitigate via two
mechanisms:

1. **`matches()` operates on the *demangled* type name**, post-v0,
   with full generic args visible. Authors are encouraged to match
   on the public type (`std::collections::HashMap<_, _>`) rather than
   the internal layout type.
2. **Layout queries via `field-offset`/`field-type`** rather than
   hardcoded byte offsets. If the field exists with the expected
   sub-type, render proceeds. If not, return the visualiser's own
   "unsupported on this layout" string and the host falls back to
   default rendering.
3. **Version fence in visualiser metadata.** `bs-viz-sdk` exposes an
   attribute:

   ```rust
   #[bs_viz_sdk::supports(min_rustc = "1.86", max_rustc = "1.99")]
   pub fn render_my_type(/* ... */) -> Summary { /* ... */ }
   ```

   Outside the range, BugStalker skips the visualiser and warns.

## Performance

- **Cold instantiation**: ~200 µs (wasmtime AOT-compiled module).
- **Warm `render` call**: a few µs of marshalling + visualiser body.
  Dominated by the host capability calls (memory reads).
- **Per-debug-session**: instantiate each visualiser once, reuse the
  `Store` across calls. Reset the budget counters between calls.
- **Lazy iteration**: the `range` callback is invoked only when the
  user expands a collection in the TUI, never eagerly.

## Migration plan

- Hardcoded specialisations in `parse_inner_with_modifiers` remain the
  default. Wasm visualisers run *after* hardcoded ones — the host
  checks visualisers only if no built-in matches.
- Long-term: candidate hardcoded specialisations get reimplemented as
  bundled wasm modules shipped in `assets/visualizers/*.wasm`. This is
  not part of this phase; just the eventual direction.

## Reference visualisers (ship in tree)

`crates/bs-viz-examples/` — example visualisers exercising the API:

- `anyhow_error/` — render `anyhow::Error` chain with all `.source()`
  links.
- `smallvec/` — `SmallVec<[T; N]>` with inline-vs-spilled annotation.
- `indexmap/` — `IndexMap` preserving insertion order.
- `bytes_bytes/` — `bytes::Bytes` showing utf-8 probe like Phase 1's
  `Vec<u8>` treatment.
- `tracing_span/` — `tracing::span::Span` with metadata.

These also serve as documentation: SDK examples that crate authors
copy.

## Test plan

- `crates/bs-viz-host/tests/load_and_render.rs` — load each example
  visualiser, render against a fixture debuggee.
- `tests/debugger/wasm_visualizers.rs` — integration test: build a
  debuggee that includes the embedded `.bs_visualizer_wasm` section,
  attach BugStalker, verify the visualiser is loaded and used.
- Negative tests: malformed wasm, exceeded fuel, exceeded memory,
  unsupported world version.

## Acceptance criteria

- All five reference visualisers load and render correctly.
- Sandbox tests confirm the visualiser cannot read host filesystem,
  spawn processes, or escape memory budget.
- Existing BugStalker tests pass with `--features viz-wasm`.
- Build size delta: `bugstalker` binary grows by < 8 MB with feature
  enabled (wasmtime is heavy but acceptable).
- Default build (without feature) is unchanged in size.

## Effort estimate

~5 weeks engineer-time. WIT + host bindings ~1 week; sandbox + budget
enforcement ~1 week; discovery (embedded section + local dir) ~3 days;
five reference visualisers ~1 week; SDK + macros ~3 days; tests +
documentation ~1 week.

## Open questions

- **Do we ship visualisers as wasm components or core modules?**
  Components give us the WIT-typed interface for free but the
  ecosystem is still maturing. Core modules with hand-rolled bindings
  ship sooner. Recommendation: start with components; the type safety
  and forward-compat story is worth it.
- **Versioning the WIT.** Bump the `@0.1.0` to `@1.0.0` only after
  field experience. Until then, accept breaking changes.
- **Cohabitation with future RFC 3191 wasm support.** If upstream adds
  `wasm_file = "..."` to `#[debugger_visualizer]`, switch to that
  attribute and deprecate our parallel section after one release.

## DAP integration

Visualiser-rendered values (both Tier A declarative and Tier B wasm)
flow through standard DAP `variables` and `evaluate` responses — the
visualiser produces the strings, BugStalker hands them to the
client. No protocol extension needed for the common case.

Custom DAP requests cover edge cases:

- `bs/visualiserList` — enumerate active visualisers (origin: built-
  in / Tier A binary section / Tier B wasm). Useful in IDE settings UI.
- `bs/visualiserToggle` — disable a visualiser per session
  (debugging the visualiser itself).
- `bs/visualiserError` — fetch the last error from a visualiser that
  fell back. Visible in the IDE problems panel.

Tier B wasm visualisers must respect the same wall-clock budget when
serving DAP requests; a timeout returns a graceful error string.

## Specifications

- RFC 3191 — `#[debugger_visualizer]` — <https://rust-lang.github.io/rfcs/3191-debugger-visualizer.html>. The cohabitation target.
- WebAssembly Component Model — <https://component-model.bytecodealliance.org/>. Authoritative for component-level semantics.
- WIT (WebAssembly Interface Types) — <https://component-model.bytecodealliance.org/design/wit.html>. Our interface contract format.
- WebAssembly Core Specification — <https://webassembly.github.io/spec/core/>. Module structure, instructions, validation.
- WebAssembly Custom Sections — <https://webassembly.github.io/spec/core/binary/modules.html#custom-section>. The discovery channel for embedded visualisers.
- WASI preview2 — <https://github.com/WebAssembly/WASI/blob/main/preview2/README.md>. We **do not** import any WASI capability; documented for boundary clarity.
- ELF System V gABI — <https://refspecs.linuxfoundation.org/elf/gabi4+/contents.html>. `link_section` semantics, `SHF_ALLOC` flag use.
- Mach-O `loader.h` — <https://github.com/apple-oss-distributions/dyld/blob/main/include/mach-o/loader.h>. 16-character section name limit, segment/section pairs.
- wasmtime API — <https://docs.rs/wasmtime/latest/wasmtime/>. Engine epoch interruption, fuel, store-level memory limits.
- LLDB data formatters reference — <https://lldb.llvm.org/use/variable.html>. Behavioural reference for tier comparison.
- GDB pretty-printing API — <https://sourceware.org/gdb/onlinedocs/gdb/Pretty-Printing-API.html>. Behavioural reference.
- serde — <https://serde.rs/>. Encoding format for `TypeViewSpec` (we use `bincode` or `postcard` at runtime; spec format versioned).

## Invariants

Checks span three layers: spec-section parsing (reading the `.bs_viz_spec` binary section),
wasm sandbox lifecycle (instantiation, fuel, memory), and visualiser invocation (WIT contract,
child node construction, lazy iteration). All `debug_assert!` calls below guard against
mistakes in BugStalker's own implementation — they are not the mechanism for catching
misbehaving third-party modules.

```rust
// Spec section header magic is exact.
debug_assert_eq!(spec_section.magic, *b"BS\0\1");

// Each spec entry's payload is in-bounds.
debug_assert!(spec_offset + spec_len <= spec_section.len());

// Wasm fuel is decremented, never incremented during a call.
debug_assert!(fuel_after <= fuel_before);
debug_assert!(fuel_after > 0 || trapped);

// Memory cap respected.
debug_assert!(instance.memory.size_bytes() <= MAX_VIZ_MEMORY);

// WIT version exact match between host bindings and module export.
debug_assert_eq!(host.wit_version(), module.wit_version());

// Child rendering produces exactly one source.
debug_assert!(child.value_addr.is_some() ^ child.override_value.is_some());

// `matches()` predicates are pure: same input, same output.
debug_assert_eq!(matches(ty1), matches(ty1));

// Lazy iterator window stays within collection bounds.
debug_assert!(offset + count <= total_len);

// Visualiser execution wall-clock budget honoured.
debug_assert!(elapsed <= EXEC_BUDGET);
```

Trust boundary: first-party in-binary specs (Tier A) are trusted — `debug_assert!` here
catches our own implementation bugs. Third-party wasm modules (Tier B) are explicitly
untrusted — the equivalent checks become hard `Err` returns from host functions, not
asserts. Asserts catch our bugs; the sandbox catches theirs.

# Phase 7 — Linker–debugger contract (Wild integration)

A linker sees the entire program. It already builds indices the
debugger has to rebuild from scratch at attach time — vtables,
monomorphisations, section coalescing. If the linker emits those
indices into accelerator sections, BugStalker reads them in O(log n)
instead of scanning all symbols. With Wild (the user's linker
supporting Linux/Mac/wasm), the contract is achievable as a co-design.

The architectural rule:

> **Accelerator sections are additive, never required.** BugStalker
> works with `lld`, `mold`, `gold`, and any other linker. Wild produces
> faster startup and richer features; other linkers fall back to the
> existing scan-and-build paths.

## What the linker can offer

| Section | Contents | Consumer | Speedup |
| --------------------- | --------------------------------------- | -------------------- | ------- |
| `.bs_buildid` | 256-bit content hash of the binary | All caches | n/a |
| `.bs_viz_index` | sorted `[(type_name_hash → spec_offset)]` over `.bs_viz_spec` | Phase 4 Tier A | 100× lookup |
| `.bs_vtables` | sorted `[(vtable_addr → impl_self_type, trait, methods)]` | Phase 3 dyn Trait | 1000× lookup |
| `.bs_monos` | `[(generic_path_hash → DIE offset)]` | Phase 3 type recovery | 10× |
| `.bs_coroutines` | `[(coroutine_type_id → [(variant_idx → file:line:col)])]` | Phase 3 async | enables column info |
| `.bs_paths` | path-prefix table for source files | All phases | smaller binaries, relocatable debug |
| `.bs_drop_glue` | sorted `[(drop_fn_addr → type_id)]` | Phase 5 attribution | profile readability |
| `.bs_inline_tree` | flattened inline-call tree | Phase 5 inlined-frame attribution | exact attribution |

Each is a sorted-by-key array with a small fixed header (magic,
version, count, alignment). BugStalker memory-maps the section and
binary-searches.

## Section format

All sections share a common header to keep parsing trivial:

```text
struct BsSectionHeader {
    magic: [u8; 4],     // "BS\0\1"
    section_kind: u32,  // VIZ_INDEX | VTABLES | MONOS | ...
    version: u16,       // bump on incompatible change
    flags: u16,
    count: u32,
    entry_size: u32,
    // followed by `count * entry_size` sorted records
}
```

Records are fixed-size and aligned for direct memory access.
Endianness matches the target. All offsets are relative to the
containing section base.

### `.bs_viz_index` example

```rust
struct VizIndexEntry {
    type_name_hash: u64,   // FxHash of the v0-demangled type name
    spec_offset:    u32,   // offset into .bs_viz_spec
    spec_len:       u32,
}
```

BugStalker computes the hash of the type it is rendering, binary-
searches `.bs_viz_index`, and reads `spec_len` bytes from the indexed
offset. No section scan. Cold-attach lookup goes from O(N specs) to
O(log N).

### `.bs_vtables` example

```rust
struct VtableEntry {
    vtable_addr:    u64,   // runtime address (post-relocation)
    impl_self_die:  u32,   // .debug_info offset for the concrete type
    trait_die:      u32,   // .debug_info offset for the trait
    method_count:   u16,
    flags:          u16,
}
```

Phase 3's headline feature — `dyn Trait` concrete-type recovery —
walks from a fat-pointer's vtable to a concrete type. Without this
section, BugStalker resolves the symbol at the vtable address, runs
`rust-mangle-tree::parse`, and maps the demangled type name to a DIE
via a hash map built at attach time. That map costs O(N symbols) to
build. With this section, the lookup is O(log V) where V is vtable
count, no demangling on the hot path.

### `.bs_coroutines` — the column-info win

DWARF emits `DW_AT_decl_file` and `DW_AT_decl_line` on coroutine
variant fields, but `DW_AT_decl_column` is missing. The linker has
access to the same information the compiler writes elsewhere; if Wild
preserves column data through compilation (via a separate index file
emitted by `rustc -Cextra-debug-info=column-table`), it can fill in
the column for each `.await` point. Phase 3 async await-trace becomes
column-precise rather than line-precise.

This requires a small `rustc` change: emit the column table.
Coordinate upstream; not a Wild-internal feature.

## Why a custom section, not an extended DWARF tag

DWARF is the right place for *type and layout* information — and
that's where rustc already puts it. These accelerator sections carry
*derived* data: indices, sorted lookup tables, link-time aggregations.
Putting them under a custom prefix (`.bs_*`) avoids:

- DWARF consumers (lldb/gdb) trying to parse them and getting
  confused.
- Vendor lock-in: another debugger could adopt the same `.bs_*`
  format without touching the DWARF spec.
- ABI churn: bumping our section's version doesn't perturb the DWARF
  parser path.

The sections are pure data, no executable content, so they survive
`strip --strip-debug` only if explicitly preserved. We use
`SHF_ALLOC = 0` to mark them as non-loaded debug sections; standard
strip removes them.

## Coordination model

Wild is a separate project with its own release cadence. Two viable
coordination models:

### Option A — Wild emits speculatively

Wild always emits the `.bs_*` sections when it sees the matching
input data. BugStalker (and any other consumer) opts in by reading
them. Risk: section format churn during early development is borne
by both projects.

### Option B — Opt-in flag in Wild

`wild --emit-bugstalker-sections` (or env var `WILD_EMIT_BS=1`) gates
emission. BugStalker docs recommend setting this in `RUSTFLAGS`.
Lower coupling, slower adoption.

Recommendation: **Option A** during development, behind a Wild build
feature `bugstalker-accelerators`. Move to Option B (always-on, gated
by section format version) once stable.

## Wild internal data already available

The user is best-placed to confirm, but conceptually Wild already
computes:

- **Symbol resolution and relocation** — produces the vtable address
  table for free as a side-effect of relocating each vtable's
  pointer.
- **Section coalescing** for `.init_array` / `.fini_array` and
  similar — same machinery applies to `.bs_viz_spec` deduplication.
- **Path table for `.debug_line`** — already built; can be re-emitted
  in our compact form.
- **Cross-section relocations** — needed for `.bs_vtables` to point
  at DIE offsets.

What might require new code in Wild:

- v0 demangling of vtable symbols to extract self-type and trait.
  Wild can either embed `rustc-demangle` (or our `rust-mangle-tree`
  once it lands) or defer to a post-link step.
- Sorting accelerator sections by key (current linkers usually
  don't sort sections by content).
- Preserving `rustc`-emitted column tables through linkage if/when
  rustc emits them.

## Specifying the format

Where the format spec lives matters:

- **Inside BugStalker** (`crates/bs-section-format/`): we author and
  version it. Wild and any other linker reads our spec.
- **Inside Wild**: the linker owns the format; debuggers consume.
- **Joint repo**: separate `bs-debug-sections` crate published to
  crates.io, depended on by both projects.

Recommendation: **joint crate**. Versioned, semver-tracked, no single
project owns the format. Both Wild and BugStalker depend on it as a
build dependency for emitting/parsing.

## Cross-platform details

- **ELF (Linux)**: `.bs_*` sections with `SHT_PROGBITS`,
  `SHF_ALLOC = 0`. Standard.
- **Mach-O (Darwin)**: `__BS,__viz_index` segment/section pairs.
  Mach-O sections are limited to 16-char names, so use abbreviated
  forms: `__viz_idx`, `__vtables`, `__coroutines`.
- **WebAssembly**: WASM custom sections (`bs.viz_index`,
  `bs.vtables`, etc.). Custom sections survive standard stripping.
  This is the route to BugStalker eventually debugging wasm targets.
- **PE/COFF (Windows)**: not on our roadmap, but the format would
  use `.bs_*` sections similarly.

The section parser handles all four; only the emission side differs
per object format. Wild's tri-platform support (Linux/Mac/wasm)
matches our needs exactly.

## Wasm support — the long view

Custom sections in WebAssembly are first-class. If Wild emits the
accelerator sections in wasm objects, and a future BugStalker phase
adds wasm-target debugging (separate project, not in this manifesto's
current scope), the accelerator sections work uniformly.

This is speculation, not a commitment. But the format being
wasm-compatible from day one preserves the option.

## Implementation order

1. **Define the spec.** Joint crate `bs-debug-sections` with header
   layout, kind enum, version. Publish 0.1.0 to crates.io.
2. **BugStalker parser.** New module
   `src/debugger/debugee/bs_sections.rs` reading whichever sections
   are present, falling back to existing scan paths when absent.
3. **Wild emission for `.bs_buildid`.** Smallest section, validates
   the integration end-to-end. Trivial implementation in Wild.
4. **Wild emission for `.bs_viz_index`** alongside Phase 4's
   `.bs_viz_spec`. Lock the binding pattern.
5. **Wild emission for `.bs_vtables`.** Highest-leverage section;
   unlocks Phase 3 fast path.
6. **Remaining sections** in priority order:
   `.bs_monos` → `.bs_coroutines` → `.bs_paths` →
   `.bs_drop_glue` → `.bs_inline_tree`.

Each section can ship independently. BugStalker checks for the
section header and decides at runtime whether to use the fast or
slow path.

## Test plan

- `tests/sections/format_roundtrip.rs` — Wild emits, BugStalker
  parses, content matches.
- `tests/sections/missing_sections.rs` — strip the accelerator
  sections, BugStalker falls back gracefully.
- `tests/sections/version_skew.rs` — older debugger, newer linker:
  ignore unknown sections; older linker, newer debugger: detect
  version mismatch, log info, fall back.
- `tests/perf/cold_attach.rs` — measure cold-attach time on a
  large binary (e.g. `bugstalker` itself) with and without
  accelerator sections. Target: ≥ 5× speedup with sections present.

## Acceptance criteria

- `bs-debug-sections` crate published with stable 0.1 spec.
- Wild emits at least `.bs_buildid` and `.bs_viz_index` behind its
  feature flag.
- BugStalker reads both, falls back when absent.
- Cold-attach time on a 100 MB Rust binary improves by ≥ 3× with
  Wild + accelerators vs lld + scan paths.
- All existing tests pass with both linkers in CI matrix.

## Effort estimate

| Item | Effort | Side |
| ----------------------------------------- | -------- | ---------- |
| Spec authoring + crate publish | 1 week | joint |
| BugStalker parser + fallback | 1 week | BugStalker |
| Wild emission: buildid, viz_index | 3 days | Wild |
| Wild emission: vtables | 2 weeks | Wild |
| Wild emission: monos, coroutines, paths | 3 weeks | Wild |
| Cross-platform (Mach-O, wasm) | 2 weeks | Wild |
| Tests, docs, CI matrix | 1 week | both |

Total: ~10 weeks engineer-time across both projects.

## Risks

- **Spec instability.** Solution: aggressive 0.x versioning; both
  projects pin exact versions; bump together.
- **Wild adoption gap.** Some users will use lld/mold; ensure the
  fallback path is always tested in CI.
- **Section bloat.** Accelerator sections shouldn't dominate binary
  size. Target: < 5 % total binary growth. Monitor in CI.
- **`rustc` column-table dependency for `.bs_coroutines`.** If rustc
  doesn't emit columns, that section ships line-only and we file
  upstream. Not a blocker.
- **Mach-O 16-char name limit.** Confirmed workable; document the
  abbreviated names in the spec.
- **Stripping semantics differ.** Linux `strip` drops non-`SHF_ALLOC`
  by default; Mach-O `strip` is more aggressive. Test on both;
  document.

## Specifications

- ELF System V gABI — <https://refspecs.linuxfoundation.org/elf/gabi4+/contents.html>.
  `Elf64_Shdr`, section flags (`SHF_ALLOC`, `SHF_WRITE`, `SHF_EXECINSTR`).
- ELF specification — <https://refspecs.linuxfoundation.org/elf/elf.pdf>.
  Foundational document.
- Mach-O `loader.h` — <https://github.com/apple-oss-distributions/dyld/blob/main/include/mach-o/loader.h>.
  `struct section_64`, segment/section pairs, 16-char name limit.
- WebAssembly Custom Sections — <https://webassembly.github.io/spec/core/binary/modules.html#custom-section>.
  Names like `bs.viz_index` survive standard stripping.
- WebAssembly Core Specification — <https://webassembly.github.io/spec/core/>.
  Module structure.
- DWARF 5 — <https://dwarfstd.org/doc/DWARF5.pdf>.
  `DW_AT_linkage_name`, DIE offsets used as our `.bs_vtables` cross-references.
- BLAKE3 specification — <https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/blake3.pdf>.
  Build-id hash recommendation.
- FxHash (rustc-hash) — <https://github.com/rust-lang/rustc-hash>.
  Type-name hash for `.bs_viz_index` lookup; non-cryptographic but consistent.
- `gimli-rs/object` — <https://github.com/gimli-rs/object>.
  The crate we use to read all four object formats uniformly.
- The Wild linker repository — (user's project). Source of truth for emission semantics.
- LLVM `.note.gnu.build-id` reference — <https://github.com/llvm/llvm-project/blob/main/llvm/docs/build-id.rst>.
  Existing convention we mirror.
- Cargo build artifacts — <https://doc.rust-lang.org/cargo/reference/build-cache.html>.
  Where the linker runs in the toolchain.
- DWARF 5 split debug info (`.dwo`/`.dwp`) — <https://dwarfstd.org/doc/DWARF5.pdf> §7.3.
  For our open question on accelerator placement under split debug.

## Invariants

These are the parser's preconditions; the section format invariants below are
checked on the reader side. Emission-side invariants belong in Wild's own test
plan.

```rust
// Header magic exact.
debug_assert_eq!(header.magic, *b"BS\0\1");

// Entry size matches the kind.
debug_assert_eq!(header.entry_size, expected_size_for_kind(header.section_kind));

// Records are sorted by primary key (binary-search precondition).
debug_assert!(records.windows(2).all(|w| w[0].sort_key() <= w[1].sort_key()),
    "section {:?} not sorted", header.section_kind);

// Section is well-formed: header + count*entry_size = section length.
debug_assert_eq!(
    size_of::<BsSectionHeader>() + (header.count as usize) * (header.entry_size as usize),
    section.len()
);

// Vtable address is in a mapped executable region.
debug_assert!(self.is_mapped_executable(entry.vtable_addr));

// Version backward compatibility.
debug_assert!(header.version <= MAX_SUPPORTED_VERSION);

// Build-id is exactly 32 bytes (256-bit BLAKE3 truncated, or full).
debug_assert_eq!(buildid_section.len(), 32);

// Path-prefix table indices in bounds.
debug_assert!(path_index < self.path_table.len());

// Cross-section relocation: DIE offsets fall within `.debug_info` size.
debug_assert!(entry.impl_self_die < debug_info_size);
debug_assert!(entry.trait_die < debug_info_size);

// Fallback path: when section absent, scanner produces same answer.
#[cfg(debug_assertions)]
{
    if let Some(slow) = self.fallback_lookup(addr) {
        let fast = self.fast_lookup(addr).unwrap();
        debug_assert_eq!(slow, fast,
            "linker accelerator disagrees with scanner for {:#x}", addr);
    }
}
```

The fast-vs-slow-path consistency check is the most important invariant — it
catches bugs in *either* the linker's emission or our reader, in debug builds,
with both paths running. Release builds run only the fast path.

## Non-goals

- Replacing DWARF. We add accelerators *on top of* DWARF; DWARF
  remains the source of truth for layout.
- Cross-binary linking metadata. Single-binary scope.
- Linker-injected runtime code. Sections are pure data.

## Open questions

- **Hash function choice.** FxHash is fast but not cryptographic.
  For type-name lookup, that's fine. For build-id, we want a
  collision-resistant hash (SHA-256 truncated to 256 bits, or
  BLAKE3 which Wild might already depend on).
- **Section naming.** `.bs_*` is short and recognisable but might
  clash with future spec extensions. Consider `.debug_bs_*` to live
  in the existing debug-section namespace, at the cost of being
  preserved by `strip --only-keep-debug`.
- **`.dwo` / `.dwp` integration.** When using split debug info, do
  the accelerator sections live in the main binary or the `.dwp`?
  Vtables need the main binary (runtime addresses); type indices
  could live in either.

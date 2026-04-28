# Phase 1 — Stdlib coverage

> **Phase 1 status:** **PHASE 1 COMPLETE — batches A through R landed.**
> Batch A: F1 (reference_type), S3 (atomics), S11 (NonNull).
> Batch B: S6 (Range family), S7 (Pin).
> Batch C: S4 (Duration), S5 (SystemTime/Instant render upgrade),
> plus rustdoc fix in `darwin_mach.rs`.
> Batch D: S12 (CString — utf-8 probe with hex-preview fallback,
> bounded 64 KiB read).
> Batch E: S16 (`Vec<u8>` / `VecDeque<u8>` utf-8 probe with
> 16-bytes-per-row hex-dump fallback).
> Batch F: S13 (OsString) and S14 (PathBuf) — peel through the
> wrapper chain to surface the underlying `Vec<u8>` bytes.
> Batch G: S10 (MaybeUninit) — Union-dispatch peel through the
> `value` arm and the transparent `ManuallyDrop` wrapper.
> Batch H: S1 (minimum viable) — `Mutex<T>` / `RwLock<T>` peeled to
> the inner T (data field). Lock-state badges deferred. S8 (Cow
> promotion) investigated and **permanently dropped** — Cow already
> renders correctly via the default RustEnum path so promotion
> would be a no-op.
> Batch I: S2 — `MutexGuard<T>` / `RwLockReadGuard<T>` /
> `RwLockWriteGuard<T>` peeled to the guarded T. Both libstd
> layouts handled (read-guard's `data: NonNull<T>` and the
> write/mutex-guard's `lock: &…` reference).
> Batch J: S9 — `Box<T>` smart-deref via a new `dereffed` field on
> `PointerValue` populated at parse time; the renderer returns
> `Wrapped(inner)` so the pointee shows inline.
> Batch K: S15 — `Weak<T>` reports `(strong=N, weak=M)` plus
> `[dropped]` when `strong == 0`; reads the counts by deref'ing
> the inner allocation pointer to read `RcBox` / `ArcInner`.
> Batch L: F3 — `RenderBudget` API + `(… N more elided)` trailers
> on `Vec`/`String`/`&str` when the length exceeds `LEN_GUARD`.
> Batch M: F3 follow-up — same elision surfacing for `HashMap`,
> `HashSet`, `BTreeMap`, `BTreeSet`.
> Batch N: S1 `[poisoned]` badge — `Mutex` / `RwLock` surface a
> `[poisoned]` trailer when the poison flag is set.
> Batch O: S12/S13/S14 DST companions — `&CStr`, `&OsStr`,
> `&Path` route through the existing CString/OsString parsers
> (rustc materialises them as `{data_ptr, length}` structs).
> Batch P: F4 (minimum-viable cut) — `/x`, `/b`, `/o`, `/d`,
> `/iso` GDB-style format specs on `var`/`argd` commands.
> Batch Q: S1 `[locked]` badge — futex backend on
> Linux/Windows/BSD; macOS pthread reports false.
> Batch R: F4 byte-slice overrides — `/utf8` (lossy utf-8 force)
> and `/hex` (hex-dump force) on `Vec<u8>` / `VecDeque<u8>` /
> `[u8; N]`. Public `ByteRenderMode` enum + `render_byte_slice_members`
> entrypoint added for future `&[u8]` callers.
> No remaining items. F2 (`char` placeholder) is upstream-tracked
> (rust-lang/rust#113819) and ships when that issue closes.
> F4 specs `/p`, `/c`, `/s`, `/y`, `/[N]`, `/[N..M]` deferred —
> slice indexing in particular probably belongs in `Dqe` rather
> than as a print-time format spec.

Plug the obvious holes the rustc Python printers also leave, plus the
latent `DW_TAG_reference_type` fall-through. No new architecture.
Everything bolts into the existing dispatch in
`src/debugger/variable/value/parser.rs:411–691` and the
`SpecializedValue` family under
`src/debugger/variable/value/specialization/`.

## Goals

- Parity with `src/etc/{lldb,gdb}_providers.py` for every stdlib type
  they cover.
- Beyond-parity prettification for time, range, smart pointer, byte-slice
  types nobody handles well.
- Format-spec syntax in the `print`/`var`/`watch` commands.

## Non-goals

- Anything requiring v0 demangling (Phase 3).
- User-extensible visualisers (Phase 4).
- Anything async or runtime-introspection (Phase 3+).

## Bug fixes that ship with this phase

### F1. `DW_TAG_reference_type` fall-through

`src/debugger/debugee/dwarf/type.rs:620–635` dispatches on tag and emits
`warn!("unsupported type die: {tag}")` for `DW_TAG_reference_type` and
`DW_TAG_rvalue_reference_type`. Treat both as pointer-shaped: same
layout, same rendering as `DW_TAG_pointer_type` with reference flavour
preserved on `TypeDeclaration::Pointer` so the displayed type is `&T` /
`&mut T` rather than `*T`.

Test: add a debugee that takes a `&T` parameter and verify the rendered
type string and value.

### F2. `char` placeholder hack

`parser.rs:187` substitutes `'?'` for invalid `char` bit patterns
(`WAITFORFIX` for rust-lang/rust#113819). When the upstream issue
closes, drop the workaround. Track in CHANGELOG.

### F3. Hard `LEN_GUARD` / `CAP_GUARD`

`src/debugger/variable/specialization/mod.rs:40` hard-codes 10 000.
Move to a configurable `RenderBudget` with sensible defaults; surface
truncation explicitly in the rendered output (`… 9234 more elided`).

## New specialisations

Each entry: detection rule, layout reference, rendered form, edge cases.

### S1. `Mutex<T>` / `RwLock<T>`

- Detect by name `"Mutex"` / `"RwLock"` in namespace `std::sync::*` or
  `parking_lot::*` (latter behind a probe — different layout).
- Layout: peel `UnsafeCell<T>` from the `data` field.
- Render: the inner `T` directly with a `[locked]` badge if the OS
  primitive's lock count is non-zero, plus a `[poisoned]` badge if
  the poison flag is set.
- Edge case: do not attempt to *acquire* the lock. Read the inner
  `UnsafeCell<T>` payload regardless. This may show torn state mid-write
  on another thread — annotate the badge accordingly.

### S2. `MutexGuard<T>` / `RwLockReadGuard<T>` / `RwLockWriteGuard<T>`

- Render the guarded `T` transparently, with a `[guard]` annotation.
- Pull the parent `Mutex`/`RwLock` address through the guard's pointer
  field for cross-reference.

### S3. Atomics

Covers `AtomicI8` through `AtomicI128`, `AtomicBool`, `AtomicUsize`, and
`AtomicPtr<T>`.

- Detect by name prefix `"Atomic"` in namespace
  `core::sync::atomic` / `std::sync::atomic`.
- Layout: single `UnsafeCell<scalar>` field.
- Render as the bare scalar (or `*T` for `AtomicPtr<T>`).
- Existing test at `tests/debugger/variables.rs:2101` confirms current
  raw-`UnsafeCell` rendering — update assertions when this lands.

### S4. `Duration`

- Detect by name `"Duration"` in namespace `core::time` / `std::time`.
- Layout: `secs: u64, nanos: u32`.
- Render: `1h 1m 1.500s` style for whole-second values; sub-second
  values use the most readable unit (`1.500ms` / `250µs` / `7ns`).
  ISO-8601 duration form (`PT1H1M1.5S`) is **deferred to batch D
  alongside F4** — it needs format-spec syntax (`:iso`) to invoke.
- Edge case: `secs == 0 && nanos == 0` → `0s` not `0h 0m 0s`.

### S5. `SystemTime` / `Instant`

- Already specialised (`SpecializedValue::SystemTime`,
  `SpecializedValue::Instant`). Switch default rendering:
  `SystemTime` → ISO-8601 UTC timestamp; `Instant` → `+Δs from program
  start` if start instant is recoverable from a TLS or process-wide
  symbol, else absolute monotonic-clock value.

### S6. `Range<T>` / `RangeInclusive<T>` / `RangeFrom<T>` / `RangeTo<T>`

- Detect by name in `core::ops::range`.
- Render: `start..end`, `start..=end`, `start..`, `..end`, `..=end`,
  `..` (`RangeFull`).
- For `RangeInclusive`, watch out for the private `exhausted: bool`
  field; render `[exhausted]` annotation when set.

### S7. `Pin<P>`

- Detect by name `"Pin"` in `core::pin`.
- Render the inner pinnee `P` transparently. The wrapper type
  identity (`Pin<&mut T>`, `Pin<Box<T>>`) is preserved on the value's
  rendered type — that already conveys "this is pinned", so no
  separate text annotation is added (mirrors the S11 decision).
- Critical for async support in Phase 3.

### S8. `Cow<'a, B>`

- Already a `RustEnum` — promote to specialised display.
- Render: `Borrowed(&...)` or `Owned(...)`, recursing into the inner
  value with normal rendering.

### S9. `Box<T>`

- Detect by name `"Box"` in `alloc::boxed`.
- Render: smart-unwrap deref. Show pointee's type and value, with a
  `Box<T> @ 0x…` line that the user can expand to see the raw pointer.
- Edge case: trait-object box `Box<dyn Trait>` defers to Phase 3
  (vtable resolution). For now, render as today (fat pointer struct).

### S10. `MaybeUninit<T>`

- Layout: `union { value: ManuallyDrop<T>, uninit: () }`.
- Render: cannot know if initialised. Always render the `value` arm
  with a `[possibly uninit]` warning. Add format-spec `:bytes` to
  force raw byte view.

### S11. `NonNull<T>`

- Detect by name `"NonNull"` in `core::ptr::non_null`.
- Render as a plain `*T` value. (Earlier drafts proposed a
  `[non-null]` verbose-mode annotation; dropped because the wrapper
  type identity already says `NonNull<T>`, so the badge would be
  redundant.)

### S12. `CString` / `CStr`

- Layout: `CString` is `Box<[u8]>` whose stored length includes the
  trailing NUL byte; `CStr` is `[c_char]` (DST), accessed via a fat
  `&CStr` pointer.
- Render (CString — **landed in batch D**): the trailing NUL is
  stripped, then the remaining bytes are utf-8 decoded. Valid utf-8 →
  `c"hello"`. Invalid utf-8 → hex preview `c"\xNN\xNN …"` (capped at
  32 bytes, a trailing `…` appended when more remain).
- **CStr deferred to batch E** — the DST + fat-pointer path needs a
  different dispatcher hook than the Structure-named CString.
- Bound length probing at 64 KiB to avoid runaway reads on corrupted
  pointers (`MAX_READ` const, after the existing `guard_len`
  10 K-element clamp).

### S13. `OsString` / `OsStr`

- Layout is platform-dependent (`Wtf8Buf` on Windows, `Vec<u8>` on
  unix).
- Render utf-8 if valid; else hex preview. Preserve the path-separator
  vs raw distinction for `Path`/`PathBuf` (S14).

### S14. `PathBuf` / `Path`

- Delegate to S13 internally; render as a quoted path with platform
  separator.

### S15. `Weak<T>` (rc / sync)

- Detect by name `"Weak"` in `alloc::rc` / `alloc::sync`.
- Render: `Weak @ 0x… (strong=N, weak=M)` and resolve inner only on
  expand. If `strong == 0`, mark `[dropped]`.

### S16. `Vec<u8>` / `&[u8]` / `[u8; N]` byte-aware rendering

- When element type is `u8`, run a utf-8 probe over the first 1 KiB.
- If valid utf-8 → render as quoted string with `b"..."` byte-string
  syntax.
- If not → 16-bytes-per-row hex dump with ASCII column.
- Either form behind format-spec override (`:hex`, `:utf8`, `:dec`).

## F4. Format-spec syntax

Add a colon-suffix grammar to the `print`/`var`/`watch`/`argd` commands,
parsed in `src/ui/command/`.

| Spec | Effect |
| ----- | ------------------------------ |
| `:x` | hex for integers |
| `:b` | binary for integers |
| `:o` | octal for integers |
| `:d` | force decimal |
| `:p` | render as pointer |
| `:c` | reinterpret integer as `char` |
| `:s` | reinterpret pointer as C-string |
| `:y` | raw bytes (hex dump) |
| `:utf8` | force utf-8 string interpretation |
| `:hex` | force hex-dump rendering |
| `:iso` | ISO-8601 form (Duration/SystemTime/Instant) |
| `:[N]` | reinterpret pointer as `[T; N]` |
| `:[N..M]` | slice indexing for collections |

Specs compose with the path: `print myvec[3].field:x`.

## Test plan

`tests/debugger/variables.rs` — add functions:

- `test_read_mutex_rwlock`
- `test_read_atomics_rendered_as_scalar` (replaces existing
  `test_read_atomic` assertions)
- `test_read_duration_systemtime_instant_human`
- `test_read_ranges`
- `test_read_pin_cow_box`
- `test_read_maybe_uninit_nonnull`
- `test_read_cstring_osstring_pathbuf`
- `test_read_weak`
- `test_read_byte_slice_utf8_probe`
- `test_format_spec_hex_bin_oct`
- `test_format_spec_array_index`
- `test_dw_tag_reference_type` (covers F1)

Each test runs a debugee binary (existing pattern in
`tests/hello_world/` style), sets a breakpoint, evaluates the variable,
and asserts on rendered output.

## Documentation impact

- `README.md` — extend the value-rendering examples.
- `CHANGELOG.md` — list each new specialisation under Added.
- `doc/ROADMAP.md` — strike Tier 1 items as they land.
- Doc-sync subagent runs on each merge to this phase per CLAUDE.md
  policy.

## Acceptance criteria

- All new tests pass on Linux and macOS.
- Existing tests continue to pass after assertion updates.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Manual smoke test: open a real-world crate (e.g. `ripgrep`), break
  in `main`, render every common stdlib type — none falls back to
  raw struct rendering.

## Effort estimate

~2 weeks engineer-time. Each specialisation is small; the bulk is test
authoring and verifying multi-version layouts (rustc 1.81 → current).

## DAP integration

Format-spec syntax flows through DAP `evaluate` requests transparently:
the colon-suffix grammar (`:x`, `:hex`, `:[N]`, etc.) parses on
BugStalker's side, the result populates the standard
`EvaluateResponse.result` string. No protocol extension needed.

Renamed and re-shaped values (e.g. `Mutex<T>` → `T [locked]`) appear
in `VariablesResponse.variables[].value`. `[locked]`, `[poisoned]`,
`[exhausted]`, `[possibly uninit]` etc. are part of the value string;
DAP clients render them inline.

`bs/setRenderBudget` — custom request to override `LEN_GUARD` per
session. Surface in VSCode as a setting.

## Specifications

- **DWARF Debugging Information Format Version 5** — <https://dwarfstd.org/doc/DWARF5.pdf>.
  §5.7.10 (`DW_TAG_variant_part`, `DW_AT_discr`), §6.2 (`.debug_line` table).

- **The Rust Reference, type layout** — <https://doc.rust-lang.org/reference/type-layout.html>.
  Authoritative for `repr(Rust)`, `repr(C)`, niche optimisation rules.

- **The Rustonomicon** — <https://doc.rust-lang.org/nomicon/>.
  Cell, RefCell, exotic sizes, transmutation rules.

- **The Rust standard library API docs** — <https://doc.rust-lang.org/std/> —
  per-type permalinks for `Mutex`, `RwLock`, `Atomic*`, `Duration`, `SystemTime`,
  `Instant`, `Range*`, `Pin`, `Cow`, `Box`, `MaybeUninit`, `NonNull`, `CString`,
  `OsString`, `PathBuf`.

- **IEEE 754-2019** — float NaN bit patterns and special-value handling.

- **ISO 8601-1:2019** — date/time formatting.

- **rust-lang/rust#113819** — the open `char` validation issue our `parser.rs:187`
  workaround tracks.

- **rust-lang/rust#125147** — `DW_AT_discr_value` `DW_FORM_data*` bug we work
  around for negative discriminants.

- **rust-lang/rust#62839** — niche-encoded enum DWARF ambiguity.

## Invariants

All listed checks are encoded as `debug_assert!` at the relevant call sites;
release builds compile them out. Failures point to either incoming-data corruption
or a bug in our renderer.

```rust
// Renderer never exceeds the user's byte budget.
debug_assert!(rendered.len() <= budget,
    "renderer for {} produced {} bytes, budget {}", type_name, rendered.len(), budget);

// Truncation always announces itself.
debug_assert!(!truncated || trailer.contains("more elided"));

// Atomic specialisation reads exactly the inner scalar's width.
debug_assert_eq!(atomic.byte_width, scalar.byte_width);

// Niche-encoded Option<NonNull<_>> / Option<Box<_>> / Option<&_>:
// None iff the inner pointer is null.
debug_assert_eq!(is_none, inner_ptr_bytes.iter().all(|b| *b == 0));

// Render-tree depth bounded; configurable cap.
debug_assert!(self.depth <= MAX_RENDER_DEPTH);

// Cycle detection: visited set monotonic during a render-tree walk.
debug_assert!(self.visited.len() >= visited_at_entry);

// Format-spec parsed at parse time, applied here only if applicable.
debug_assert!(format_spec.applies_to(value_type));

// Mutex peek does not change observable lock state.
debug_assert_eq!(lock_state_before, lock_state_after);
```

Integration tests in `tests/debugger/variables.rs` cover the positive case for
each invariant; the asserts catch the negative.

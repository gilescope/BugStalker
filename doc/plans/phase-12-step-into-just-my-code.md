<!-- markdownlint-disable MD013 -->
# Phase 12 — Step-Into "Just My Code"

A normal Step-In should land in *your* code, not in the guts of
`core::`, `alloc::`, `hashbrown`, `tokio`, or whatever dependency the
line happens to call. Stepping into `println!` and watching twelve
frames of formatting machinery go by is the single most common waste of
a stepping session. .NET solves this with **Just My Code**; this is the
same idea for BugStalker.

## The rule

> Step-In stops at the next line of *user* code. Library frames are
> transparently stepped through, never stopped in — unless the user asks
> for the raw behaviour with a modifier.

## Behaviour

Two bindings, mapped exactly onto the keys already in muscle memory:

- **`alt+right`** — Step-In, **skip libraries** (the new default). Steps
  through any frame outside your crate(s) and stops at the next user
  line. This *replaces* the binding currently on plain Step-In.
- **`shift+alt+right`** — Step-In, **any frame** (today's behaviour):
  descend into whatever the line calls, library or not.

Semantics of "skip libraries":

- If the call on the current line leads to user code (directly, or via a
  user closure the library invokes — e.g. `iter.map(|x| ...)`,
  `sort_by(...)`), stop at that user line.
- If the call is *entirely* library (no user frame is entered before it
  returns), the net effect is a Step-Over: stop on the line after the
  call, back in the current frame.
- Never stop with the PC inside a library frame.

## What counts as "my code"

A frame is **user code** when the source file backing its PC resolves to
a path under the workspace, and **library** otherwise. Detection is
path-based, cheap, and needs no new debug-info:

- Library if the decl/source path contains any of: `/registry/`
  (cargo deps), `/.cargo/`, `/.rustup/`, `/toolchains/`, `/rustc/`, or
  the resolved sysroot prefix (`rustc --print sysroot`).
- Library if the PC has no line info at all (PLT stubs, stripped/FFI
  frames) — there's nothing for the user to read there.
- Otherwise user code.

Refinements (later): an explicit allow-list / deny-list
(`bugstalker.stepInto.alsoMine = ["my_vendored_dep"]`,
`...skip = ["proc_macro_helper"]`), and a "workspace root" override for
non-cargo layouts. v0 uses the path heuristic only.

This reuses the source-resolution BugStalker already does to map a PC to
`(file, line)`; no relation to `current_crate_namespace_root` (that's a
namespace, not a path) beyond sharing the intent.

## Mechanism (`bs` side)

`Debugger::step_into` (`src/debugger/mod.rs:1296`) gains a sibling that
takes a mode:

```rust
pub enum StepIntoMode { AnyFrame, SkipLibraries }
pub fn step_into_with(&mut self, mode: StepIntoMode) -> Result<(), Error>;
// step_into() == step_into_with(AnyFrame)
```

`SkipLibraries` algorithm (design level):

1. Record the starting frame's stack depth and the caller's return
   address.
2. Do one line-granular step-into.
3. Resolve the landing PC → source path. If it's **user code**, stop
   (done — this is the common, fast path: one step).
4. If it's **library code**, keep going without surfacing a stop:
   - Set a temporary breakpoint at the recorded return address (so we
     catch the library call returning to *our* frame), then run/step.
   - While inside library frames, also stop on entry to any user frame
     (callbacks): the cheap version steps line-by-line and checks the
     path each time; the efficient version sets breakpoints on the user
     compilation unit's line entries reachable from here. Start with the
     line-by-line walk; optimise only if it's measurably slow.
   - Whichever fires first — return-address hit (we're back on the line
     after the call) or a user frame entered (a callback) — is where we
     stop.
5. Bounds / bail-outs (must never hang or run away):
   - A real breakpoint hit during the walk → stop there (respect it).
   - Debuggee exit / signal → surface it.
   - An instruction/step budget (e.g. N steps) as a backstop; on
     exhaustion, fall back to a plain stop and log. Tune N so normal
     library calls complete well under it.

MVP vs full:

- **MVP** (covers ~90%): step-into; if library, `step_out` back to the
  caller and stop on the next line — i.e. behave as Step-Over for
  library calls. Simple, reuses `step_out`/`step_over_any`. Misses the
  *callback* case (a library calling your closure — you'd step over it).
- **Full**: the return-address-breakpoint + stop-on-user-frame walk
  above, which also catches callbacks. Ship MVP first behind the same
  keybinding, then upgrade the engine without changing the UX.

## DAP + extension wiring

DAP's `stepIn` request carries no "modifier held" bit, and VS Code's
built-in `workbench.action.debug.stepInto` (bound to `alt+right` today,
`package.json` keybindings) always sends a plain `stepIn`. So the
modifier becomes **two commands**, not one command reading a key state:

- The extension registers `bugstalker.stepIntoSkipLibs` and
  `bugstalker.stepIntoAnyFrame`. Each calls
  `vscode.debug.activeDebugSession.customRequest('bs/stepIn', { skipLibraries })`
  (a custom request so we don't fight VS Code's `stepIn` bookkeeping;
  the adapter answers it and emits the normal `stopped` event).
- Keybindings (scoped to `when: "inDebugMode && debugType == 'bugstalker'"`
  so other adapters are untouched):
  - `alt+right` → `bugstalker.stepIntoSkipLibs`
  - `shift+alt+right` → `bugstalker.stepIntoAnyFrame`
  - The `lldb` alias type gets the same pair.
- `bs` adds a `bs/stepIn` custom-request handler beside
  `handle_step_in` (`src/dap/yadap/session/control.rs:566`) that reads
  `skipLibraries` and dispatches to `step_into_with`. Plain DAP `stepIn`
  keeps mapping to `AnyFrame` so non-keybinding step-in (toolbar button,
  other clients) still works — its default could later be made
  configurable.

Rejected alternatives: (a) `stepInTargets` (`control.rs:900`) — that's
for picking *which* call on a line, not a my-code filter; (b) a sticky
on/off setting only — loses the per-step choice the user wants; (c)
abusing the `granularity` field — not ours to repurpose.

## Settings

**Not yet wired (v0 ships none of these).** The classifier uses a
hard-coded path heuristic (`LIBRARY_PATH_FRAGMENTS` in
`src/debugger/step.rs`) and the toolbar button stays `AnyFrame`. These
were intentionally *not* declared in `package.json` rather than ship
dead config keys; add them when the adapter actually consumes them.

- `bugstalker.stepInto.skipLibraries` (bool, default **true**) — would
  be the default for plain DAP `stepIn` (toolbar / non-keybinding); the
  keybindings always pick a mode explicitly and ignore it. (Toolbar
  default is still an open question below.)
- `bugstalker.stepInto.libraryPaths` / `...alsoMine` (string[],
  optional) — extra path fragments to force library / force-user.

## Edge cases

- **Recursion / deep user stacks** — depth tracking must use a frame
  identity that survives recursion (return address + SP), not just a
  count.
- **Inlined library code** — an inlined `core::` call has the *caller's*
  user decl_file in some DWARF; treat by the innermost inline frame's
  file. Acceptable if v0 occasionally stops one inline level off.
- **Async** — stepping into a `.await` lands in poll machinery (library)
  that eventually calls the user future. The walk's "stop on user frame"
  naturally handles it; verify against the `tokio_*` examples.
- **No-line-info frames** — treated as library (nothing to show); the
  walk steps past them.
- **The call returns immediately** (library inlined to nothing) — the
  return-address breakpoint fires at once; stop on the next line.
- **Macro expansions** (`println!`) — expand to library calls with the
  user's line as decl_file; ensure we don't treat the macro's own frame
  as a stop target while its body is library.

## Testing

Fixture `examples/step_into_jmc` (bin): a user `main` whose line calls
(a) a pure library function, and (b) a library function that invokes a
user closure (`[1,2,3].iter().map(user_fn).sum()`).

- `SkipLibraries` step-in on (a) → stops on the line *after* the call,
  in `main` (Step-Over semantics), never inside `core`.
- `SkipLibraries` step-in on (b) → stops inside `user_fn` (the
  callback), not in the iterator adaptor. *(Full engine; MVP records
  this as a known gap — steps over.)*
- `AnyFrame` step-in on (a) → stops inside the library frame (today's
  behaviour preserved).
- A DAP integration test driving `bs/stepIn` with `skipLibraries:
  true|false` and asserting the resulting top-frame source path is
  user / library respectively.

## Phasing

1. ✅ **Done.** `step_into_with(SkipLibraries)` MVP + `FrameKind` /
   `classify_source_path` path classifier (`src/debugger/step.rs`) +
   classifier unit tests + doctest. The engine is a **single walk**:
   `step_in`; if the landing frame is library, climb out with
   `step_out_frame`; then stop *iff* the displayed source line is user
   code **and** differs from the start line — otherwise keep walking.
   This one loop steps **over** library calls but **into** user calls in
   execution order, and never strands you on the start line. Three
   subtleties, each found by stepping real programs to the end (see the
   sweep test `test_skip_libs_never_stops_in_library`):
   - **Frame vs. line classification.** The climb/skip decision uses the
     **real frame function's** own file (its first range's place), not
     `find_place_from_pc` — so an *inlined* library call (`iter().map`)
     inside a user line doesn't look like a library frame. But the *stop*
     decision uses the displayed **place** file: we won't stop on an
     inlined library line (`boxed.rs` spliced into a user fn) even though
     its frame is user — we'd be showing the user `boxed.rs`.
   - **Library-then-user on one line.** `helper(&v)` does a `Vec`→`&[T]`
     deref (library) *then* calls the user `helper`. After skipping the
     deref we're mid-line, so we keep walking rather than step-over the
     line — and step *into* `helper`. (Stepping over here was the first
     bug the sweep caught.)
   - **Off the end of `main`.** When the climb finds no user frame to
     return to (stepped past all user code into the C runtime), the step
     degrades to run-to-completion (`continue_to_stop`) — exit or next
     breakpoint — instead of erroring on no-debug-info runtime.
2. ✅ **Done.** `bs/stepIn` custom request (`skipLibraries` bool) →
   `step_into_with` dispatch (`src/dap/yadap/session/control.rs`,
   `…/session/mod.rs`) + DAP integration tests
   (`tests/dap/dap_integration.rs`) + direct-debugger test
   (`tests/debugger/steps.rs::test_step_into_skip_libraries`) over the
   `examples/step_into_jmc` fixture. Plain DAP `stepIn` (toolbar) stays
   `AnyFrame`.
3. ✅ **Done.** Extension: `…stepIntoSkipLibs` / `…stepIntoAnyFrame`
   commands (`extension/stepInto.ts`) sending the custom request,
   keybindings `alt+right` (skip libs) / `shift+alt+right` (any frame)
   scoped per `debugType`, `lldb` alias. Settings deferred — v0 is the
   hard-coded path heuristic (see Settings note below).
4. ⏸️ **Deferred (2026-05-30).** The full callback-aware walk. The
   blind-but-fast `step_out`/`continue` the MVP uses can't pause in a
   user closure the library invokes (`iter().map(user_fn)`) — catching
   it costs either speed (single-step the whole library, regressing the
   headline `println!` case) or machinery (breakpoint every user fn +
   return addr, then one `continue`, plus inlining/async/recursion
   corners). Decision: ship the MVP; the callback case has adequate
   workarounds — `shift+alt+right` (any-frame) and step through, or set
   a breakpoint in your own code. The MVP behaviour is **pinned by
   tests** (`…callback_is_stepped_over_mvp`, and line `28` in the direct
   test) so the future engine upgrade is a deliberate, visible flip.

## Unwinder fix (found while stepping real binaries)

Stepping a real wild-linked binary (whisky-csl tests) erupted in
`dwarf file parsing error: no unwind info for address` the moment a step
descended into a trivial library function (`Vec::new`). Root cause, found
by instrumenting `get_cfa`:

- **macOS compact unwind was silently dead.** `compact_cfa_at` /
  `compact_function_range_at` looked the PC up in the `__unwind_info`
  table with `u32::try_from(global_pc)`. On macOS arm64 every PC is
  `image_base + offset` with `image_base ≈ 0x1_0000_0000`, so the cast
  **always overflowed** → every lookup returned `None`. It only ever
  "worked" because `__eh_frame` happened to cover the functions tested;
  the first function with *only* compact unwind (no FDE) exposed it.
  Fix: subtract the image base (the `__TEXT` segment vmaddr —
  `object`'s `relative_address_base()` returns 0 for Mach-O, so read the
  segment) before the u32 lookup. `src/debugger/debugee/dwarf/mod.rs`.
- **The full unwinder (`unwind.rs`) had no compact fallback at all.**
  `return_address` / backtraces used only `__eh_frame` / `.debug_frame`,
  so `step_out_frame` couldn't climb out of a frameless library leaf.
  Added a compact-unwind return-address fallback: frameless → RA in `lr`;
  frame-based → RA at `[fp+8]` (`CompactUnwind` enum + `compact_unwind_at`).

With both, skip-libs steps cleanly through real user code on macOS arm64
instead of erroring. Note the underlying `step_in` fragility (it can't
introspect stripped system dylibs at all) is handled in the engine by
treating a recoverable step error as "entered foreign code → climb out"
(`is_recoverable_step_error`).

## Open questions

- Default for the *toolbar* Step-In button (plain DAP `stepIn`): skip or
  any-frame? Leaning skip (matches the keybinding default), behind
  `skipLibraries`.
- Should "skip" also apply to Step-Over returning into a library frame
  (it shouldn't normally, but tail calls can surprise)?
- Is there appetite for a status-bar indicator of the current default
  mode, or is the two-key split enough? (Probably enough.)

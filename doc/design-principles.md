<!-- markdownlint-disable MD013 -->
# Design principles

Durable rules that shape how BugStalker is built. Each is here because
violating it has bitten us (or is about to). Keep them short; link the
enforcing test/bench rather than restating the implementation.

## 1. Measure the debuggee, not the debugger

User-facing performance numbers (the perf overlay) describe the
*inferior's* behaviour. They must bracket only the run interval — the
window between resuming the debuggee and the next stop. Everything the
debugger itself does (DWARF parsing, variable enumeration, value reads,
rendering, symbol lookup) happens while the inferior is stopped and must
never be attributed to debuggee time.

- Enforced by structure: `begin_perf_run` fires on resume
  (`begin_running`), `finish_perf_stop` on the stop. Work done in
  `handle_scopes`/`handle_variables` lands outside that bracket.
- Corollary — keep the two perf worlds apart. Benchmarks that measure
  *our* overhead (attach latency, stepping latency, statics enumeration,
  render cost) are a separate, legitimate category. Name them so it's
  obvious they time the debugger, not a debuggee workload, and never
  fold their numbers into anything the user reads as "their program's
  performance". A stepping bench that silently included DWARF parse time
  would be measuring us; a perf overlay that included it would be lying
  to the user. Both are the same mistake from opposite ends.

## 2. Lazy by default for expensive scopes

A pane the user hasn't opened costs nothing. A DAP scope returns its
`variablesReference` cheaply; the contents are enumerated and read only
when the client expands it. Genuinely expensive scopes are marked
`expensive: true` so the client knows not to auto-expand.

- Why: the scope cache is wiped on every stop (`begin_stop_epoch`), so
  anything computed eagerly in `handle_scopes` is recomputed on every
  single step. Reading every file-scope static's value just to hand back
  a reference makes stepping pay, on every step, for data that may never
  be looked at.
- Rule of thumb: if producing a value requires reading inferior memory
  or walking DWARF, defer it to the expand request, not the parent's
  enumeration.

## 3. Don't re-read immutable state

State that physically cannot change for the life of the process is read
once and cached for the session. Read-only statics live in a read-only
segment (`.rodata` / non-writable `PT_LOAD`); their value is fixed at
load. Only mutable state (`.data`/`.bss`, writable segments) is re-read
per stop.

- We already classify storage (`static_ro` vs `static_rw`) and
  mutability for the variables view — reuse that signal, don't re-derive
  it. Cache read-only reads keyed by address/DIE.
- Why it matters here: with thousands of statics, re-reading the
  immutable majority on every stop is the bulk of the cost and buys
  nothing.

## 4. Structure large collections instead of dumping them

A flat list of thousands of unordered entries is unusable. Group by the
structure that already exists — for statics, the `::` namespace path
(`hyper_util::client::legacy::pool::__CALLSITE` becomes a tree). The tree
is not just cosmetic: each namespace node is its own lazy reference, so
expanding one subtree reads only that subtree (principle 2 again).

- Use the namespace metadata the enumeration already carries; don't
  re-parse display strings to recover structure you threw away.

## 5. Never panic on malformed debug info

Debug info comes from many producers (rustc, C/`-sys` crates, older
toolchains) and is frequently wrong. A single bad DIE — an absurd type
size, a negative array bound, a dangling reference — degrades *that one
variable* to unreadable. It never panics the session.

- Validate sizes/counts before allocating (checked arithmetic, reject
  implausible lengths); surface a non-fatal error and carry on.
- Enforced by e.g. `array_byte_size` unit tests and the
  `into_raw_bytes` capacity guard.

# BugStalker Threat Model

BugStalker is a native debugger; it operates with elevated trust over the
debuggee process. This document enumerates the trust boundaries, attack
surfaces, and mitigations across the manifesto phases. It is updated alongside
features.

## Trust Boundaries

- **User → BugStalker process.** The user runs BugStalker; the process inherits
  the user's privileges. Linux: ptrace is restricted to processes the user owns
  (or to all if `kernel.yama.ptrace_scope = 0`); seccomp\_notify requires
  `CAP_SYS_PTRACE` or `kernel.unprivileged_userns_clone`. Darwin:
  `task_for_pid` requires either a codesigned entitlement
  (`com.apple.security.cs.debugger`) or root.
- **BugStalker process → debuggee process.** BugStalker has full memory
  read/write of the debuggee via ptrace/Mach. The debuggee is the inner trust
  boundary; if BugStalker is compromised, so is the debuggee.
- **Debuggee binary → BugStalker.** The binary's DWARF, symbol tables, and
  embedded BugStalker sections (`.bs_viz_spec`, `.bs_viz_wasm`, accelerators
  from Phase 7) are *parsed* by BugStalker. A maliciously-crafted binary should
  not be able to take over BugStalker.
- **External wasm visualisers from disk → BugStalker.** Loaded from
  `~/.config/bugstalker/visualizers/*.wasm`. The disk path is user-writable;
  another local user cannot reach it without compromising the user account.
- **Differential test oracles → BugStalker.** GPL tools like `rr` run in CI as
  binary oracles; their output is consumed but not their source.

## Per-Component Threats

### Phase 2 — `rust-mangle-tree` (parser)

**Threat**: adversarial mangled symbols (deliberately malformed `_R…` or
`_ZN…`) embedded in a malicious binary. Goal: panic, infinite loop, or
unbounded memory in BugStalker.

**Mitigations**:

- Parser is total: returns `Err(ParseError)` for any input, never panics.
  Enforced by `cargo fuzz` on every PR.
- Recursion depth bounded to 256 (matches `rustc-demangle`); deeper inputs
  return `ParseError::TooDeep`.
- Backreferences must point strictly earlier in the input; circular references
  rejected.
- Punycode decoder bounded by output length cap.
- Memory budget per parse: ~3× input size; arena reset between symbols.

### Phase 3 — vtable-driven type recovery + `call_debug_fmt`

**Threat A**: malicious vtable pointer (e.g. heap-corrupted `Box<dyn Trait>`)
leads BugStalker to follow into unmapped memory or unrelated symbols.

**Mitigations**:

- `read_pointer` failures return `Err`, not panic.
- Resolved vtable symbol must demangle as `<T as Trait>::vtable` shape;
  otherwise we render the fat pointer as today and surface a warning.
- The recovered concrete type must have a DWARF DIE; mismatch → fallback.

**Threat B**: `call_debug_fmt` runs the debuggee's own `Debug::fmt` inside the
debuggee. A malicious or buggy `Debug::fmt` can panic, infinite-loop, or
corrupt memory.

**Mitigations**:

- `Debug::fmt` runs on the debuggee's *paused* state; BugStalker controls the
  resumption. We can detect non-termination and abort.
- A fault during the synthetic `Formatter` call manifests as a normal debugger
  exception; the user sees a stack trace, not a debugger crash.
- The synthetic `Formatter` is mmapped in a region BugStalker controls; the
  call is gated by an explicit user command (`vard`/`argd`), not automatic.

### Phase 4 — Wasm Visualisers

**Threat A** (Tier B, third-party from disk): a user installs a wasm visualiser
from an untrusted source. Goal: read host filesystem, exfiltrate data, attack
the debuggee.

**Mitigations**:

- Wasmtime sandbox: no WASI, no host filesystem, no network. Only the typed
  `debuggee` capability interface.
- Memory cap (50 MB / instance), fuel cap (10 M instructions / call), and
  wall-clock cap (100 ms / call).
- The `debuggee` capability proxies to BugStalker's existing memory-read
  primitives — the visualiser cannot escalate beyond reading bytes BugStalker
  can already read.
- Visualisers cannot *write* the debuggee. Read-only by construction in the WIT
  contract.
- Discovery is opt-in: visualisers in `~/.config/bugstalker/visualizers/` are
  loaded; nothing auto-loaded from `/tmp` or `$PWD`.

**Threat B** (Tier A, malicious `.bs_viz_spec` in a debuggee binary): a hostile
crate ships a `TypeViewSpec` that triggers BugStalker bugs.

**Mitigations**:

- Spec is pure data, not code. The spec format is bounded (no recursion in the
  schema beyond depth 16).
- Spec parser is total (same property as rust-mangle-tree); fuzzed.
- A malformed spec causes BugStalker to fall back to default rendering with a
  warning, not crash.

### Phase 5 — Performance Overlay

**Threat**: a debuggee deliberately overflows the perf ring buffer or floods the
PT trace to cause BugStalker decoder OOM.

**Mitigations**:

- Ring buffer is fixed size; overflow shows up as `PERF_RECORD_LOST` and is
  reported to the user.
- PT decode is bounded by the AUX buffer size (64 MB); decoder caps memory and
  aborts if exceeded.
- Decoder runs in BugStalker's own collector thread, not the debuggee's; OOM
  here aborts BugStalker only, not the debuggee.

### Phase 6 — Time Travel

**Threat A**: `seccomp_notify` lets the tracer rewrite the debuggee's syscall
results. A bug in the recorder/replayer could corrupt the debuggee's state.

**Mitigations**:

- The seccomp filter installed in the debuggee restricts only what we want to
  intercept; it cannot escalate the debuggee's privileges.
- Replay-mode results are read from the trace file, not synthesised — corruption
  requires forging trace files (which requires a higher-trust attacker than the
  threat we model).
- The recorder runs as the user; no setuid/`CAP_SYS_ADMIN` paths.

**Threat B**: a malicious trace file from another user is loaded for replay.
Goal: corrupt BugStalker, exfiltrate from the replay-host's debuggee.

**Mitigations**:

- The trace manifest's `build-id` must match the binary at replay time;
  mismatched binary → refuse.
- Trace records are validated before being applied (length-prefixed,
  ruzstd-decoded, schema-checked).
- Replay runs the binary in BugStalker's normal sandboxed-tracee model; it
  cannot escape further than recording could.

### Phase 7 — Linker Accelerators

**Threat**: a maliciously-linked binary with crafted `.bs_vtables` entries
pointing at attacker-controlled addresses. Goal: BugStalker dereferences a
pointer that crashes or exfiltrates.

**Mitigations**:

- Vtable address is checked against the loaded module's mapped executable range
  (`debug_assert!` plus runtime guard) before any deref.
- DIE offsets are validated against `.debug_info` size.
- Section header magic + version is checked; on any anomaly we fall back to the
  slow path (no acceleration).
- Fast-vs-slow consistency check (`cfg(debug_assertions)`): in debug builds the
  answer from the accelerator must match the answer from the scanner. CI runs
  both modes.

### Cross-Cutting — DWARF Input

**Threat**: a debuggee binary contains malicious DWARF (e.g. cyclic type
chains, oversize variable-length records).

**Mitigations**:

- `gimli` (the DWARF parser) is fuzzed upstream; we keep its version current.
- Type-graph traversal has a depth cap (`MAX_RENDER_DEPTH`).
- Variable-length records have explicit byte caps.

## Outstanding Work

- Formal threat model for `bs-replay-engine`'s seccomp filter shape (what
  syscalls do we intercept; what do we never let through). Lands with Phase 6
  sub-phase 3B.
- Capability audit for the WIT `debuggee` interface (Phase 4) — does any
  function leak more than the user can already see? Lands with Phase 4 ship.
- Supply-chain review of submodule pins (Phase 8). Lands with Phase 8
  infrastructure.
- Differential fuzzing between BugStalker and `lldb` on adversarial DWARF
  inputs.

## Reporting Policy

Security issues should be filed via GitHub's private vulnerability reporting on
the BugStalker repository. Do not file public issues for unpatched
vulnerabilities. We commit to acknowledge within one week and ship a fix or
coordinate disclosure within 90 days.

## Non-Goals

- Not a guarantee against all attacks. The debugger trusts the user fully; a
  user running BugStalker on a malicious binary they own is debugging at their
  own risk, but the debugger should not amplify damage.
- Not a sandbox for the debuggee. BugStalker debugs; it does not contain.
- Not protection against compromised dependencies (cargo supply-chain attacks).
  `cargo deny` + dependency review handle that elsewhere.

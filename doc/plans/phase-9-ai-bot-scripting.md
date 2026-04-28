# Phase 9 — AI-bot-friendly scripting surface

BugStalker today has two front-ends: a human-oriented console
(rustyline + crossterm + ANSI colour) and a DAP server speaking the
VSCode debug-adapter protocol over stdio/TCP. Both work for their
target audience. Neither is a *good* fit for an AI agent driving the
debugger as a tool — and that workflow is increasingly the way Rust
debuggers will be used.

This phase designs and lands a third front-end: a stable, structured,
*scriptable* interface tuned for non-interactive callers (AI agents,
CI scripts, smoke harnesses). It is **not** a replacement for either
of the existing front-ends.

## Why this is its own phase

A naïve "just emit JSON instead of strings" pass would scatter format
flags through every printer in `src/ui/generic/*`. Doing it right
needs a single structured-output layer that every command flows
through, plus a transport that an out-of-process caller can depend on.
That is design work, not a refactor — hence a phase.

## The audience and what it needs

An AI agent driving BugStalker has four needs the existing surfaces
don't satisfy cleanly:

1. **Stable, machine-parseable output.** Colour codes, ad-hoc
   indentation, "x is at 0x7f…", "no active task found for current
   worker, or no active worker found" — all anti-friendly for
   programmatic consumers. The agent needs typed JSON values.
2. **Headless invocation.** No PTY. No TTY. `bugstalker --script`
   takes a list of commands on stdin (one per line, or a JSON array)
   and produces one structured response per command on stdout. Like
   `gdb --batch -ex` but JSON.
3. **Self-describing capabilities.** A `describe` command (or a
   one-shot `--describe-commands` mode) returns a JSON schema for
   every command's name, arguments, and response shape. Agents can
   introspect rather than memorise.
4. **Streaming events alongside command responses.** A breakpoint
   hit, an `exit`, a `SIGSEGV` are events, not command outputs. The
   transport interleaves them with command responses using a tagged
   envelope (`{"kind": "event", ...}` vs `{"kind": "response", ...}`).

The DAP server already addresses #2 and #4 — but DAP is heavy. Its
schema is VSCode-shaped (frames, scopes, threads, variables) and
doesn't expose BugStalker-specific commands (`oracle`, `async
await-trace`, `call`, `trigger`, the format-spec language). Mapping
every BugStalker command to a DAP `customRequest` would be
straight-line plumbing but inverts the right dependency: the
*native* surface gets second-class JSON exposure through a protocol
designed for someone else.

## Proposed architecture

### A. The structured-output layer (`src/ui/structured/`)

One module, one trait:

```rust
pub trait StructuredCommand {
    type Response: Serialize;
    fn execute(self, dbg: &mut Debugger) -> Result<Self::Response, BsError>;
}
```

Every existing `Command` variant gets a corresponding
`StructuredCommand` impl. The console keeps its current handler;
the structured layer is a parallel dispatch that never produces a
human string.

`BsError` is the single error envelope:

```json
{ "kind": "error", "code": "VAR_NOT_FOUND",
  "message": "no variable named 'foo' in scope",
  "location": { "file": "src/main.rs", "line": 42 } }
```

Codes are stable; messages can evolve.

### B. The transport (`src/ui/script/`)

One binary entry point: `bugstalker --script` (or
`bugstalker-script` if we want a separate crate target). Reads
JSON-RPC 2.0 lines on stdin, writes JSON-RPC responses + JSON-encoded
events on stdout. JSON-RPC because it is universal, well-trodden, and
maps directly to the request/response/notification model BugStalker
already needs.

```jsonc
// → request
{ "jsonrpc": "2.0", "id": 1,
  "method": "break.set",
  "params": { "at": "src/main.rs:42" } }

// ← response
{ "jsonrpc": "2.0", "id": 1,
  "result": { "breakpoint_id": 7, "address": "0x55a…" } }

// ← event (server-initiated, no `id`)
{ "jsonrpc": "2.0", "method": "stop",
  "params": { "reason": "breakpoint",
              "breakpoint_id": 7,
              "thread": 12345,
              "frame": { "function": "my_app::handler",
                         "file": "src/main.rs", "line": 42 } } }
```

### C. Capability discovery

`bugstalker --describe-commands` (no debuggee — pure metadata)
prints a single JSON document: every method name, its `params` schema,
its `result` schema, its event shape if any. JSON Schema Draft 7. The
agent can either embed this once and pin to a BugStalker version, or
re-fetch on connect.

### D. Output budgeting

AI agents have context windows. A `var print local_map` on a 100k-entry
HashMap is destructive. Two mitigations:

1. Every list-shaped response carries `{ items: [...], total: N,
   truncated: bool, cursor: "opaque" }`. A second call with `cursor`
   continues. Same machinery as Phase 1 F3's `LEN_GUARD`, lifted from
   ad-hoc into the response envelope.
2. The transport accepts a `max_response_bytes` per-request hint.
   Server respects it by setting `truncated: true` early.

### E. Determinism and reproducibility

For agent workflows that diff one run against another (root-cause
search, regression bisect):

- No timestamps in output by default. An optional
  `include_timestamps: true` request flag.
- No randomly-assigned IDs in user-visible fields. `breakpoint_id`,
  `task_id`, `thread_id` already come from the runtime; that's fine.
  Internal UUIDs (`BsUnit::id`) stay internal.
- Address output uses fixed-width hex with a leading `0x`. No
  `Display` impls leaking over.

## What this is NOT

- Not a replacement for the console. Humans keep the rustyline UI.
- Not a replacement for DAP. VSCode keeps using DAP. DAP-side
  exposure of new BugStalker features (e.g. `bs/awaitTrace` from
  Phase 3D) keeps growing as `customRequest`s — but it's no longer
  the *source of truth*; it becomes one of two transports over the
  same structured-command core.
- Not a script *language*. There is no `if/while/let` — the agent
  composes commands itself. (If a need emerges later, the natural
  evolution is to embed `rhai` or `mlua` over the same command core,
  not to extend JSON-RPC.)

## Phasing

Implementation falls into four batches, each independently shippable.

### A — Structured-command core

- `StructuredCommand` trait + `BsError` envelope.
- Migrate **read-only** commands first (no execution-state changes):
  `var`, `args`, `bt`, `frame info`, `thread info`, `sharedlib
  info`, `break info`, `watch info`, `async backtrace`, `async
  await-trace`. These are the agent's bread and butter and have no
  side-effects.
- Existing console handlers delegate to the structured layer when
  appropriate, but most just keep their bespoke printer.

### B — Stateful commands

- `break set/remove`, `watch set/remove`, `run`, `continue`,
  `step{i,into,out,over}`, `async step{over,out}`, `call`,
  `register write`, `memory write`. These mutate state; their
  responses now carry the *stop-reason* envelope used by the event
  stream so the agent can correlate.

### C — Transport

- `bugstalker --script` reads JSON-RPC 2.0 on stdin, writes
  responses + events on stdout.
- `--describe-commands` capability dump.
- Streaming events (stop, exit, signal, watchpoint hit) emitted as
  JSON-RPC notifications interleaved with responses.

### D — Documentation and conformance

- `doc/scripting/` user guide: connect, describe, run a session,
  example transcripts.
- `doc/scripting/schema.json` checked-in JSON Schema, regenerated
  from the structured-command core in CI; CI fails if drift.
- `crates/bs-script-conformance/` test crate: spawns
  `bugstalker --script` against `examples/*` debuggees, runs a
  recorded transcript, asserts the response stream byte-matches a
  golden file. Catches accidental output changes.

## Acceptance criteria

- An AI agent can drive a full debug session — set breakpoints, run,
  inspect variables, walk an await-trace, step, finish — using only
  JSON-RPC over stdin/stdout.
- `bugstalker --describe-commands | jq` produces a valid JSON Schema
  document.
- Conformance crate passes on Linux x86_64, Linux aarch64, and macOS
  arm64.
- The DAP server's `customRequest` handlers are reimplemented as
  thin shims over the structured-command core (proving the layering
  is right).

## Effort estimate

~5 weeks engineer-time. Bulk of the work is mechanical:
StructuredCommand impls for every existing command (~50 commands).
The transport (`--script`) is small (~500 lines). Capability discovery
and the conformance crate are the design-heavy parts and need the
most care.

## Open questions

1. **Should `--script` accept multiple concurrent inferiors?** The
   console runs against one debuggee at a time. An agent might want
   one scripting endpoint controlling several inferiors (compare
   behaviour across binaries). Probably "no" for v1 — bots can spawn
   one BugStalker per inferior. Revisit if there is real demand.
2. **Should events be opt-in?** Some agents want pure RPC and will
   poll `state` themselves. A `subscribe_events: true` field on the
   first `initialize` call is cheap. v1 default: events on.
3. **Authentication?** `--script` is stdio-bound, so the security
   model is "whoever started the process". A future TCP-bound
   mode would need bearer tokens. Out of scope for v1.
4. **Token-budget hints from the agent.** The agent knows its own
   context window. A per-request `max_response_bytes` is the
   minimum; a per-session "I am tight on context, prefer truncation
   over verbosity" mode is a thought.

## Specifications

- JSON-RPC 2.0 — <https://www.jsonrpc.org/specification>
- JSON Schema Draft 7 — <https://json-schema.org/draft-07/json-schema-release-notes.html>
- Debug Adapter Protocol (the prior art we are deliberately *not*
  reusing as the source of truth) —
  <https://microsoft.github.io/debug-adapter-protocol/>

## Invariants

```rust
// Every response is either Ok or Err — never partial.
debug_assert!(response.is_ok() ^ response.is_err());

// Cursor opacity: agents must not parse cursor strings.
debug_assert!(cursor.starts_with("bs:"));

// Capability schema is monotone within a major version: existing
// fields don't disappear, existing types don't change shape, only
// new fields/methods get added.
```

## Cross-references

- Phase 3D's `bs/awaitTrace` DAP custom request becomes one of the
  first methods migrated to the structured-command core; the DAP
  shim then calls through it.
- Phase 1 F3's `LEN_GUARD` / render budget is the prior art for
  output truncation and should generalise into the response
  envelope's `truncated` / `cursor` machinery.
- Phase 8 (testing) should grow a "script-conformance" target that
  the new conformance crate plugs into.

# BugStalker scripting front-end

`bs --script` is a stable, machine-friendly transport intended for AI
agents and CI scripts. It speaks JSON-RPC 2.0 over stdin/stdout, accepts
JSON5 input (so a script file can carry `// comments` and trailing
commas), and emits one JSON object per line of output.

The console (`bs my-app`) and the DAP server (`bs --dap-local`) are
unchanged. This is a third front-end, not a replacement.

## Quickstart

```sh
$ cat > session.json5 <<'EOF'
// Set a breakpoint, run, dump dyn_ref.
{ jsonrpc: "2.0", id: 1, method: "break.set", params: { at: "main.rs:122" } }
{ jsonrpc: "2.0", id: 2, method: "run" }
{ jsonrpc: "2.0", id: 3, method: "var", params: { name: "dyn_ref" } }
EOF

$ bs --script ./my-app < session.json5
{"jsonrpc":"2.0","method":"event","params":{"kind":"process_installed","pid":12345}}
{"jsonrpc":"2.0","id":1,"result":{"breakpoints":[…],"deferred":false}}
{"jsonrpc":"2.0","method":"event","params":{"kind":"breakpoint_hit",…}}
{"jsonrpc":"2.0","id":2,"result":{"kind":"breakpoint",…}}
{"jsonrpc":"2.0","id":3,"result":{"items":[{"name":"dyn_ref","type":"…","value_text":"…"}],"total":1,"truncated":false}}
```

## Catalogue

`bs --describe-commands` writes the full JSON Schema document for every
method, the request/response/notification envelopes, the error envelope,
and the event variants. The schema follows JSON Schema Draft 7. Pin to
the version you tested against:

```sh
$ bs --describe-commands > schema.json
$ jq '.methods | map(.method)' schema.json
[
  "bt", "frame.info", "thread.info", "sharedlib.info",
  "break.info", "watch.info", "var", "arg",
  "run", "continue",
  "step.into", "step.over", "step.out", "step.instruction",
  "break.set", "break.remove",
  "watch.set", "watch.remove"
]
```

The catalogue is monotone within a major version: existing fields and
methods do not disappear, types do not change shape, only new
methods/fields get added.

## Wire format

### Requests

```jsonc
{
  jsonrpc: "2.0",     // optional; defaults to 2.0
  id: 1,              // any JSON value; omit for notifications
  method: "var",      // see --describe-commands
  params: { name: "x" },
  // Optional truncation hint. The server emits truncated:true and
  // a continuation cursor early if the response would otherwise
  // exceed this byte budget.
  max_response_bytes: 65536,
}
```

JSON5 features available on input: `// line comments`, `/* block */`,
unquoted keys, single-quoted strings, trailing commas, multi-line
values. Input may be one request per line or one large value spanning
multiple lines — the parser auto-detects.

Notifications (no `id`) get no response.

### Responses

Every response carries `jsonrpc: "2.0"` and the request's `id`, plus
exactly one of `result` or `error`:

```jsonc
{ jsonrpc: "2.0", id: 1, result: { … } }
{ jsonrpc: "2.0", id: 2, error: { code: -32602, message: "…", data: null } }
```

### Events

Server-initiated notifications carry `method: "event"` and no `id`. The
event variant lives in `params.kind`:

```jsonc
{ jsonrpc: "2.0", method: "event", params: { kind: "breakpoint_hit", … } }
{ jsonrpc: "2.0", method: "event", params: { kind: "exit", code: 0 } }
```

See `--describe-commands` for the full event schema.

## Error envelope

```jsonc
{
  jsonrpc: "2.0", id: 1,
  error: {
    code: -32001,
    message: "program is not being started",
    // optional structured detail
    data: null,
  }
}
```

| Code      | Meaning                                                        |
| --------- | -------------------------------------------------------------- |
| `-32700`  | `PARSE_ERROR` — malformed JSON5                                |
| `-32601`  | `METHOD_NOT_FOUND` — unknown method name                       |
| `-32602`  | `INVALID_PARAMS` — params shape mismatch                       |
| `-32603`  | `INTERNAL` — unclassified internal error                       |
| `-32001`  | `PROCESS_NOT_STARTED`                                          |
| `-32002`  | `PROCESS_EXITED`                                               |
| `-32010`  | `VAR_NOT_FOUND`                                                |
| `-32011`  | `FRAME_NOT_FOUND`                                              |
| `-32012`  | `THREAD_NOT_FOUND`                                             |
| `-32013`  | `BREAKPOINT_NOT_FOUND`                                         |
| `-32014`  | `UNRESOLVED_LOCATION`                                          |
| `-32015`  | `WATCHPOINT_NOT_FOUND`                                         |
| `-32016`  | `STOPPED_ABNORMALLY`                                           |
| `-32017`  | `NO_REPLAY_SESSION`                                            |
| `-32018`  | `NO_FOCUSED_THREAD`                                            |
| `-32019`  | `BAD_ADDRESS`                                                  |
| `-32020`  | `BAD_EXPRESSION`                                               |

Codes are stable; messages may evolve.

## Output budgeting

Every list-shaped response carries `items / total / truncated / cursor`:

```jsonc
{
  result: {
    items: [ … ],
    total: 50000,
    truncated: true,
    cursor: "bs:offset:1024"
  }
}
```

Cursors are **opaque** — agents must not parse them. Future cursor
schemes will keep the `bs:` prefix and change what follows.

A per-request `max_response_bytes` hint makes the server set
`truncated: true` early. Without a hint, list responses cap at 1024
items.

## Locations (breakpoints, etc.)

Two equivalent forms — agents may pick whichever is most natural:

```jsonc
// shorthand string
{ method: "break.set", params: { at: "main.rs:42" } }
{ method: "break.set", params: { at: "my_app::handler" } }
{ method: "break.set", params: { at: "0x55a01200" } }

// tagged-union object
{ method: "break.set", params: { at: { kind: "line", file: "main.rs", line: 42 } } }
{ method: "break.set", params: { at: { kind: "function", name: "my_app::handler" } } }
{ method: "break.set", params: { at: { kind: "address", address: "0x55a01200" } } }
```

## Driving from a host script

### Rust embedders — typed client

If your host script is itself in Rust, use
`bugstalker::ui::script::client::ScriptClient`. It owns the
subprocess, knows every method's request/response type, and surfaces
server errors as a typed `ClientError::Server(BsError)`:

```rust
use bugstalker::ui::script::ScriptClient;
use bugstalker::ui::structured::commands::r#break::{BreakSet, Location};
use bugstalker::ui::structured::commands::print_var::Var;
use bugstalker::ui::structured::commands::run::Run;

let mut bs = ScriptClient::spawn("./target/debug/bs", "./my-app")?;
bs.call(BreakSet { at: Location::Shorthand("main.rs:42".into()), deferred: false })?;
let stop = bs.call(Run::default())?;
let v = bs.call(Var { name: Some("x".into()), expression: None })?;
println!("stopped at {}: x = {}", stop.address, v.items[0].value_text);
```

`call_raw(method, params)` is an escape hatch when the wire has gained
a method the Rust DTOs don't yet expose.

### Other languages

Spawn `bs --script <debuggee>` as a subprocess, write JSON-RPC objects
to stdin, parse JSON-per-line on stdout, dispatch responses (have
`id`) versus events (`method: "event"`). See `examples.md` for a
Python driver template.

## Determinism

- No timestamps in responses unless requested with
  `include_timestamps: true` (per-request, not yet wired in v1 — fields
  reserved for v1.1).
- Addresses are fixed-width `0x…` hex.
- IDs (breakpoint, thread, frame) come straight from the runtime —
  stable within a session.

## Limits and non-goals

- One inferior per `bs --script`. Drive multiple inferiors by spawning
  one BugStalker per process; the v1 transport does not multiplex.
- No script *language*. There is no `if/while/let` — the agent composes
  requests itself.
- The DAP server is a separate transport; methods exposed by the
  scripting front-end are not automatically available as DAP custom
  requests (yet).
- `bs --script` reads stdio. A future TCP-bound mode would need a
  bearer-token authentication design.

## Phasing

This is the v1 surface. See `doc/plans/phase-9-ai-bot-scripting.md` for
the design decisions behind it. The natural v2 work:

- structured `value_tree` for `var` / `arg` so agents can introspect
  nested struct fields without re-querying;
- inferior `output` events so agents can see what the debuggee printed;
- DAP custom-request shims that delegate to the same structured-command
  core (proving the layering is right);
- async commands (`async.bt`, `async.at`) and replay commands as
  structured methods.

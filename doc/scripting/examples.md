# Example transcripts

These are runnable. Pipe each block into
`bs --script ./your-debuggee` after `cargo build` of the inferior. The
JSON5 input accepts `// comments` and trailing commas.

## 1. Set a breakpoint, run, dump a variable

The original case from the user that motivated this front-end:
break at a line, run, inspect a `dyn Trait` reference.

```jsonc
// Line 122 of the showcase example holds a `&dyn Greeter` named
// dyn_ref. Set the breakpoint, run, dump it.
{ jsonrpc: "2.0", id: 1, method: "break.set", params: { at: "main.rs:122" } }
{ jsonrpc: "2.0", id: 2, method: "run" }
{ jsonrpc: "2.0", id: 3, method: "var", params: { name: "dyn_ref" } }
{ jsonrpc: "2.0", id: 4, method: "var", params: { name: "dyn_box" } }
```

## 2. List breakpoints, threads, libraries

All read-only — useful for polling.

```jsonc
{ jsonrpc: "2.0", id: 1, method: "break.info" }
{ jsonrpc: "2.0", id: 2, method: "thread.info" }
{ jsonrpc: "2.0", id: 3, method: "sharedlib.info" }
{ jsonrpc: "2.0", id: 4, method: "frame.info" }
{ jsonrpc: "2.0", id: 5, method: "bt", params: { all: false } }
```

## 3. Step through a function

```jsonc
{ jsonrpc: "2.0", id: 1, method: "break.set", params: { at: { kind: "function", name: "my_app::compute" } } }
{ jsonrpc: "2.0", id: 2, method: "run" }
{ jsonrpc: "2.0", id: 3, method: "step.over" }
{ jsonrpc: "2.0", id: 4, method: "step.over" }
{ jsonrpc: "2.0", id: 5, method: "var" /* all locals */ }
{ jsonrpc: "2.0", id: 6, method: "step.out" }
```

## 4. Inspect deep data structures (DQE)

`var.expression` accepts a Data Query Expression — same syntax as the
console's `var foo.bar[0]`:

```jsonc
{ jsonrpc: "2.0", id: 1, method: "var", params: { expression: "config.servers[0].port" } }
{ jsonrpc: "2.0", id: 2, method: "var", params: { expression: "*ptr" } }
{ jsonrpc: "2.0", id: 3, method: "var", params: { expression: "buf[0..16]" } }
```

## 5. Watch a memory address

```jsonc
{ jsonrpc: "2.0", id: 1, method: "watch.set", params: {
    address: "0x16fdfe4b8",
    size: "bytes8",
    condition: "write"
}}
{ jsonrpc: "2.0", id: 2, method: "watch.info" }
{ jsonrpc: "2.0", id: 3, method: "continue" }
{ jsonrpc: "2.0", id: 4, method: "watch.remove", params: { number: 1 } }
```

## 6. Truncation hint

Agents with tight context windows can ask the server to truncate early.
Returned `cursor` is opaque; pass it back on a follow-up request to
continue.

```jsonc
{
  jsonrpc: "2.0",
  id: 1,
  method: "thread.info",
  // Cap the response at 4 KiB. The server will set truncated:true and
  // include a cursor before exceeding the budget.
  max_response_bytes: 4096
}
```

## 7. Driver pseudocode (Python)

```python
import json
import subprocess

bs = subprocess.Popen(
    ["bs", "--script", "./my-app"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    text=True
)

def request(method, params=None, id=None):
    obj = {"jsonrpc": "2.0", "method": method}
    if params is not None: obj["params"] = params
    if id is not None: obj["id"] = id
    bs.stdin.write(json.dumps(obj) + "\n")
    bs.stdin.flush()

def read_one():
    return json.loads(bs.stdout.readline())

# Breakpoint then run.
request("break.set", {"at": "main.rs:42"}, id=1)
request("run", id=2)

while True:
    msg = read_one()
    if msg.get("method") == "event":
        kind = msg["params"]["kind"]
        if kind == "exit":
            break
    elif msg.get("id") == 2:  # response to run
        break

# Now query.
request("var", {"name": "x"}, id=3)
print(read_one())
```

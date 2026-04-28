# Logging convention

Phase 0 deliverable. Source: `doc/plans/phase-0-preflight.md` § Logging convention.

## Crate

All BugStalker workspace crates use [`tracing`](https://docs.rs/tracing). New
crates declare it via the workspace dependency:

```toml
[dependencies]
tracing.workspace = true
```

The root `bugstalker` crate currently uses `log` + `env_logger`. Migration
to `tracing` is **not** part of Phase 0 — it is queued for the first
phase that meaningfully edits the affected modules.

## Levels

| Level   | When                                                               |
| ------- | ------------------------------------------------------------------ |
| `debug` | Development noise; off by default.                                 |
| `info`  | Single-line user-relevant events ("attached to PID 12345").        |
| `warn`  | Visible degradation ("vtable resolution falling back to slow path").|
| `error` | Things that prevent forward progress.                              |

## Target

Every event uses `target = <crate name>` (the default — `module_path!()`
already resolves to the crate root for the typical `tracing::info!` call).
Cross-crate fanout (e.g. `bs-test-harness` calling into `bs-perf`) gets
distinct lines per crate, not nested.

## Filter format

`RUST_LOG` follows the standard `tracing-subscriber` env-filter grammar.
The canonical project filter is:

```text
RUST_LOG=bugstalker=info,bs_perf=debug,rust_mangle_tree=warn
```

Each new workspace crate registers under its own name. The root binary
(`bs`) installs the subscriber once at startup.

## Forbidden

- `eprintln!` / `println!` from library code. Only the binary entry
  point and CLI handlers may print to stdout/stderr.
- `dbg!()` outside of throwaway test code.
- Custom log macros that bypass `tracing` (the project uniform pipeline
  is what makes `RUST_LOG` filtering reliable).

## Enforcement

- `cargo clippy --workspace -- -D warnings` will gain a project-local
  lint group in Phase 8 (`bs-clippy`) that flags `eprintln!`/`println!`
  in non-binary targets.
- Until then, reviewers reject the patterns by hand.

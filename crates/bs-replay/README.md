<!-- markdownlint-disable MD041 -->
# bs-replay

Tier 2 fork-checkpoint orchestrator for BugStalker's Phase 5
time-travel debugging.

The plan calls for "fork PIDs in a ring buffer (e.g. last 32
checkpoints)" with "when the buffer fills, oldest gets SIGKILL".
This crate is the platform-agnostic ring; the platform-specific
fork-and-stop work hides behind the [`CheckpointMechanism`] trait.

## Status

Phase 5 — Tier 2 orchestration **shipped**; per-platform mechanisms
are stubs awaiting Linux/Mach test paths. See
[`doc/phase-5-overview.md`](../../doc/phase-5-overview.md).

| Component                        | Status                                        |
| -------------------------------- | --------------------------------------------- |
| `CheckpointMechanism` trait      | shipped                                       |
| `CheckpointRing<M>` orchestrator | shipped — capacity-bounded FIFO + drop-oldest |
| `MockCheckpointMechanism`        | shipped — proves the ring on any host         |
| Linux `ForkCheckpointMechanism`  | stub: real `fork(2)` + ptrace deferred        |
| Darwin `MachCheckpointMechanism` | stub: real `mach_vm_remap` deferred           |

## Why a trait + mock + stubs

The real fork-checkpoint capture needs ptrace syscall-injection
into the debuggee — genuinely untestable from the macOS-arm64 dev
host without a Linux runner. The trait + mock split lets the
*orchestration* land today (eviction, lookup, drain, capacity
invariants), tested on any platform. Linux's real mechanism slots
in as a single `impl CheckpointMechanism for ForkCheckpointMechanism`
when there's a Linux runner to prove it on.

## Quick start

```rust
use bs_replay::{CheckpointRing, MockCheckpointMechanism};

let mut ring = CheckpointRing::with_capacity(
    MockCheckpointMechanism::new(),
    8,
);

// Take a few — keys are caller-defined (event index, PC, wallclock ns).
ring.take(0)?;
ring.take(100)?;
ring.take(250)?;

// Replay-driver query: latest checkpoint at or before target.
if let Some(handle) = ring.find_at_or_before(180) {
    // Restore from this checkpoint. (Mock returns ID; real impl
    // returns a PID that you `SIGCONT` and `PTRACE_SEIZE`.)
    let _ = handle;
}

// On full + new take: ring evicts the oldest via mechanism.kill().
```

## Plan invariant

`self.checkpoints.len() <= MAX_CHECKPOINTS` is `debug_assert!`-pinned
on every successful `take()`. `MAX_CHECKPOINTS = 32` per the plan.

## Tests

`cargo nextest run -p bs-replay`. Nine unit tests covering ring
semantics: empty, below-capacity inserts, at-capacity eviction,
at-or-before lookup across boundaries, drain, capacity-zero
clamping, take-failure rollback, eviction-kill-failure surfacing,
iteration order.

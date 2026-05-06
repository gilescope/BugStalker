<!-- markdownlint-disable MD041 -->
# bs-replay

Tier 2 fork-checkpoint orchestrator for BugStalker's Phase 5
time-travel debugging.

The plan calls for "fork PIDs in a ring buffer (e.g. last 32
checkpoints)" with "when the buffer fills, oldest gets SIGKILL".
This crate is the platform-agnostic ring + the per-platform
mechanisms that take, restore, and kill checkpoints.

## Status

| Component                                  | Status                              |
| ------------------------------------------ | ----------------------------------- |
| `CheckpointMechanism` trait                | shipped                             |
| `CheckpointRing<M>` orchestrator           | shipped                             |
| `MockCheckpointMechanism`                  | shipped                             |
| Linux `LinuxForkSelfMechanism`             | shipped — fork(2) + SIGSTOP + SEIZE |
| Linux `proc_maps` / `proc_mem` / `proc_regs` | shipped — full state capture/restore |
| Linux Tier 2 ↔ Tier 3 bridge               | shipped — payload codec + writer integration |
| Darwin `mach_vm_remap`-style snapshotter   | shipped — task_for_pid + region walk + read/write |

## Why a trait + per-platform mechanism

Linux's `fork(2)` gives us a free copy-on-write child; macOS
doesn't, so the Darwin path heap-copies writable regions via
`mach_vm_read_overwrite`. The trait split keeps the
orchestration (eviction, lookup, drain, capacity invariants)
testable on any host while the kernel-touching parts stay in
their own modules.

## Quick start

Ring orchestration with the mock mechanism (cross-platform):

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

// Replay query: latest checkpoint at or before target.
if let Some(handle) = ring.find_at_or_before(180) {
    // …restore from handle.payload…
}
// On full + new take: ring evicts the oldest via mechanism.kill().
```

Linux real Tier 2 (fork checkpoint with SIGSTOP + SEIZE):

```rust
use bs_replay::linux::fork_self::LinuxForkSelfMechanism;
use bs_replay::linux::tier2::Tier2Capture;

let mut mech = LinuxForkSelfMechanism::new();
let cap = Tier2Capture::capture(&mut mech, /*key=*/ 0)?;
// cap.state holds writable memory + register snapshot.
// To restore into a fresh fork:
//   restore_writable_state(target_pid, &cap.state.writable)?;
//   restore_registers(target_pid, &cap.state.regs)?;
cap.kill(&mut mech)?;
```

Darwin Mach checkpoint:

```rust
use bs_replay::darwin::checkpoint::{capture_for_task, task_port_for_self};

let task = task_port_for_self();
let state = capture_for_task(task)?;
// state.regions holds (addr, bytes) pairs for every writable+
// !shared region.
```

## Plan invariants

`self.checkpoints.len() <= MAX_CHECKPOINTS = 32` per the plan.
Pinned via `debug_assert!` on every successful `take()`.

## Tests

`cargo nextest run -p bs-replay`. 19 tests covering ring
semantics on every host + Mach VM region walker (Darwin) +
LinuxForkSelf end-to-end on Linux: fork + SIGSTOP + SEIZE +
capture writable state + register capture + restore into a
sibling fork + assert byte-equality.

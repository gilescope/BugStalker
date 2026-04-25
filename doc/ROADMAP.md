# BugStalker architecture roadmap

This document captures the larger-than-one-PR efforts that the
codebase is currently moving towards. Each section names the goal,
sketches the design, and lists the concrete milestones; the
[`CHANGELOG`](../CHANGELOG.md) tracks what has actually shipped.

## Linux/aarch64 port

**Goal:** debug aarch64-unknown-linux-gnu binaries with the same
feature set as x86_64. Drives running BugStalker inside an arm64
Linux VM on an Apple Silicon host (and on AWS Graviton, Raspberry Pi,
…) without yet attempting a native Darwin port.

**Status:** core debug path works; software breakpoints, stepping,
multithreading, signal handling, DWARF unwinding and most
variable inspection pass on linux/arm64. See the most recent
`feat(aarch64): …` and `fix(aarch64): …` entries in
[`CHANGELOG`](../CHANGELOG.md#unreleased).

**Done**

* Compile-ready skeleton: `register/{x86_64,aarch64,debug}.rs` split,
  `BRK #0` software breakpoint, `PTRACE_GETREGSET(NT_PRSTATUS)` for
  general-purpose registers, DWARF register numbering per ARM IHI
  0057.
* `Breakpoint::PC_ADJUST` (x86: 1 — `INT3` reports PC+1; aarch64: 0 —
  `BRK #0` reports PC).
* `disasm.rs` arch-aware byte-restoration when a function under
  disassembly has live breakpoints (1 byte vs 4 bytes).
* `libthread_db` shim — the upstream `thread_db` crate is x86_64/i686
  only; the aarch64 stub keeps the rest of the debugger working with
  TLS-related features gracefully degraded.
* SysV-AMD64 inferior-call machinery (`CallContext`/`CallHelper`,
  `mmap`/`munmap` shellcode) gated to x86_64; on aarch64,
  `Debugger::call` and `call::fmt::call_debug_fmt` return clear
  errors.
* `NT_ARM_HW_WATCH` data watchpoints with WCR/WVR encoding and
  `si_addr`-based hit attribution. The kernel-level path is correct;
  the 9 watchpoint integration tests are skipped on aarch64
  containers because Apple Virtualization.framework does not
  virtualise debug-exception delivery (the `TRAP_HWBKPT` never
  fires). The tests should pass on a KVM host or bare metal.

**Remaining**

1. **AAPCS64 inferior calls.** Marshal up to eight integer/pointer
   args in `x0..x7`, lay down a `BLR x8 ; BRK #0` trampoline, do
   `mmap`/`munmap` via `svc #0` with `x8 = 222 / 215`. Maintain
   16-byte SP alignment per AAPCS64. Re-enables `Debugger::call`,
   `call::fmt::call_debug_fmt`, and the `vard` / `argd` UI commands.
   Unblocks 2 integration tests (`variables::test_debug_trait_repr_*`).

2. **Aarch64 TLS walker.** Replace `libthread_db` for our use with a
   small in-tree implementation: `PTRACE_GETREGSET(NT_ARM_TLS=0x401)`
   to read `TPIDR_EL0`, then walk glibc's `tcbhead_t` and DTV to find
   each loaded module's TLS block (combined with the DWARF
   `DW_AT_location` TLS-block offset we already parse). Unblocks
   `variables::test_read_tls_*` and the tokio task-context oracle
   (`tokio::test_async0`).

3. **CI matrix.** Add an `ubuntu-24.04-arm` job that runs the full
   test suite (we already have an Earthfile target). Optional KVM
   runner for the watchpoint tests once we find one.

## Time-travel (record-and-replay) debugging

**Goal:** make BugStalker capable of stepping **backwards** —
restore an earlier program state and re-execute deterministically up
to a chosen instruction. This is the single largest user-facing
feature on the wishlist; it sits above the porting work because it
has to compose with everything below.

**Why:** the conventional "set a breakpoint, hope it fires near the
fault" loop is slow on rare bugs. With reversible execution the
workflow becomes "stop at the crash, step backwards until the
invariant first breaks." `rr` has shown this works for native code
on Linux at acceptable overhead.

**Shape we're aiming for (inspired by `rr`, scoped to BugStalker):**

* **Record phase.** Run the debuggee under ptrace as today, but
  intercept every source of nondeterminism and log it to a
  per-process trace file:
  * syscall enter/exit (args, return value, any data buffers
    transferred from kernel → user, e.g. `read`/`recv`/`getrandom`),
  * signals delivered to the tracee,
  * memory-mapped I/O reads,
  * `rdtsc` / `mrs CNTVCT_EL0` / other timing-source reads
    (intercepted by trapping the instruction, x86 via
    `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`, aarch64 via emulation),
  * shared-memory observable-by-other-process reads (single-stepped
    or serialised via scheduling control).
* **Checkpoints.** Every N seconds (tunable) write a full
  copy-on-write snapshot of the tracee's memory plus a register
  snapshot. Snapshots are the entry points for replay.
* **Replay phase.** Restore from the nearest checkpoint, then
  re-execute under ptrace with all syscalls and other
  nondeterministic events served from the recorded log instead of
  the kernel. Memory diverges only because of bugs in the recording
  layer; convergence is verified with periodic
  page-checksum comparisons.
* **Reverse stepping UI.** A `reverse-step` / `reverse-cont` /
  `reverse-finish` set of commands. Internally they binary-search:
  pick a checkpoint earlier than the current point, replay forward
  to one instruction before the current PC, repeat until the target
  is found.

**Open architectural decisions (to settle before code lands):**

* Record/replay engine in-process or as a separate `bs-record`
  binary that produces a trace consumed by `bs --replay <trace>`?
  The latter mirrors `rr record` / `rr replay` and keeps the live
  debugger code path simpler.
* Trace format: framed protobuf, CBOR, or a custom layout?
  Determines whether a trace is portable across host kernels.
* Multi-threaded record. `rr` serialises threads onto a single CPU
  with `sched_setaffinity` and uses the `CPU_PERFCTR` interrupt to
  preempt; we'd need an equivalent. On aarch64 we'd use
  `arm_pmu`-based PMI rather than x86 PMC events.
* Watchpoints during replay: the replay process can use software
  watchpoints (page protection + `SIGSEGV` handler) regardless of
  host hardware-debug support, because we control its execution.
  This is one of the places where time-travel can do something
  forward-only debugging can't.

**Milestones** (rough — none of this has started):

1. Trace format + recorder for the syscall path on linux/x86_64
   single-threaded. Replay enough to step forward through a recorded
   run and observe identical state.
2. Periodic checkpoints (process snapshot via `process_madvise` +
   `userfaultfd` for COW, fall back to dirty-page tracking via
   `/proc/<pid>/clear_refs`).
3. Reverse-step / reverse-continue UI on top of (1) + (2).
4. Multi-threaded recording (scheduler control + thread interleave).
5. Cross-arch (linux/aarch64) recorder once the aarch64 port above
   has TLS and inferior calls in place.
6. UI integration: DAP `stepBack`, `reverseContinue` capability flags
   are already part of the Debug Adapter Protocol; once (3) lands,
   the existing DAP server should be able to expose them with little
   change.

## Native macOS (Darwin/aarch64)

Future, gated behind the Linux/aarch64 port stabilising. Requires a
parallel debuggee backend on top of Mach task ports + exception
ports rather than ptrace, plus codesigning entitlements
(`com.apple.security.cs.debugger`) on `bs` itself. Significant
enough that it's tracked here as awareness, not as imminent work.

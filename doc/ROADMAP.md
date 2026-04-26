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

**Status:** at parity with x86_64 for everything the test suite
exercises. On linux/arm64: 20/20 lib + 75/75 tests/debugger +
75/75 tests/dap + 1/1 doc tests = **171/171 passing, 0 failing,
0 ignored**. The 9 hardware-watchpoint integration tests are
`#[cfg]`-excluded on aarch64 — see "Hardware watchpoints" below.

**Done**

* Compile-ready skeleton: `register/{x86_64,aarch64,debug}.rs` split,
  `BRK #0` software breakpoint, `PTRACE_GETREGSET(NT_PRSTATUS)` for
  general-purpose registers, DWARF register numbering per ARM IHI
  0057.
* `Breakpoint::PC_ADJUST` (x86: 1 — `INT3` reports PC+1; aarch64: 0 —
  `BRK #0` reports PC).
* `disasm.rs` arch-aware byte-restoration when a function under
  disassembly has live breakpoints (1 byte vs 4 bytes).
* `Register::SP` / `Register::PC` / `Register::RA` arch-agnostic
  aliases on both arches, so cross-arch call sites
  (`Debugger::set_pc`, the unwinder, the DAP `goto`/`restartFrame`
  handlers, `tests/debugger/main.rs::test_registers`) don't have to
  spell `rip` / `rsp` / `pc` / `x30` themselves.
* `NT_ARM_HW_WATCH` data watchpoints with WCR/WVR encoding and
  `si_addr`-based hit attribution.
* AAPCS64 inferior calls: `x0..x7` arg marshalling, 16-byte SP
  alignment (also fixes a long-standing flake in
  `test_debug_trait_repr_vars` on x86_64 where SysV-AMD64 alignment
  was being skipped — see `c6ed935`), `BLR x8 ; BRK #0` trampoline,
  `mmap` / `munmap` via `svc #0` with `x8 = 222 / 215`. Re-enables
  `Debugger::call`, `call::fmt::call_debug_fmt`, and the `vard` /
  `argd` UI commands.
* TLS via the `gilescope/thread_db` fork — `proc_service` shims
  use `PTRACE_{GET,SET}REGSET(NT_PRSTATUS|NT_FPREGSET)` on aarch64
  in place of the x86-only `PTRACE_GETREGS` / `PTRACE_GETFPREGS`,
  with a `NoFRegs` stub for `ps_get_thread_area` (aarch64 reads its
  TLS base from `TPIDR_EL0` directly). Unblocks
  `test_read_tls_*` and tokio's task-context oracle.
* CI: `test-arm64` and `lint-arm64` jobs on `ubuntu-24.04-arm` in
  `.github/workflows/ci.yml`, mirroring the x86_64 setup with the
  latest supported rustc.

**Hardware watchpoints — environment caveat**

The aarch64 `HardwareDebugState` implementation is correct: a
`PTRACE_SETREGSET(NT_ARM_HW_WATCH)` write succeeds and the kernel
readback matches what we wrote (`addr=0xfffffffff0c1, en=true`).
But on the only hosted aarch64 environments we can reach today —
Docker Desktop on top of Apple Virtualization.framework, and
GitHub's `ubuntu-24.04-arm` runners — the hypervisor accepts the
ptrace write but never delivers `TRAP_HWBKPT` when the debuggee
touches the watched address. The 9 `watchpoint::*` integration
tests are therefore `#[cfg(target_arch = "x86_64")]`-gated in
`tests/debugger/main.rs` (with the reasoning inline). They should
pass on a KVM host or bare-metal aarch64; flipping the gate is a
one-line change once such a runner is wired up.

**Remaining (small, opportunistic)**

1. **Bare-metal / KVM aarch64 runner** for the
   `watchpoint::*` tests. Drops the `#[cfg]` exclusion in
   `tests/debugger/main.rs:12`.
2. **Upstream the `thread_db` aarch64 work** to godzie44/thread_db,
   then drop the `git = "..."` pin in `Cargo.toml` for a crates.io
   release (`thread_db = "0.1.5"` or similar).

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

**Goal:** debug aarch64-apple-darwin binaries directly from a macOS
host without a Linux VM in the middle. Reuses the arch abstraction
that the Linux/aarch64 port already established (register file, BRK
opcode, AAPCS64 calling convention, `Register::PC`/`SP`/`RA`); the
new surface is the *backend* (how to spawn, attach, stop, read
memory, read registers, install breakpoints).

**Status:** compileable skeleton landed on the
`giles-darwin-aarch64` branch. `cargo check` and `cargo build`
both pass on `aarch64-apple-darwin`. Every linux-specific call
site (`nix::sys::ptrace`, `nix::sys::personality`,
`nix::sys::uio::process_vm_readv`, `libc::PTRACE_*` constants,
`thread_db`) is `#[cfg(target_os = "linux")]`-gated, with a
`#[cfg(not(target_os = "linux"))]` darwin parallel that
`unimplemented!()`s with a comment naming the Mach API to use.

**Done**

* `build.rs` accepts macOS/aarch64 (darwin/x86_64 explicitly not
  supported); skips the `--export-dynamic` linker flag (ld64 doesn't
  have it).
* `thread_db` Cargo dep moved under
  `[target.'cfg(target_os = "linux")'.dependencies]`. The
  `thread_db_compat` shim's stub branch covers darwin too.
* Linux ptrace path gated to `target_os = "linux"` in:
  `process.rs` (spawn/attach), `tracer.rs` (wait/event loop),
  `tracee.rs` (per-thread state — `tls_base` already
  lives behind a `NoThreadDB` error path), `breakpoint.rs`
  (software-bp install/remove via `PTRACE_POKEDATA`),
  `register/aarch64.rs::current`/`persist`,
  `register/aarch64.rs::debug_impl` (hw watchpoints),
  `debugee/dwarf/unit/die_ref.rs::read_tpidr_el0` (TPIDR_EL0
  reader),
  `debugee/rendezvous.rs` (entire GNU `r_debug` walker),
  `Debugger::write_memory`, `read_memory_by_pid`. Each darwin stub
  carries a comment naming the Mach API the runtime impl will use.

**Phase 2 — first end-to-end stop (the POC)**

1. **Spawn + attach.** `posix_spawn` with
   `_POSIX_SPAWN_DISABLE_ASLR` (= `0x0100`) so addresses are
   deterministic. `task_for_pid()` (requires the
   `com.apple.security.cs.debugger` entitlement on `bs` itself, and
   `get-task-allow` on the debuggee — easy when we control the
   build) returns the Mach task port. `task_set_exception_ports()`
   subscribes to `EXC_MASK_BREAKPOINT | EXC_MASK_SOFTWARE |
   EXC_MASK_BAD_ACCESS`.
2. **Stop loop.** A dedicated thread `mach_msg_receive`s on the
   exception port; translates an incoming `mach_exception_raise`
   into the same `StopReason` shape `Tracer::resume` returns on
   linux today.
3. **Memory I/O.** `mach_vm_read_overwrite` for reads;
   `mach_vm_write` framed by `mach_vm_protect(VM_PROT_READ |
   VM_PROT_WRITE | VM_PROT_COPY)` for writes (because text pages
   are normally `r-x`).
4. **Registers.**
   `thread_get_state(thread, ARM_THREAD_STATE64, &state, &count)`
   / `thread_set_state` for the GP set;
   `ARM_NEON_STATE64` for vector regs;
   `ARM_DEBUG_STATE64` for the WCR/WVR slots needed for hardware
   watchpoints.
5. **Breakpoint write.** Same `BRK #0` opcode and `PC_ADJUST = 0`
   that already work on linux/aarch64; only the write path
   changes (vm_protect dance + mach_vm_write).
6. **First test:** `bs ./hello_world` spawns, hits a breakpoint at
   `main`, prints the source line, single-steps, continues, the
   debuggee exits cleanly.

**Phase 3 — parity with linux/aarch64**

* TLS via `thread_get_state(ARM_THREAD_STATE64)` →
  `pthread_self` pointer → walk `_pthread_t` to find the per-image
  TLS slot (or the dyld TLV records). Replaces the
  `thread_db_compat` stub.
* Module discovery via `task_info(TASK_DYLD_INFO)` →
  `dyld_all_image_infos` → walk image array. Replaces the linux
  `r_debug` rendezvous walker.
* DWARF in `.dSYM/Contents/Resources/DWARF/<binary>` bundles. The
  `object` crate already parses Mach-O; `gimli` is portable. The
  `dsymutil`-produced bundle is the macOS equivalent of separate
  debug info and we'll need to follow the `LC_UUID` from the
  binary to find the matching dSYM.
* Multi-thread support via Mach `task_threads()` + per-thread
  exception subscription.
* AAPCS64 inferior calls — already done at the trampoline level on
  linux; only the `mach_vm_write`-of-the-shellcode and `task_resume`
  details change.
* DAP: same as on linux once the underlying `Debugger` works.

**Open architectural decisions**

* **Codesigning of `bs`.** `task_for_pid` requires
  `com.apple.security.cs.debugger`. We can either (a) ship
  pre-signed release binaries via `cargo dist` + a Developer ID
  certificate, (b) document a `codesign --entitlements …`
  one-liner in the install instructions, or (c) both. (a) is what
  LLDB does; (b) is what `rust-gdb` users do. Pick before the POC
  ships.
* **SIP-protected debuggees.** Standard user binaries are fine.
  System binaries (anything in `/System`) need SIP partial-disable
  (`csrutil enable --without debug` from recovery). Document but
  don't try to work around.
* **Backend abstraction.** The current cfg-gated approach (parallel
  `#[cfg(target_os = …)]` impls per function) gets us to the POC
  fastest. Once both backends are real, refactor to a `Backend`
  trait so a third backend (e.g. *BSD, or a record-and-replay
  backend for time-travel) is a clean addition rather than
  another arm of every cfg.

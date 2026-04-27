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

### Done

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

### Hardware watchpoints — environment caveat

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

### Remaining (small, opportunistic)

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

**Status:** the POC is in. A debuggee spawns, hits a `BRK`-installed
breakpoint, and the engine surfaces it back to the front-end (the
`debugger_runs_to_first_breakpoint` smoke test under
`tests/darwin_smoke.rs`). The Mach exception-port loop
(allocate / register / receive / reply) is wired up but not yet
substituted for the ptrace-driven `Tracer::resume`; the cutover is
the next big chunk. `cargo check` and `cargo build` pass
unconditionally on `aarch64-apple-darwin`; `cargo test` runs the
non-entitlement-gated smokes (`exception_port_*`) on every macOS
host.

### Done — Phase 2 (POC)

* `build.rs` accepts macOS/aarch64 (darwin/x86_64 explicitly not
  supported); skips the `--export-dynamic` linker flag (ld64 doesn't
  have it).
* `thread_db` Cargo dep moved under
  `[target.'cfg(target_os = "linux")'.dependencies]`. The
  `thread_db_compat` shim's stub branch covers darwin too.
* Linux ptrace path gated to `target_os = "linux"`; every linux-
  specific call site has a `#[cfg(not(target_os = "linux"))]`
  darwin parallel naming or using the Mach API. Affected:
  `process.rs`, `tracer.rs`, `tracee.rs`, `breakpoint.rs`,
  `register/aarch64.rs`, `debugee/dwarf/unit/die_ref.rs`,
  `debugee/rendezvous.rs`, `Debugger::write_memory`,
  `read_memory_by_pid`.
* `darwin_mach.rs`: Mach shim — `task_for_pid`, `vm_read_n`,
  `vm_write_word` (with `VM_PROT_READ|WRITE|COPY` framing for r-x
  text pages), `task_threads_vec`, `thread_get_arm_state64` /
  `thread_set_arm_state64`, `arm_debug_state64_t` round-trip,
  `dyld_image_list` (TASK_DYLD_INFO walk).
* DWARF via `dsymutil`-produced
  `<binary>.dSYM/Contents/Resources/DWARF/<basename>`; the
  loader follows the `LC_UUID` of the binary to find the matching
  dSYM.
* Hardware watchpoints via `ARM_DEBUG_STATE64` (WCR/WVR/MDSCR_EL1
  identical encoding to linux). `HardwareDebugState::sync` writes
  the slot table to **every** thread of the task —
  `task_threads()` + per-thread `thread_set_state` — so worker-
  thread accesses don't miss the watch.
* AAPCS64 inferior calls (`Debugger::call`, `vard`, `argd`,
  `fmt::call_debug_fmt`): syscall trampoline parameterised by
  `syscall_abi` submodule — darwin uses x16 for the syscall
  number, `svc #0x80`, BSD nrs (mmap=197, munmap=73), and reads
  CPSR.C (bit 29 of pstate) for success/failure rather than
  `x0 == -errno`. Linux behaviour unchanged.
* `ExceptionPort`: allocate (receive right + insert send right),
  register on `EXC_MASK_{BREAKPOINT,SOFTWARE,BAD_ACCESS}` with
  `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES`, `receive(timeout)`
  via `mach_msg(MACH_RCV_MSG | MACH_RCV_TIMEOUT)` decoding
  `mach_exception_raise` (msg id 2405) into a `ReceivedException`,
  `reply` writing the 36-byte `__Reply__mach_exception_raise_t`
  with `MACH_MSG_TYPE_MOVE_SEND_ONCE` and `MACH_SEND_TIMEOUT`.
* Codesigning workflow documented in the smoke-test header;
  `tests/darwin.entitlements` ships `com.apple.security.cs.debugger`
  and `get-task-allow`. The entitlement-gated smokes are `#[ignore]`
  so an un-codesigned `cargo test` run is clean.
* Hardware watchpoint **hit detection** via `ARM_EXCEPTION_STATE64`'s
  `FAR_EL1` field — the macOS analogue of linux's
  `siginfo.si_addr` for a `TRAP_HWBKPT` SIGTRAP. The darwin Tracer
  classifies SIGTRAP into Breakpoint (PC matches BP registry) or
  Watchpoint (FAR matches an armed slot's BAS-encoded byte set)
  before falling through to SignalStop.
* Quiet-signal pass-through in `Tracer::resume` and
  `Tracer::single_step`: SIGALRM, SIGURG, SIGCHLD, SIGIO,
  SIGVTALRM, SIGPROF are re-injected via `ptrace::cont(Some(sig))`
  rather than dropping to a user prompt — matches the linux
  `QUIET_SIGNALS` policy.
* `MachError` carries human-readable diagnostics: `Display` decodes
  the `kern_return_t` to a name + context (e.g. `KERN_FAILURE` →
  *"missing com.apple.security.cs.debugger entitlement on the
  caller"*); `From<MachError> for Error` logs the full form before
  collapsing to `Errno::EFAULT` so log greps surface the real cause.
* dyld notification BP at `dyld_all_image_infos.notification`
  (offset 16) — the macOS analogue of `r_debug.r_brk` for module-
  load tracking. `Rendezvous::r_brk` returns this address;
  installing a BP there fires on every dlopen/dlclose.
* TLS read on darwin returns `ENOSYS` instead of panicking — real
  resolution needs the dyld TLV walker plus the per-thread TSD
  array out of `pthread_t`; until then the surrounding
  `weak_error!` surfaces "no TLS for this variable" gracefully
  rather than crashing the debugger.

### Done — Tracer cutover (path A, pure Mach)

* **`Child::install`** rewritten to `posix_spawnp` with
  `POSIX_SPAWN_START_SUSPENDED`; no ptrace involvement on the
  spawn path. The kernel parks the inferior at creation; the
  parent gets `task_for_pid` rights for free (parent/child
  relationship, no entitlement needed for the spawn case).
* **`Tracer::resume`** drives through the Mach loop: reply to
  the previously-saved `(remote_port, msg_id)` to release the
  parked faulting thread, `task_resume`, `port.receive` blocking,
  `task_suspend` to coherent the rest of the threads while we
  classify, save the new pending reply, return `StopReason`.
* **Classifier** uses Mach exception type + codes directly:
  EXC_BREAKPOINT(6)+EXC_ARM_BREAKPOINT(1) → software BP;
  EXC_BAD_ACCESS(1)+EXC_ARM_DA_DEBUG(0x102) → HW watchpoint
  (codes[1]=FAR_EL1); EXC_SOFTWARE(5)+EXC_SOFT_SIGNAL(0x10003) →
  Unix signal.
* **`Tracer::pause`** is `task_suspend` (was `kill(SIGSTOP)`).
* **First call** synthesises `DebugeeStart` from the
  spawn-suspend state; subsequent calls drive the receive loop.
* All 3 entitlement-gated smoke tests pass on the cutover code:
  `spawn_and_read_pc`, `exception_port_allocate_and_register`,
  `debugger_runs_to_first_breakpoint`.

### Done — Mach-native CallHelper (`69dd33e`)

The trampoline driver chose path (C) from the design notes
below: `Cell<Option<(u32, i32)>>` on
`DarwinSupervision::pending_reply` lets the trampoline mutate
the port's reply state via `&Tracer` — the FFI-opaque
mutability linux gets for free through `ptrace::cont`/`step`.

`CallHelper::drive_one(ccx, single_step)` is the per-step
primitive: arm SS bits if needed, reply prior pending exception
(releases the parked thread), `task_resume`, `port.receive`
blocking, `task_suspend`, save new pending reply, optionally
disarm SS. `mmap`/`munmap`/`jump` use `single_step=true` (one
instruction); `call_fn` uses `single_step=false` (run-to-BRK).

Plumbing: `pub(crate) Debugger::debugee()` →
`Debugee::tracer()` → `Tracer::darwin_state()` exposes the
supervision state through `&Debugger`; `DarwinSupervision`
gains `task()`/`port()`/`take_pending_reply()`/`set_pending_reply()`,
all `&self`-callable thanks to the `Cell`. No `&mut` cascade.

Validated: 3/3 entitlement-gated tests pass; 13/13 non-ignored
darwin smokes; 20/20 lib tests; earthly +check clean. The
design notes below stay for context.

### Inferior calls — design notes (resolved by `69dd33e`)

The trampoline driver (`CallHelper::mmap` → `jump` → `call_fn` →
`munmap`) on aarch64 still uses `ptrace::cont`/`step` + `waitpid`,
which is incompatible with the pure-Mach Tracer cutover. A
Mach-native rewrite needs:

1. Allocate a temp `ExceptionPort`.
2. `swap_in_temp_exception_port(EXC_MASK_BREAKPOINT, temp)` to
   capture the current chain (the Tracer's port goes there) and
   install ours. Atomic swap — already implemented in `31c37d4`.
3. **Release the Tracer's parked thread.** When `CallHelper` is
   invoked, the inferior's faulting thread is parked at the most
   recent stop; the kernel is waiting for the Tracer's
   `pending_reply` to advance it. We must take that reply, send
   `KERN_SUCCESS`, then write trampoline code, set regs, and
   block on the temp port for the next exception.
4. For each trampoline step (mmap/jump/call/munmap):
   * Write the trampoline instruction at PC.
   * Set up registers (syscall args, x8/x16 = nr).
   * For BRK-terminated trampolines (`call_fn`): just resume.
      For single-instruction steps (`jump`, `mmap`, `munmap`):
      `arm_set_single_step` + resume.
   * Reply prior pending → kernel resumes thread → trampoline
      executes → exception fires → temp port receives.
   * Save new pending reply for the next step.
5. After the last step's reply: `restore_exception_ports` to put
   the Tracer's port back. Update `Tracer::darwin_state.pending_reply`
   so it reflects the final exception state on our restored port
   (or clear it; the next `Tracer::resume` call needs a coherent
   starting state).

**Architectural friction**: step 3 + step 5b need
`&mut Tracer` access (or interior mutability via `Cell` /
`RefCell`). The current code path is
`Debugger::call (&mut)` → `with_disabled_brkpts (&self)` →
`call_fn (&self)` → `call_fn_raw (&self)` → `CallHelper::* (&CallContext)`.
Routing `&mut Tracer` through requires either:

* propagating `&mut self` from `call_fn_raw` up through
  `with_disabled_brkpts`, `call_fn`, and the closure shape — and
  also through `call_debug_fmt` and the `Print Handler` chain
  (cascades into `ui::command::print` and the TUI's
  `tui::components::variables`); or
* making `DarwinSupervision::pending_reply` a `Cell<Option<…>>`
  and adding `Tracer::darwin_state(&self) -> Option<&_>` plus a
  `pub(crate) fn debugee(&self) -> &Debugee` on `Debugger` so
  `CallHelper` can reach the cell from `&CallContext.dbg`.

The Cell approach is less invasive but needs a new public-ish
surface on `Tracer` and `Debugger`. Either way, the refactor is
~150–250 lines spread across 4–6 files. Until it lands,
`vard`/`argd`/`fmt::call_debug_fmt`/`Debugger::call` return
`CallError::Mmap` on darwin (clear failure rather than corrupt
state).

### Status — `tests/debugger` on darwin/aarch64

Running with `--test-threads=1 --skip multithreaded --skip tokio
--skip signal --skip test_step_over_for_loop_issue_156 --skip
test_read_tls`: **62 passed, 0 failed, 1 ignored, 12 filtered out
(75 runnable)**.

No remaining runnable failures in this filter set.

#### LinkerMapFn rendezvous — done via Mach IPC

We use [`task_dyld_process_info_notify_register`](
https://github.com/apple-oss-distributions/dyld/blob/main/libdyld/dyld_process_info_notify.cpp)
to subscribe a Mach port to dyld's image-load/unload event stream
(see `darwin_mach::DyldNotifyPort`). Wire format is decoded from
`libdyld/dyld_process_info_internal.h` —
`dyld_process_info_notify_header` followed by
`dyld_process_info_image_entry[]` and a string pool. Every message
dyld sends is synchronous (`mach_msg(MACH_SEND_MSG | MACH_RCV_MSG)`)
so we reply to all of them, including LOAD/UNLOAD, or dyld wedges
in `mach_msg_overwrite`. When a LOAD or UNLOAD arrives and a
`LinkerMapFn` BP is registered, the tracer suspends the task and
synthesises `StopReason::Breakpoint(linker_map_addr)` so the
existing higher-level handler runs the deferred-BP refresh.

The legacy `_allImageInfo->notification` SW-BP is left wired up
alongside — it still fires for the cases where it works, and is
harmless when it doesn't.

Two related fixes lit up at the same time:

1. **`Rendezvous::link_maps()` re-walks `dyld_image_list` on every
   call** (was a stale snapshot from `Rendezvous::new`). Without
   this `update_debug_info_registry` couldn't see newly `dlopen`-ed
   dylibs.
2. **`DwarfRegistry::update_mappings` consults dyld's `infoArray`
   for each dylib's `imageLoadAddress`**, not the lowest-VA
   `proc_pidinfo` region. On darwin the kernel keeps a parse-time
   mmap of every dylib at a low VA in addition to the slid runtime
   mapping, and `min_by(start)` was picking the parse region — so
   every BP install into a dlopen-loaded dylib EFAULT'd at the
   wrong address.

#### Debug::fmt empty buffer — fixed

The vtable / formatter layout was correct all along; what
silently swallowed the call was page-protection drift. The
inferior-call data scratchpad is `mmap`-ed `R+W` in the
inferior, then `Debugger::write_memory` (a.k.a.
`darwin_mach::vm_write_word`) lays out the String header,
vtable, and Formatter struct on it. The earlier `vm_write_word`
hardcoded the post-write protection to `R+X` — fine for BP
installs into text, but it tightened our scratch page from
`R+W` to `R-X`, so the *first* inferior store into the buffer
(e.g. `do_reserve_and_handle`'s `stp x20, x8, [x19]` when the
empty `String` had to grow to fit `"["`) raised
`KERN_PROTECTION_FAILURE` and the call returned without ever
reaching `write_str`'s `memcpy`. Reading the dropped String
header back showed the original empty-Vec sentinel, which is
how the symptom looked like a vtable miss.

Fix: `vm_write_word` snapshots the page's current protection
via `mach_vm_region` and restores *exactly that* after the
write. The trampoline page (also `mmap`-ed `R+W`, since darwin
W^X bars `PROT_EXEC | PROT_WRITE` without `MAP_JIT`) now needs
an explicit nudge to `R+X`, which `CallHelper::call_fn` does
via `darwin_mach::vm_protect_rx` after writing
`BLR x8 ; BRK #0`. A separate usize-underflow in
`BsUnit::find_exact_place_by_pc` (pre-existing, but only
reachable once the call chain ran far enough to hit a `pc==0`
binary-search hit) was uncovered along the way.

Recent darwin-specific fixes in this phase:

* CU disambiguation when dsymutil's `low_pc/high_pc` engulfs other
  CUs (`95e477d`).
* Mach-O eh_frame BaseAddresses use `__eh_frame` etc. (`3b3c514`).
* Dylib slide computed from per-file `__TEXT.vmaddr`, not assumed 0
  (`c08b87a`).
* `mmap` for inferior calls drops PROT_EXEC (W^X EACCES) (`d486a1c`).
* Stray BRK in dyld pages no longer surfaces as SignalStop SIGTRAP
  (`b8a3965`).
* `Tracee::location()` falls back to identity when registry has no
  mapping for PC (e.g. internal stops in dyld).
* `restart_debugee` no longer fails on LinkerMapFn BPs (`166e16b`).
* Mach-O symbol-name regex strips leading `_` (`0ec4e1e`).
* Test fixtures: `.comment` fallback to workspace MSRV; backtrace
  test asserts a sane lower bound rather than libc-startup-chain
  exact count.
* Test runner self-signs + re-execs if missing `cs.debugger`.

### Phase 3 — parity with linux/aarch64

* **Cut `Tracer` over from ptrace+SIGTRAP to Mach exception ports.**
  Today's darwin `Tracer::resume` does `ptrace::cont` + `waitpid`,
  classifying SIGTRAP as a breakpoint by matching PC against the
  registry. The post-BP single-step path is racy because the
  ptrace event is the only stop signal we have. Wiring
  `ExceptionPort::receive` as the primary stop source (decoding
  EXC_BREAKPOINT, EXC_BAD_ACCESS-as-watchpoint, EXC_SOFTWARE) and
  `reply(KERN_SUCCESS)` as the resume primitive lets us drop the
  ptrace stop-signal coupling and gives multi-thread for free.
* **TLS reads.** `thread_get_state(ARM_THREAD_STATE64)` doesn't
  expose `TPIDRRO_EL0` (the pthread pointer on darwin); we have
  to follow the dyld pthread struct layout via `mach_vm_read`.
  Replaces the `thread_db_compat::NoThreadDB` stub.
* **Multi-thread tracee enumeration.** `TraceeCtl` currently only
  knows about the main thread; `task_threads()` enumeration
  populates the rest. Mach thread ports are u32 names — we need a
  stable mapping into the `Pid`-typed `TraceeCtl` API.
* **Module-load notifications.** `dyld_image_list` is a snapshot;
  for runtime dlopen/dlclose tracking we install a software BP on
  `dyld_all_image_infos.notification` and re-walk the image array
  on each fire (the linux equivalent is the `r_brk` rendezvous
  callback).
* **DAP.** Free once the underlying `Debugger` works end-to-end
  with the exception-port loop.

### Open architectural decisions

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

# Phase 5 — Time-travel debugging

Reverse step. Set a watchpoint and rewind to the write that put the
bad value there. Replay the last few seconds with different
breakpoints. The single most-requested debugger feature in interactive
Rust development.

## Three tiers

The architectural insight is that "time travel" is not one feature but
three at very different cost points.

| Tier | Mechanism | Cost during run | Window | Determinism |
| ---- | ----------------------------- | --------------- | ------ | ----------- |
| 1 | Intel PT trace decode | ~1–5 % CPU | seconds | exact |
| 2 | `fork(2)` checkpoint replay | nil until checkpoint | minutes | best-effort |
| 3 | Full record-and-replay (rr-style) | ~10–30 % CPU | hours | exact |

Implement in tier order. Tier 1 ships with Phase 6 essentially for
free. Tier 2 is moderate effort, immediately useful. Tier 3 is a
multi-month commitment but delivers the canonical experience users
expect from `rr`.

## Tier 1 — Intel PT reverse step

The trace buffer Phase 6 captures (`crates/bs-perf/` Intel PT
back-end) is *also* a complete record of what the CPU did. The PT
decoder produces a sequence of executed instructions; Phase 6 uses it
for cycle attribution, but the same sequence supports reverse stepping
here.

### What the user sees

```text
(bs) rstep
stepped backward 1 instruction; now at src/handler.rs:42
(bs) rnext
stepped backward 1 line; now at src/handler.rs:41
(bs) rcontinue
stopped at breakpoint #2 (src/middleware.rs:18); 142 ms ago in run
```

Same UX as forward stepping, prefix `r`.

### Implementation

1. PT decoder produces a `Vec<Pc>` (or compressed equivalent) of
   executed PCs since the last stop.
2. Reverse-step iterates that vector backwards: the previous PC is
   the previous instruction.
3. Reverse-next finds the previous PC whose `(file, line)` differs
   from the current one's.
4. Reverse-continue scans backwards for a PC matching any active
   breakpoint location.
5. To inspect *state* at a past PC, we cannot read memory directly
   (the debuggee has moved on). Two options:
   - **Symbolic re-execution** (limited): for simple expressions
     local to one function, we can re-derive values from register
     deltas the PT trace records. Works only for a small subset.
   - **Defer to Tier 2 if installed**: if a checkpoint exists at or
     before the target time, fork from it and run forward to the
     target. Real memory state.

### Limitations

- Window-bound: PT buffer is finite (~64 MB → ~5 s of trace at
  typical throughput). Reverse stepping works only inside the window.
- No memory state at past PCs unless combined with Tier 2.
- Linux x86 only (PT availability).
- No syscall replay — if the user reverse-steps over an `open`,
  the file descriptor that was returned does not "un-open".

These limits are exactly why Tier 2 and Tier 3 exist.

### Effort

~2 weeks engineer-time on top of Phase 6's PT integration. Bulk of
work is the UX commands and the `rcontinue` breakpoint scanning.

## Tier 2 — Checkpoint-based replay

Periodically `fork(2)` the debuggee at safe points; keep the forks
suspended; replay forward from a checkpoint to land on a target time
with full memory state.

### Mechanism

1. **Checkpoint cadence.** On every breakpoint stop, optionally on
   every N seconds wall-clock, optionally on every M function calls
   (heuristic). Configurable.
2. **The fork.** `fork(2)` produces a frozen-in-time copy of the
   debuggee's address space. Send it `SIGSTOP` immediately. The
   parent debuggee continues; the child sleeps.
3. **Storage.** Fork PIDs in a ring buffer (e.g. last 32 checkpoints).
   When the buffer fills, oldest gets `SIGKILL`.
4. **Replay.** To inspect state at an earlier PC:
   a. Find the latest checkpoint at or before the target time.
   b. Attach BugStalker to the fork (`PTRACE_SEIZE` or equivalent).
   c. Set a one-shot breakpoint at the target PC.
   d. `SIGCONT` and let it run forward.
   e. On hit, the user inspects state. Detach and `SIGKILL` the
      fork when done.
5. **Combined with Tier 1.** PT's exact PC sequence picks the precise
   instruction to land on; the checkpoint provides the live memory
   state at that instruction.

### Determinism caveats

- Forks share file descriptors initially but diverge as either side
  performs I/O. Replay across an I/O syscall is approximate — the
  syscall in the replay returns the *same* result only if the
  underlying state hasn't changed.
- Network I/O, randomness, timing — all non-deterministic. For UI
  work and pure-compute bugs, this is fine. For race conditions,
  it isn't (Tier 3 fixes this).
- Threads: each fork captures all threads at that instant. But
  scheduling on replay differs from the original. Multi-threaded
  bugs replay imperfectly.

### Storage cost

Each `fork` is initially copy-on-write — cheap. As the parent
continues to mutate memory, COW pages diverge. For a process with a
1 GB working set diverging at 100 MB/s, a 32-checkpoint buffer at
1-second intervals consumes ~32 GB at the worst case but typically
much less. Tunable.

### Implementation

New crate `crates/bs-replay/` with:

- `linux/fork_checkpoint.rs` — `fork()` + `ptrace` plumbing.
- `darwin/checkpoint.rs` — Mach equivalent. Apple does not expose
  `fork`-with-ptrace cleanly; fall back to `mach_vm_remap` snapshots
  of writable regions (heavier but functional).
- `replay.rs` — pick checkpoint, attach, drive forward to target.

### Effort

~4 weeks engineer-time. Linux is straightforward; Mach equivalent
on Darwin is ~2 weeks of that.

## Tier 3 — Clean-room deterministic record-and-replay

Deterministic re-execution across arbitrary intervals. This is **the
big one** and is a planned, scoped, in-scope deliverable — not a
compromise wrapper around `rr`. Built from scratch under MIT/Apache,
no GPL contamination from rr/Pernosco code.

The canonical existing implementations (`rr`, `Pernosco`) are GPL.
We do not vendor them. We do not wrap them as a shipping feature.
We may *consult their behaviour* for differential testing — running
the same workload through both record engines and asserting matching
syscall logs — but no code copies.

### Why clean-room, planned upfront

The user's directive: time-travel debugging is a strategic feature,
not a side project to be tackled "if there's time." Wrapping rr would
make BugStalker permanently dependent on a GPL component the user
must install separately, restrict us to Linux x86-64 (rr's only
supported target), and tie us to whatever rr does and doesn't fix.
A first-party engine puts the feature on equal footing with the rest
of the debugger.

The time-cost is honest: 6–12 months single-engineer to an MVP that
covers single-threaded Linux x86-64 + multi-threaded with single-CPU
serialisation. aarch64 and longer-tail features add to that. We plan
for it now so the architecture supports incremental delivery from
month 3 onwards.

### Sources of non-determinism we record

Every input to the program that is not the program's own code:

| Source | Mechanism |
| ------------------------------------- | ---------------------------------- |
| Syscall return values + side-effect data | `seccomp-bpf` notify + `ptrace` |
| `RDRAND`, `RDSEED` | `CPUID` flag clear; trap `#UD` on use |
| `RDTSC`, `RDTSCP` | `PR_SET_TSC = PR_TSC_SIGSEGV` |
| `CPUID` | seccomp emulate or trap-and-virtualise |
| Signal delivery (timing + payload) | ptrace signal-delivery-stop |
| `mmap` / `mremap` / `brk` results | logged as syscall results |
| vDSO calls (`gettimeofday`, etc.) | virtual-DSO patching at startup |
| Shared-memory races (multi-threaded) | single-CPU serialisation (see 3F) |
| Atomic instruction interleaving | implicit from single-CPU execution |
| Initial state (env, args, fds, cwd) | snapshot at record start |

The replay engine re-launches the binary from the initial-state
snapshot, drives execution forward via ptrace, and supplies recorded
results in place of every syscall and trapped instruction. The
program's *own* code, registers, and memory writes happen exactly as
they did originally.

### Sub-phases

Tier 3 is structured as nine sub-phases, each independently
shippable. MVP = 3A through 3E + 3I; multi-threaded MVP adds 3F;
aarch64 is 3G; PT integration is 3H.

#### 3A. Trace format and storage

- New crate `crates/bs-replay-engine/`
- Trace file is a directory: `manifest.toml` + numbered `event-*.zst`
  segments + periodic `checkpoint-*.snap` snapshots
- Manifest carries: build-id of recorded binary (cross-checked at
  replay), kernel version, CPU feature set, recording engine
  version, initial environment
- Event records: variable-length, length-prefixed, zstd-compressed
  per segment via the **pure-Rust `ruzstd` crate**
  (<https://github.com/KillingSpark/zstd-rs>) at
  `CompressionLevel::Fastest` (≈ level 1). ruzstd's encoder is, in
  the maintainer's words, "usable, but does not yet reach the speed,
  ratio or configurability of the original zstd library" — that's
  acceptable for replay traces, where recording speed matters more
  than density. Segment size ~16 MB; rotated on size or time. No C
  linkage. The on-disk format is RFC 8478 stable, so a future swap
  to a faster pure-Rust encoder (or upstream improvements to ruzstd)
  is a drop-in change.
- Periodic full-process snapshots (every N seconds or M syscalls)
  enable replay seek without scanning from start
- Format versioning from day 1; bump on incompatible change
- Effort: ~3 weeks

#### 3B. Single-threaded syscall record (Linux x86-64)

- `seccomp-bpf` filter installed in tracee at fork: every syscall
  triggers `SECCOMP_RET_USER_NOTIF`
- Tracer reads notification, reads syscall args from registers,
  allows the syscall (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`), then
  reads result registers and any output buffers via `process_vm_readv`
- All of this written to the trace as a syscall event
- ~340 Linux x86-64 syscalls; we handle each by reading the right
  output buffers (e.g. `read(fd, buf, n)` → log `n` bytes from `buf`
  on success)
- Includes `io_uring` recording: SQ/CQ shared-memory tracking via
  `userfaultfd`, per-SQE/CQE event logging, replay writes recorded
  CQEs back into the shared ring memory at recorded timestamps. See
  the `io_uring` design note below
- Effort: ~9 weeks (the long tail of "every syscall, every flavour")

#### 3C. Single-threaded replay

- Re-launch the binary with recorded environment + cwd + args + fd
  table reconstituted via `posix_spawn_file_actions`
- Same `seccomp-bpf` filter, but on each notification we *do not*
  let the syscall through; instead, we read the next syscall event
  from the trace, write the recorded output buffers back into the
  tracee's address space, and set return registers from the log
- Result: program executes exactly as recorded, byte-for-byte
- Mismatch detection: if syscall args at replay differ from recorded,
  the trace is corrupt or the binary changed — fail loudly with
  diff report
- Effort: ~4 weeks

#### 3D. Non-deterministic instruction trapping

- Mask `RDRAND`/`RDSEED` from `CPUID` advertisements at fork (so
  glibc/openssl/etc. don't use them); on replay, supply same values
- `PR_SET_TSC = PR_TSC_SIGSEGV` makes `RDTSC` raise `SIGSEGV`;
  trap, log/replay value
- vDSO is more subtle: glibc's `gettimeofday` calls vDSO directly,
  bypassing seccomp. Solution: at fork, find the vDSO mapping via
  `/proc/self/maps`, overwrite the entry points with `int 0x80`
  (or `syscall`) so they enter the kernel and trip seccomp like any
  other syscall. (rr does this; the technique is public.)
- Effort: ~3 weeks

#### 3E. Signal recording and replay

- `PTRACE_O_TRACESYSGOOD` to distinguish syscall traps from signal
  stops
- Async signals: log signal number, siginfo, and exact PC where
  delivered (instruction count from PT or single-step counters)
- On replay, deliver the same signal at the same PC via
  `PTRACE_SETSIGINFO` + step-and-deliver
- Synchronous signals (`SIGSEGV` from program faults) are replayed
  implicitly because the same instructions run and produce the
  same fault
- Effort: ~3 weeks

#### 3F. Multi-threaded with single-CPU serialisation

- Pin the recorded process to a single CPU via `sched_setaffinity`
- All threads execute on one core; thread switches happen only at
  syscalls and timer interrupts
- Log every context switch with switching thread + count of
  retired instructions on outgoing thread (via PMU counter)
- On replay, drive threads one at a time, each running for exactly
  the recorded instruction count before yielding to the next
- Cost: ~5× slowdown during recording for CPU-bound workloads
- Document the limitation clearly; it is the same trade rr makes
- Effort: ~6 weeks

#### 3G. aarch64 port

- ARM lacks seccomp-notify in older kernels; require Linux ≥ 5.5
- `RDTSC` analogue is `MRS CNTVCT_EL0` — trap via `PR_SET_TSC`
  equivalent (less mature on ARM; may need ptrace-singlestep workaround
  for older kernels)
- vDSO on aarch64 has different entry points; same patching technique
- Effort: ~6 weeks

#### 3H. PT integration

- When Intel PT is available (Phase 6 substrate), the PT trace
  *replaces* much of what 3F has to log: instruction counts between
  context switches come from PT, syscall boundaries from PT branch
  records
- Reduces recording overhead from ~5× to ~2× on PT-capable hardware
- Optional optimisation; the engine works without it
- Effort: ~4 weeks

#### 3I. BugStalker driver integration

- New crate `bs-replay-driver` exposes the same ptrace-event surface
  BugStalker's existing tracee plumbing already consumes
- BugStalker doesn't know whether it's attached to a live process or
  a replay; same breakpoints, same watchpoints, same step semantics
- The `replay-load <trace>` command instantiates the engine, attaches,
  drives forward to the trace's start, awaits user commands
- Effort: ~3 weeks

### macOS strategy

Tier 3 on Darwin is genuinely hard:

- No seccomp; closest equivalent is `EndpointSecurity.framework`
  which requires a system extension and entitlement (not viable
  for an end-user tool)
- DTrace can observe but not modify syscall results; insufficient
  for replay
- `dyld` interposing (`DYLD_INSERT_LIBRARIES`) bypasses many
  syscalls Apple does internally
- KAuth was removed; no kernel-level interposition surface remains
  for non-privileged tools

Realistic options on Darwin:

1. **`DYLD_INSERT_LIBRARIES`-based syscall shim** — covers `libsystem`
   syscalls, misses anything bypassing libc. Incomplete but useful
   for many programs.
2. **Mach-task port + thread suspension + user-space single-stepping**
   — heavy but functional; recordable subset.
3. **Defer until Apple opens a syscall-interposition interface.**

Recommendation: **3I-Darwin sub-phase ships option 1** as a
best-effort tier-3 on Darwin; programs that bypass libc don't
record-and-replay, but the majority do. Tier 1 and Tier 2 work fully
on Darwin regardless. Effort: ~6 weeks.

### Differential testing against rr

We do not ship rr-wrapper as a feature. We *do* use rr in CI as a
test oracle:

- For a corpus of small test programs, record with both rr and our
  engine
- Compare syscall logs (normalised — same syscall numbers, same
  arg patterns, same return values)
- Discrepancies are bugs in our engine
- This is a test-only dependency; rr is in `dev-dependencies` only
  and never linked into release builds

This gives us behavioural confidence without GPL contamination.

### Architecture

```text
crates/bs-replay-engine/src/
├── format/                      # 3A
│   ├── manifest.rs
│   ├── segment.rs
│   ├── checkpoint.rs
│   └── version.rs
├── record/
│   ├── linux/
│   │   ├── seccomp.rs           # 3B
│   │   ├── ptrace_driver.rs     # 3B
│   │   ├── instrs.rs            # 3D — rdrand/rdtsc/cpuid
│   │   ├── vdso_patch.rs        # 3D
│   │   ├── signals.rs           # 3E
│   │   ├── thread_sched.rs      # 3F — single-CPU serialisation
│   │   └── pt_assist.rs         # 3H
│   └── darwin/
│       └── dyld_shim.rs         # macOS option 1
├── replay/
│   ├── linux/
│   │   ├── shim.rs              # 3C — supply syscall results
│   │   ├── scheduler.rs         # 3F — drive threads in order
│   │   └── ...
│   └── darwin/
│       └── ...
└── driver/                      # 3I
    ├── ptrace_facade.rs
    └── bs_attach.rs

crates/bs-replay-driver/         # 3I — BugStalker integration
└── src/lib.rs
```

### Trace size

A program doing 1000 syscalls/second with average 100-byte returns
records at ~100 KB/s + protocol overhead → ~6 MB/min uncompressed,
~2–3 MB/min after `ruzstd` Fastest-level compression (slightly
worse ratio than C zstd at level 3, comparable to LZ4). A 30-minute
session produces ~60–90 MB. I/O-heavy programs (e.g. file servers)
can record at 10 MB/s+; cap the trace size with rotation policy.

### Architectural decisions, stated upfront

1. **Single-CPU serialisation for multi-threaded determinism.** No
   alternative has been demonstrated to work reliably without
   specialised hardware. Cost: ~5× recording overhead on CPU-bound
   workloads (PT brings this down to ~2×).
2. **Linux first, aarch64 second, Darwin best-effort.** Apple's
   kernel is closed and Tier 3 will always be partial there.
3. **`seccomp-notify` on Linux ≥ 5.5.** No support for older kernels;
   detect at startup and report cleanly.
4. **Trace format is custom.** No "extend rr's format" — different
   project, different design choices, different versioning.
5. **No production-recording mode.** Tier 3 is for interactive
   debugging only. The 5× overhead alone makes it unsuitable for
   production; we don't pretend otherwise.
6. **MIT/Apache licensed throughout.** Every file is original work.
   `dev-dependencies` may include rr (for differential tests) and
   GPL libraries (for cross-checking only); release builds do not
   link any GPL code.
7. **Trace compression: pure-Rust `ruzstd`** at
   `CompressionLevel::Fastest`. The on-disk format is RFC 8478
   stable so we can swap encoders later (faster pure-Rust impl,
   upstream ruzstd improvements, etc.) without touching the trace
   format. No C linkage in Phase 5.

### `io_uring` is in scope

`io_uring` recording is included in 3B from day one. Async I/O is too
common in modern Rust (tokio-rs, monoio, glommio) to ship a record
engine that fails on it.

Mechanism:

- Intercept `io_uring_setup` to log the ring's parameters and the
  shared-memory region's address
- Track the submission and completion queues by tagging memory pages
  via `userfaultfd` (Linux ≥ 4.3) or by single-stepping through the
  pages of interest
- On every submission queue entry (SQE) the program writes, log the
  `op`, `fd`, args, user_data
- On every completion queue entry (CQE) the kernel writes, log the
  result; on replay, the engine writes the same CQE values back into
  the shared CQ memory at the recorded times
- Treat `io_uring_enter` as a syscall boundary even when no syscall
  is actually issued (the kernel may write completions without an
  enter on `IORING_SETUP_SQPOLL`); detect SQPOLL setup and refuse to
  record those rings (rare in practice)

Effort budget: +3 weeks added to 3B (now ~9 weeks total). The shared-
memory tracking is the new mechanism; the rest is wiring.

### Other architectural questions for Tier 3

- **Shared-memory recording for IPC across processes.** Single-process
  recording is the MVP. Multi-process IPC (shared memory regions
  between recorded and unrecorded processes) is out of scope unless
  we also record the peer.
- **Container / namespace handling.** PID namespace differences
  between record and replay break trace replay. Document the
  expected namespace setup at record and replay sides.

### Inspirational prior art (read, do not copy)

- **gVisor** (Apache-2.0) — syscall-interposition architecture, OCI
  runtime. Cleanly licensed; useful for sanity-checking design
  approaches.
- **Firecracker** (Apache-2.0) — `seccomp-bpf` filter authoring
  patterns.
- **WASI runtime sandboxes** — capability-based syscall mediation
  patterns.
- **`criu`** (LGPL — read-only inspiration) — process snapshot and
  restore. Mechanism for binary-level memory checkpointing.
- **DoublePlay, Capo, dOS** (academic papers, public domain ideas)
  — deterministic replay theoretical underpinnings.

`rr` source itself is GPL; engineers working on Tier 3 must NOT
read its source. Behavioural specification (manpages, blog posts,
conference talks) is fair game.

## Cross-tier UX

Same commands work across all three tiers; BugStalker picks the
finest-grained back-end available:

| Command | Tier 1 only | + Tier 2 | + Tier 3 |
| --------- | ----------- | --------- | --------- |
| `rstep` | exact, no state | exact, state from checkpoint | exact, state |
| `rnext` | exact, no state | exact, state | exact, state |
| `rcontinue` | within PT window | within checkpoint range | unbounded |
| `replay-from <time>` | n/a | nearest checkpoint | exact |
| `replay-record <name>` | n/a | save the checkpoint set | save the trace |
| `replay-load <name>` | n/a | restore checkpoints | replay from trace |

The `replay-record` / `replay-load` commands are the bug-reproduction
flow: capture a session, ship it to a colleague, they load it on
their machine.

## Integration with other phases

- **Phase 3 (async).** Replay should reproduce async task scheduling.
  Tier 3 logs `io_uring`/`epoll` events, so async timing is
  deterministic. Tier 2 may schedule tasks differently on replay; flag
  this in UI.
- **Phase 6 (perf overlay).** Replay-mode runs can show the *original*
  per-line cycle distribution, not the replay's. The PT trace is part
  of the recording.
- **Phase 4 (visualisers).** No interaction needed; visualisers just
  see the value at the current (replay) PC.
- **Phase 1 (stdlib).** Watchpoint reverse step ("when did this `Vec`
  grow past 1024?") is the canonical demo.

## Test plan

- **Tier 1**: deterministic single-threaded test; PT decoder produces
  the expected reverse PC sequence.
- **Tier 2**: `fork` checkpoint at PC X, run forward, verify state at
  PC Y matches the original run's state at Y.
- **Tier 3 — record/replay self-consistency**: record a workload, then
  replay; assert PC trace at every breakpoint matches recorded PC
  trace exactly.
- **Tier 3 — differential vs rr** (CI-only, dev-dependency on rr):
  record same workload through our engine and rr; normalise both
  syscall logs; assert matching event sequences.
- **Tier 3 — chaos**: small program runs 1000 times with random
  scheduling; record once; replay 1000 times; every replay produces
  identical output and PC trace.
- **Tier 3 — long-tail syscalls**: test corpus exercises every recorded
  syscall (each Linux x86-64 syscall has at least one test).
- **Cross-tier**: combined Tier 1 + Tier 2 — reverse-step through a PT
  window, then `replay-from` a checkpoint to inspect state.
- **Cross-tier (Tier 3 + Phase 3 async)**: record a tokio program;
  replay; await-trace at each breakpoint matches recorded await-trace.

## Acceptance criteria

- Tier 1 ships in the same release as Phase 6's PT integration.
- Tier 2 works on Linux and Darwin (with the Mach fallback path).
- Tier 3 single-threaded MVP (3A + 3B + 3C + 3D + 3E + 3I) ships as
  the first major Tier 3 milestone, ~5 months in.
- Tier 3 multi-threaded MVP (+ 3F) ships ~7 months in.
- Tier 3 aarch64 (+ 3G) ships ~9 months in.
- Tier 3 PT-assisted recording (+ 3H) ships when convenient; not
  blocking.
- Tier 3 Darwin best-effort ships in same release as Tier 3 multi-
  threaded MVP.
- Differential test against rr passes for the test corpus on Linux
  x86-64.
- All reverse commands produce the same answer as a forward run that
  stops at the same PC, modulo the documented determinism caveats.
- `replay-record` / `replay-load` round-trip works for a 30-minute
  Tier 3 session, all platforms with Tier 3 support.

## Effort estimate

Tier 1 and Tier 2 first; Tier 3 is the long pole.

| Item | Effort |
| ------------------------------------------- | --------- |
| Tier 1 reverse step + commands | 2 weeks |
| Tier 2 fork-checkpoint Linux | 2 weeks |
| Tier 2 Mach checkpoint Darwin | 2 weeks |
| Tier 3A trace format | 3 weeks |
| Tier 3B Linux x86-64 syscall record (incl. `io_uring`) | 9 weeks |
| Tier 3C Linux x86-64 replay | 4 weeks |
| Tier 3D non-deterministic instructions | 3 weeks |
| Tier 3E signal record/replay | 3 weeks |
| Tier 3F multi-threaded single-CPU serialisation | 6 weeks |
| Tier 3G aarch64 port | 6 weeks |
| Tier 3H PT-assisted recording | 4 weeks |
| Tier 3I BugStalker driver integration | 3 weeks |
| Tier 3 Darwin DYLD shim | 6 weeks |
| Differential test infrastructure (rr oracle) | 2 weeks |
| UI work across all tiers | 2 weeks |
| Documentation + bug-repro flow | 2 weeks |

- **Tier 1 + Tier 2 minimum-viable**: ~6 weeks.
- **Tier 3 single-threaded Linux MVP** (incl. `io_uring`): +22 weeks
  (~6 months total).
- **Tier 3 multi-threaded Linux MVP**: +6 weeks (~7.5 months total).
- **Tier 3 cross-platform** (aarch64 + Darwin best-effort): +12 weeks
  (~10.5 months total).
- **Full Tier 3** (all sub-phases): ~13 months single-engineer.

This is a large commitment, planned upfront. Each sub-phase is
independently shippable; users get reverse-step from Tier 1 in week 2
and full record-and-replay incrementally over the year.

## Risks

- **PT decoder lag.** Decoding a 64 MB trace takes seconds. Reverse
  step at the very end of a long run feels slow. Mitigate with
  incremental decoding.
- **Mach checkpoint divergence.** Apple's COW behaviour is less
  predictable than Linux's. Validate carefully.
- **Trace file size.** Tier 3 traces can be huge for I/O-heavy
  programs. Compression at record time (pure-Rust `ruzstd` at
  Fastest level) is essential; rotation policy bounds the worst
  case.
- **Multi-threaded determinism.** Single-CPU serialisation is the
  only reliable approach; cost is ~5× slowdown on CPU-bound
  workloads. Document; PT integration brings it to ~2×.
- **vDSO stability.** vDSO entry-point patching is fragile across
  kernel versions. Maintain a per-kernel-version compatibility
  table; test on all currently-supported LTS kernels.
- **Syscall coverage.** ~340 Linux x86-64 syscalls; missing any
  one breaks recording for programs using it. The 3B effort
  budget includes time for the long tail; a "log unhandled syscall
  for triage" path catches gaps in production.
- **GPL contamination.** Engineers on Tier 3 do not read rr's source
  code. Code review enforces this. Differential testing uses rr as
  a binary oracle, not a source reference.
- **Apple deprecating mechanisms.** Darwin Tier 3 best-effort path
  depends on `DYLD_INSERT_LIBRARIES` which Apple is gradually
  restricting. Accept that Darwin Tier 3 may degrade with macOS
  updates.

## DAP integration

**Reverse step / next / continue** (Tier 1) map to standard DAP
`stepBack` (already in the protocol, capability flag
`supportsStepBack`). BugStalker advertises the capability when PT
trace is available.

**Time-travel session controls** (Tiers 2 and 3) via custom
requests under `bs/replay*`:

- `bs/replayCheckpointList` — enumerate checkpoints in the current
  session (Tier 2) with timestamps and PCs.
- `bs/replayJump` — jump to a checkpoint or a recorded timestamp.
- `bs/replayRecord` — start/stop trace recording (Tier 3).
- `bs/replayLoad` — load a saved trace from disk and attach.
- `bs/replayTimeline` — fetch a sparse timeline of breakpoint hits,
  syscall events, and signal deliveries for a UI scrubber.

VSCode rendering: a scrubber bar above the call-stack panel showing
the timeline, with checkpoints as dots and the current playhead. The
scrubber is the headline UX hook for non-TUI users — without it,
time-travel feels invisible.

## Specifications

- `seccomp(2)` — <https://man7.org/linux/man-pages/man2/seccomp.2.html>. Filter installation.
- `seccomp_unotify(2)` — <https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html>. The `SECCOMP_RET_USER_NOTIF` mechanism. Linux ≥ 5.5 required.
- `ptrace(2)` — <https://man7.org/linux/man-pages/man2/ptrace.2.html>. Tracee control.
- `userfaultfd(2)` — <https://man7.org/linux/man-pages/man2/userfaultfd.2.html>. Page-fault interception for `io_uring` SQ/CQ shared memory.
- `prctl(2)`, in particular `PR_SET_TSC` — <https://man7.org/linux/man-pages/man2/prctl.2.html>. RDTSC trapping.
- `vdso(7)` — <https://man7.org/linux/man-pages/man7/vdso.7.html>. The set of fast-path entries we patch.
- `process_vm_readv(2)` / `process_vm_writev(2)` — output-buffer extraction and replay-write.
- `io_uring` whitepaper — <https://kernel.dk/io_uring.pdf>. Architecture overview.
- `io_uring_setup(2)` — <https://man7.org/linux/man-pages/man2/io_uring_setup.2.html>. SQ/CQ memory layout.
- Linux x86-64 syscall ABI — <https://syscalls.mebeim.net/?table=x86/64/x64/v6.6>. Authoritative table for the ~340 syscall surface.
- aarch64 ELF psABI — <https://github.com/ARM-software/abi-aa/releases>. AArch64 calling convention and syscall numbering.
- aarch64 hardware capabilities — <https://docs.kernel.org/arch/arm64/elf_hwcaps.html>.
- Linux kernel signals — `signal(7)` — <https://man7.org/linux/man-pages/man7/signal.7.html>.
- Intel® 64 and IA-32 Architectures Software Developer's Manual, Volume 1 — <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>. RDTSC, RDRAND, CPUID semantics.
- zstd compressed data format — RFC 8478 — <https://datatracker.ietf.org/doc/html/rfc8478>. Trace segment compression.
- `ruzstd` (KillingSpark's `zstd-rs`) — <https://github.com/KillingSpark/zstd-rs>. Pure-Rust zstd encoder/decoder. We use `CompressionLevel::Fastest` for trace recording.
- Apple `dyld` reference — <https://github.com/apple-oss-distributions/dyld>. `DYLD_INSERT_LIBRARIES` semantics for Darwin best-effort recording.
- Apple `EndpointSecurity` framework — Apple Developer documentation. Not used (system extension required) but documented for completeness.
- gVisor architecture — <https://gvisor.dev/docs/>. Apache-2.0 reference for syscall-interposition design (no code copy).
- Firecracker seccomp filter authoring — <https://github.com/firecracker-microvm/firecracker/tree/main/src/vmm/src/resources>. Apache-2.0 reference patterns.
- POSIX.1-2017 signal semantics — <https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/signal.h.html>. Cross-check with Linux divergences.
- Academic: "Engineering Record and Replay for Deployability" — Mozilla rr paper — <https://arxiv.org/abs/1705.05937>. Read for *behavioural* spec only; do not consult rr source code.

Engineers working on Tier 3 must NOT read `rr` (GPL) source. Behavioural specs (manpages, papers, blog posts, this document) are sufficient.

## Invariants

Tier 3's defining invariant is *replay determinism*: the same trace replayed twice produces identical PC sequences.

```rust
// Trace file format.
debug_assert_eq!(trace_magic, *b"BSREPLAY");
debug_assert!(trace.format_version <= MAX_SUPPORTED_FORMAT_VERSION);

// Segment ordering.
debug_assert!(segment_index >= prev_segment_index);
debug_assert!(segment.events.len() > 0);

// Replay determinism: PC at every observable event matches recorded.
debug_assert_eq!(replay_pc, recorded_event.pc,
    "replay PC divergence at event {}: expected {:#x}, got {:#x}",
    event_idx, recorded_event.pc, replay_pc);

// Syscall arg bits match (post-mask of don't-care fields).
debug_assert_eq!(observed_args & arg_mask, recorded_args & arg_mask,
    "syscall arg divergence on syscall {}", syscall_nr);

// CPU feature subset: replay host must support the recording's features.
debug_assert!(replay_host.cpu_features.is_superset(&trace.cpu_features));

// Single-CPU mode pre-record.
debug_assert!(sched_getaffinity_count() == 1,
    "Tier 3 record requires sched_setaffinity to one CPU");

// io_uring SQ/CQ tracking — head never overtakes tail.
debug_assert!(sq_head <= sq_tail);
debug_assert!(cq_head <= cq_tail);

// Seccomp filter is loaded before fork.
debug_assert!(seccomp_filter_loaded());

// Checkpoint ring bounded.
debug_assert!(self.checkpoints.len() <= MAX_CHECKPOINTS);

// PT trace decoder advances forward.
debug_assert!(self.pt_decoder.position() >= prev_position);

// vDSO patching: every patched entry is in the recorded vDSO mapping.
debug_assert!(self.vdso_range.contains(&patched_addr));
```

Replay determinism is verified end-to-end by the soak test (record once, replay 100 times, compare PC traces); the `debug_assert!` here catches divergence at the earliest possible point (the first mismatched syscall arg or PC), giving a precise diagnosis rather than "the program ran differently".

## Non-goals

- Production debugging. Tier 3 recording in production is not
  supported. Use `samply` or a tracing crate.
- Time-travel for distributed systems. This is single-process only.
- Time-travel inside virtual machines or containers without ptrace.
  Out of scope.
- Vendoring or wrapping `rr` as a shipped feature. We use rr in CI
  as a test oracle only.
- Real-time replay (i.e. replay at original wall-clock speed).
  Replay runs as fast as the host can drive it; faster on simple
  workloads, slower on complex.

## Open questions

- **Glibc version dependence.** glibc internals change; some
  `read`/`write` flavours are emulated through `pread64` etc. Trace
  records the syscall, not the libc call, so this is mostly fine —
  but vDSO patching has glibc-version sensitivity.
- **Container/namespace handling.** Document expected setup; provide
  a `bs-replay-engine setup-doctor` command that detects and reports
  problems.

### Decisions on previously-open questions

- **`io_uring` recording**: in scope; baked into 3B with +3 weeks
  effort and a dedicated mechanism (`userfaultfd` ring tracking).
- **Trace portability across hosts**: not a concern. Traces are
  expected to be replayed on the recording host or one with the same
  CPU feature set. The manifest captures CPU features for the same-
  host case (replay on different feature set fails fast with a clear
  error). No "feature-mask" recording mode.

## Pure-Rust policy

Tier 3 record-and-replay introduces no C dependencies. Specifically:

- `seccomp-bpf`, `ptrace`, `userfaultfd`, `prctl`, `process_vm_readv`
  — all accessed via `rustix` (pure Rust, preferred) or
  `seccompiler` (Apache-2.0, pure Rust, from the Firecracker
  project) where rustix lacks coverage.
- vDSO patching: pure Rust binary patching via raw memory writes
  through `process_vm_writev`; no helper library needed.
- `io_uring` SQ/CQ tracking: `userfaultfd` via rustix; no C dep.
- Compression: `ruzstd` (pure Rust;
  <https://github.com/KillingSpark/zstd-rs>) at
  `CompressionLevel::Fastest`. The encoder is, per its maintainer,
  "usable, but does not yet reach the speed, ratio or
  configurability of the original zstd library" — fine for replay
  traces, where recording speed matters more than density. Output
  is RFC 8478-compliant zstd, so a future faster encoder (whether
  upstream ruzstd improvements or our own) is a drop-in change with
  no on-disk format impact. Phase 5 has **zero C dependencies**.
- Disassembly (where required, e.g. for vDSO entry-point detection):
  `iced-x86` (pure Rust) on x86; `bad64` (pure Rust) or `disarm64`
  on aarch64.
- Differential testing against `rr`: `rr` is invoked as an external
  binary in CI only; never linked.

Default `cargo build --features replay` produces a pure-Rust binary.

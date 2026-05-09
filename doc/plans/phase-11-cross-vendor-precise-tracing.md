# Phase 11 — Cross-vendor precise tracing

Phase 6 makes Intel PT the first precise-trace provider in
BugStalker. Phase 11 generalises that work into a provider model so
Linux/Intel PT, AMD Processor Trace / LBR-stack hardware, Apple
Processor Trace, and ARM CoreSight ETM can feed the same overlay,
reverse-navigation, and heap-timeline machinery.

The goal is not to pretend all vendors expose the same trace. The goal
is a stable BugStalker contract:

- Capture runs outside the debuggee.
- Decode produces source-attributable control-flow or instruction
  events.
- Consumers can see exactly what precision a provider has before they
  promise reverse-step semantics.

## Why this is its own phase

Phase 6 deliberately uses Intel PT names in the public diagnostics and
feature gates because the first real backend is Intel-specific:
`intel_pt` PMU discovery, AUX buffers, and `libipt` decoding. Extending
that code in place for every vendor would spread architecture checks
through the DAP layer, replay layer, and Phase 10 heap overlay.

Phase 11 creates one precise-trace abstraction and then ports Intel PT
onto it. AMD and Apple support then become providers, not special cases
inside `bs/perfOverlay`.

## AMD scope

AMD's current public APM documents the Extended Performance Monitoring
and Debug CPUID leaf `Fn8000_0022`. In that leaf:

- `EAX[LbrStack]` advertises Last Branch Record Stack support.
- `EBX[LbrStackSize]` reports the number of branch-stack entries.
- `EAX[LbrAndPmcFreeze]` advertises freezing the LBR stack with a PMC
  overflow.

That gives BugStalker a branch-precise AMD backend. It is not the same
thing as Intel PT's compressed packet stream, and Phase 11 should not
market it as instruction-exact reverse stepping until target hardware
and Linux perf exposure prove a richer AMD Processor Trace stream.

Naming rule:

- `amdProcessorTrace` is the user-facing umbrella for AMD precise trace.
- `amdLbrStack` is the first concrete backend when the OS exposes only
  branch records.
- If a packet-stream AMD PT PMU is present, add it as a second backend
  under the same umbrella rather than replacing the LBR path.

## Provider model

Add a `bs-trace` or `bs-perf::precise` module with these concepts:

```rust
pub enum TraceProviderKind {
    IntelPt,
    AmdProcessorTrace,
    AmdLbrStack,
    AppleProcessorTrace,
    ArmCoreSightEtm,
}

pub enum TracePrecision {
    InstructionExact,
    BranchExact,
    Sampled,
}

pub trait PreciseTraceProvider {
    fn probe(&self) -> TraceProbe;
    fn open(&self, pid: Pid, options: TraceOptions) -> Result<Box<dyn TraceCapture>>;
}
```

Decoded output is provider-neutral:

```rust
pub enum TraceEvent {
    Instruction { ip: u64, size: u8, speculative: bool },
    Branch { from: u64, to: u64, speculative: bool },
    Sync { ip: Option<u64> },
    Gap { reason: TraceGapReason },
}
```

Intel PT maps to `Instruction` when `libipt` can decode the AUX bytes.
AMD LBR maps to `Branch`; it can still drive source attribution,
branch-level reverse navigation, and checkpoint rendezvous, but it does
not have every in-between PC.

## DAP and console surface

Keep the Phase 6 compatibility fields:

- `arguments.intelPt: true`
- `arguments.precise: true`
- `intelPt` diagnostic body

Add vendor-neutral diagnostics:

```json
{
  "preciseTrace": {
    "requested": true,
    "selectedProvider": "amdLbrStack",
    "precision": "branchExact",
    "providers": [
      { "kind": "intelPt", "status": "unavailable" },
      { "kind": "amdLbrStack", "status": "available" }
    ]
  }
}
```

Enable shape:

```json
{
  "preciseTrace": {
    "enabled": true,
    "provider": "auto"
  }
}
```

Console command:

```text
(bs) trace providers
provider        status       precision
intel-pt        unavailable  instruction-exact
amd-lbr-stack   available    branch-exact
```

## Phase 10 contract

Phase 10's Bytehound timeline splice must depend on decoded trace
events, not on Intel PT specifically. Heap queries need these common
fields only:

- source location,
- playhead ordering,
- provider precision,
- optional gap metadata.

On an AMD LBR-only backend, heap-aware reverse navigation can move
between branch boundaries and checkpoint anchors. It must not claim to
walk every instruction in a basic block. On Intel PT or a verified AMD
packet backend, the same Phase 10 API can advertise instruction-exact
heap stepping.

## Implementation slices

| Sub-phase | Description | Weeks |
| --------- | ----------- | ----- |
| 11A | Provider-neutral probe/result/DAP types | 1 |
| 11B | Move Intel PT Phase 6 code behind provider trait | 2 |
| 11C | AMD CPUID probe for `Fn8000_0022` and Linux perf branch-stack discovery | 1 |
| 11D | AMD LBR capture/decode into `TraceEvent::Branch` | 2 |
| 11E | Source attribution and aggregation for branch events | 1 |
| 11F | Phase 5 reverse-navigation precision gates | 1 |
| 11G | Phase 10 heap-timeline precision gates | 1 |
| 11H | Hardware-matrix tests and docs | 1 |
| | **Total** | **10** |

Apple Processor Trace and ARM CoreSight ETM are follow-on providers
using the same API. They should not block the AMD work.

## Acceptance criteria

- `bs/perfOverlayEnable` with `preciseTrace.provider = "auto"` selects
  the best available provider and reports the chosen precision.
- Intel PT still works through the new provider layer with the same
  Phase 6 diagnostics.
- On AMD hardware with LBR Stack support, BugStalker reports
  `amdLbrStack` as available and captures a branch window through the
  OS-supported mechanism.
- Source attribution works for AMD branch records.
- Phase 5 reverse controls and Phase 10 heap reverse controls degrade
  honestly when the selected provider is branch-exact instead of
  instruction-exact.
- Unsupported or permission-blocked providers fall back to cycles
  sampling without disabling the debugger.

## References

- AMD64 Architecture Programmer's Manual, combined volumes,
  Rev. 4.09 / March 2026 — CPUID `Fn8000_0022`, DebugCtl, and Last
  Branch Record Stack.
  <https://docs.amd.com/v/u/en-US/40332_4.09_APM_PUB>
- Intel 64 and IA-32 Architectures Software Developer's Manual,
  Volume 3, Chapter 36 — Intel Processor Trace.
  <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>
- Linux `perf_event_open(2)` and branch-stack sampling.
  <https://man7.org/linux/man-pages/man2/perf_event_open.2.html>

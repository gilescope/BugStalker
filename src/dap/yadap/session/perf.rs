// SPDX-License-Identifier: MIT
//! Phase 6 perf-overlay DAP JSON boundary.
//!
//! `bs-perf` owns the typed request/response semantics. This module
//! adapts those shapes to the active DAP server's `serde_json::Value`
//! transport and keeps default builds graceful when the optional root
//! `perf` feature is disabled.

use crate::dap::yadap::protocol::DapRequest;
use anyhow::anyhow;
#[cfg(all(feature = "perf", target_os = "linux"))]
use nix::unistd::Pid;
use serde_json::{Value, json};
#[cfg(all(feature = "perf", target_os = "linux"))]
use std::path::{Path, PathBuf};
#[cfg(all(feature = "perf", target_os = "linux"))]
use std::time::Instant;

#[cfg(not(feature = "perf"))]
const PERF_UNAVAILABLE: &str = "BugStalker was built without the `perf` feature";

#[cfg(feature = "perf")]
#[derive(Debug, Default)]
pub(super) struct PerfOverlaySession {
    enabled: bool,
    intel_pt_requested: bool,
    data: bs_perf::aggregator::PerfData,
    unavailable: Option<String>,
    #[cfg(target_os = "linux")]
    unsampled_thread_count: usize,
    #[cfg(target_os = "linux")]
    pt_decode_config: Option<bs_perf::pt_decode::IntelPtDecodeConfig>,
    #[cfg(target_os = "linux")]
    pt_decode_unavailable: Option<String>,
    #[cfg(target_os = "linux")]
    pt_last_aux_bytes: usize,
    #[cfg(target_os = "linux")]
    pt_last_decoded_instructions: u64,
    #[cfg(target_os = "linux")]
    pt_last_resolved_instructions: u64,
    #[cfg(target_os = "linux")]
    pt_last_unresolved_instructions: u64,
    #[cfg(target_os = "linux")]
    active: Vec<ActivePerfRun>,
    /// Darwin per-run state: rusage start snapshot (Tier 2) and
    /// the polling sampler (Tier 1b). Both populated at
    /// `begin_perf_run`; both consumed at `finish_perf_stop`.
    /// `None` when no run is active or setup failed (cause is
    /// always also recorded in `unavailable`).
    #[cfg(target_os = "macos")]
    darwin_run: Option<DarwinPerfRun>,
    /// Diagnostic counters captured from the last completed run —
    /// page-ins, disk I/O bytes — used to flag the per-stop
    /// diagnosis line.
    #[cfg(target_os = "macos")]
    darwin_last_delta: DarwinLastDelta,
    /// Whether the last completed run had a live poll sampler.
    /// Captured in `finish_perf_stop` *before* the sampler is drained:
    /// that method `take()`s `darwin_run` and consumes the sampler, so
    /// by the time the stopped summary calls `perf_mode_label` the live
    /// sampler is already gone. Reading `darwin_run` there always saw
    /// `None`, so the mode misreported `macos-rusage-only` even when the
    /// sampler ran (it just resolved no samples — e.g. an I/O-bound
    /// workload). This snapshot is the source of truth for the label.
    #[cfg(target_os = "macos")]
    last_run_sampler_active: bool,
    /// Dominant blocking syscall (`x16`) sampled during the last run —
    /// names *why* a mostly-waiting step was waiting (lock / sleep /
    /// I/O / mach-IPC). `None` when nothing useful was sampled.
    #[cfg(target_os = "macos")]
    last_wait_syscall: Option<i64>,
    /// Thread-isolation guard for the current run (EXPERIMENTAL, opt-in via
    /// `BS_PERF_ISOLATE_THREAD`): suspends every non-focus thread so the
    /// process-wide rusage delta reflects only the stepped thread, with a
    /// watchdog that thaws on a stall (a step that blocks on a frozen thread
    /// would otherwise hang). Thawed in `finish_perf_stop` / on drop.
    #[cfg(target_os = "macos")]
    freeze_guard: Option<ThreadFreezeGuard>,
}

/// macOS in-flight run state. Owns the rusage start snapshot, the
/// optional poll sampler, and the source resolver used to map
/// sampled PCs to (file, line).
#[cfg(all(feature = "perf", target_os = "macos"))]
struct DarwinPerfRun {
    rusage_start: bs_perf::darwin::ProcessSnapshot,
    sampler: Option<bs_perf::darwin::PollSampler>,
    resolver: Option<bs_perf::decoder::SourceResolver>,
}

/// Cached macOS diagnostic counters from the most recent stop —
/// page-ins, disk I/O bytes, etc. Drives the emoji-coded
/// `diagnosis` field in `body.bs_perf`.
#[cfg(all(feature = "perf", target_os = "macos"))]
#[derive(Debug, Default, Clone, Copy)]
struct DarwinLastDelta {
    pageins: u64,
    disk_bytes_read: u64,
    disk_bytes_written: u64,
    /// Memory-footprint change over the last run window, in bytes
    /// (signed). Surfaced as `physFootprintDelta` — a cheap proxy for
    /// "did this step allocate" (it's footprint growth, not a malloc
    /// count: the allocator's free-list reuse won't move it).
    phys_footprint_delta: i64,
}

#[cfg(all(feature = "perf", target_os = "macos"))]
impl std::fmt::Debug for DarwinPerfRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DarwinPerfRun")
            .field("rusage_start", &self.rusage_start)
            .field("sampler", &self.sampler.is_some())
            .field("resolver", &self.resolver.is_some())
            .finish()
    }
}

#[cfg(all(feature = "perf", target_os = "linux"))]
#[derive(Debug)]
struct ActivePerfRun {
    tid: Pid,
    monitor: bs_perf::PerfMonitor,
    ring: bs_perf::linux::ring::PerfRingBuffer,
    pt_capture: Option<bs_perf::linux::IntelPtCapture>,
    resolver: Option<bs_perf::decoder::SourceResolver>,
    started_at: Instant,
    /// Optional pure-counter `PERF_COUNT_HW_INSTRUCTIONS` event,
    /// opened alongside the cycles+IP sampler. Read at stop to
    /// produce `runInstructions` in body.bs_perf. None when the
    /// kernel refused the open (typically the same perf_event_paranoid
    /// path that already let cycles through, so this is rare).
    instructions: Option<bs_perf::PerfMonitor>,
}

impl super::DebugSession {
    #[cfg(all(feature = "perf", target_os = "linux"))]
    pub(super) fn begin_perf_run(&mut self) {
        if !self.perf_overlay.enabled {
            return;
        }
        self.perf_overlay.data.begin_run();
        self.perf_overlay.active.clear();
        self.perf_overlay.unavailable = None;
        self.perf_overlay.unsampled_thread_count = 0;
        self.perf_overlay.pt_decode_config = None;
        self.perf_overlay.pt_decode_unavailable = None;
        self.perf_overlay.pt_last_aux_bytes = 0;
        self.perf_overlay.pt_last_decoded_instructions = 0;
        self.perf_overlay.pt_last_resolved_instructions = 0;
        self.perf_overlay.pt_last_unresolved_instructions = 0;

        let Some((proc_pid, focus_tid, program, thread_tids)) = ({
            self.debugger.as_ref().map(|dbg| {
                let proc_pid = dbg.process().pid();
                let focus_tid = dbg.ecx().pid_on_focus();
                let program = PathBuf::from(dbg.process().program());
                let thread_tids = dbg.thread_tids();
                (proc_pid, focus_tid, program, thread_tids)
            })
        }) else {
            self.perf_overlay.unavailable = Some("debugger not initialized".to_owned());
            return;
        };

        let mut tids = match thread_tids {
            Ok(mut tids) => {
                if tids.is_empty() {
                    tids.push(focus_tid);
                }
                tids
            }
            Err(err) => {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!("thread enumeration failed: {err}; sampling focused tid only"),
                );
                vec![focus_tid]
            }
        };
        tids.sort_by_key(|tid| tid.as_raw());
        tids.dedup();

        let resolver_template = match source_resolver_for_process(proc_pid, &program) {
            Ok(resolver) => Some(resolver),
            Err(err) => {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!(
                        "source resolver unavailable for {}: {err}",
                        program.display()
                    ),
                );
                None
            }
        };

        let intel_pt_probe = bs_perf::linux::probe_intel_pt();
        let intel_pt_pmu_type = if self.perf_overlay.intel_pt_requested {
            match (&intel_pt_probe.status, intel_pt_probe.pmu_type) {
                (bs_perf::linux::IntelPtStatus::Available, Some(pmu_type)) => Some(pmu_type),
                _ => {
                    append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        "Intel PT requested but unavailable on this host".to_owned(),
                    );
                    None
                }
            }
        } else {
            None
        };

        if intel_pt_probe.usable_without_elevated_caps() {
            match intel_pt_decode_config_for_process(proc_pid) {
                Ok(config) if !config.image_sections.is_empty() => {
                    self.perf_overlay.pt_decode_config = Some(config);
                }
                Ok(_) => {
                    self.perf_overlay.pt_decode_unavailable =
                        Some("no executable file mappings".to_owned());
                }
                Err(err) => {
                    self.perf_overlay.pt_decode_unavailable =
                        Some(format!("image map unavailable: {err}"));
                }
            }
        }

        for tid in tids {
            match open_active_perf_run(tid, resolver_template.clone()) {
                Ok(mut active) => {
                    if let Some(pmu_type) = intel_pt_pmu_type {
                        match open_active_intel_pt_capture(tid, pmu_type) {
                            Ok(capture) => active.pt_capture = Some(capture),
                            Err(err) => append_unavailable(
                                &mut self.perf_overlay.unavailable,
                                format!("Intel PT capture unavailable for tid {tid}: {err}"),
                            ),
                        }
                    }
                    self.perf_overlay.active.push(active);
                }
                Err(err) => append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!("cycles sampling unavailable for tid {tid}: {err}"),
                ),
            }
        }

        if self.perf_overlay.active.is_empty() {
            append_unavailable(
                &mut self.perf_overlay.unavailable,
                "cycles sampling inactive: no thread monitors opened".to_owned(),
            );
        }
    }

    /// Darwin perf-run start. Pieces together Tier 2 (rusage
    /// snapshot for CPU time) + Tier 1b (poll sampler for the
    /// gutter heat-map). Both are best-effort: failures fall back
    /// to whichever sub-tier still works, and the reason is
    /// recorded in `unavailable`.
    #[cfg(all(feature = "perf", target_os = "macos"))]
    pub(super) fn begin_perf_run(&mut self) {
        use crate::debugger::darwin_mach;
        use std::path::PathBuf;

        if !self.perf_overlay.enabled {
            return;
        }
        self.perf_overlay.data.begin_run();
        self.perf_overlay.unavailable = None;
        self.perf_overlay.darwin_run = None;
        self.perf_overlay.last_wait_syscall = None;

        let Some((proc_pid, program)) = self
            .debugger
            .as_ref()
            .map(|dbg| (dbg.process().pid(), PathBuf::from(dbg.process().program())))
        else {
            self.perf_overlay.unavailable = Some("debugger not initialized".to_owned());
            return;
        };

        // Tier 2: rusage snapshot. The status-bar CPU-time figure
        // depends on this even when the sampler fails.
        let rusage_start = match bs_perf::darwin::ProcessSnapshot::capture(proc_pid.as_raw()) {
            Ok(snap) => snap,
            Err(err) => {
                self.perf_overlay.unavailable =
                    Some(format!("rusage snapshot at run start unavailable: {err}"));
                return;
            }
        };

        // Tier 1b: poll sampler. Needs the debuggee's mach task
        // and the dyld load slide for source resolution.
        let task = match darwin_mach::task_for_pid(proc_pid) {
            Ok(task) => task,
            Err(err) => {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!("task_for_pid failed for sampler: {err}"),
                );
                self.perf_overlay.darwin_run = Some(DarwinPerfRun {
                    rusage_start,
                    sampler: None,
                    resolver: None,
                });
                return;
            }
        };

        let resolver = match bs_perf::decoder::SourceResolver::from_object_path(&program) {
            Ok(mut resolver) => {
                if let Some(load_addr) = darwin_main_image_load_addr(task) {
                    resolver = resolver.with_load_bias(load_addr);
                } else {
                    append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        "dyld load slide unavailable; sampled PCs will be unresolved".to_owned(),
                    );
                }
                Some(resolver)
            }
            Err(err) => {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!(
                        "source resolver unavailable for {}: {err}",
                        program.display()
                    ),
                );
                None
            }
        };

        let mut sampler =
            bs_perf::darwin::PollSampler::new(task, bs_perf::darwin::DEFAULT_POLL_PERIOD);
        let sampler = match sampler.start() {
            Ok(()) => Some(sampler),
            Err(err) => {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!("poll sampler start failed: {err}"),
                );
                None
            }
        };

        self.perf_overlay.darwin_run = Some(DarwinPerfRun {
            rusage_start,
            sampler,
            resolver,
        });

        // EXPERIMENTAL thread isolation: freeze every non-focus thread for
        // the step window so the process-wide rusage delta reflects only the
        // stepped thread (otherwise a runtime worker's millions of
        // instructions get billed to the stepped line). Off unless
        // BS_PERF_ISOLATE_THREAD is set — it changes execution semantics and
        // a step that blocks on a frozen thread relies on the watchdog.
        if std::env::var_os("BS_PERF_ISOLATE_THREAD").is_some()
            && let Some(focus_pid) = self.debugger.as_ref().map(|dbg| dbg.ecx().pid_on_focus())
        {
            self.perf_overlay.freeze_guard = install_thread_freeze(task, focus_pid);
        }
    }

    #[cfg(all(feature = "perf", not(any(target_os = "linux", target_os = "macos"))))]
    pub(super) fn begin_perf_run(&mut self) {
        if self.perf_overlay.enabled {
            self.perf_overlay.unavailable =
                Some("perf overlay live collection unavailable on this platform".to_owned());
        }
    }

    #[cfg(not(feature = "perf"))]
    pub(super) fn begin_perf_run(&mut self) {}

    #[cfg(all(feature = "perf", target_os = "linux"))]
    pub(super) fn finish_perf_stop(&mut self) {
        if self.perf_overlay.active.is_empty() {
            return;
        }

        let active_runs = std::mem::take(&mut self.perf_overlay.active);
        let sampled_tids = active_runs
            .iter()
            .map(|active| active.tid)
            .collect::<Vec<_>>();

        let mut wall_ns = 0_u64;
        let mut total_instructions: u64 = 0;
        let mut instructions_seen = false;
        for mut active in active_runs {
            if let Err(err) = active.monitor.disable() {
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!(
                        "cycles sampling disable failed for tid {}: {err}",
                        active.tid
                    ),
                );
            }
            if let Some(mut counter) = active.instructions.take() {
                let _ = counter.disable();
                match counter.read_count() {
                    Ok(n) => {
                        total_instructions = total_instructions.saturating_add(n);
                        instructions_seen = true;
                    }
                    Err(err) => append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        format!(
                            "instructions counter read failed for tid {}: {err}",
                            active.tid
                        ),
                    ),
                }
            }
            match active.ring.drain() {
                Ok((records, stats)) => {
                    for record in records {
                        match record {
                            bs_perf::linux::ring::PerfRecord::Sample(sample) => {
                                if let Some(resolver) = active.resolver.as_mut() {
                                    if let Some(resolved) = resolver.resolve(sample.ip) {
                                        self.perf_overlay.data.record_resolved_pc(&resolved);
                                    } else {
                                        self.perf_overlay.data.record_unresolved_pc();
                                    }
                                } else {
                                    self.perf_overlay.data.record_unresolved_pc();
                                }
                            }
                            bs_perf::linux::ring::PerfRecord::Lost { lost, .. }
                            | bs_perf::linux::ring::PerfRecord::LostSamples { lost } => {
                                self.perf_overlay.data.record_unresolved_samples(lost);
                            }
                            bs_perf::linux::ring::PerfRecord::Aux { .. }
                            | bs_perf::linux::ring::PerfRecord::Unknown { .. } => {}
                        }
                    }
                    if stats.lost != 0 {
                        append_unavailable(
                            &mut self.perf_overlay.unavailable,
                            format!(
                                "perf ring for tid {} reported {} lost sample(s)",
                                active.tid, stats.lost
                            ),
                        );
                    }
                }
                Err(err) => {
                    append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        format!("perf ring drain failed for tid {}: {err}", active.tid),
                    );
                }
            }

            if let Some(capture) = active.pt_capture.as_mut() {
                match capture.stop_and_drain() {
                    Ok(drain) => {
                        self.perf_overlay.pt_last_aux_bytes = self
                            .perf_overlay
                            .pt_last_aux_bytes
                            .saturating_add(drain.aux_stats.bytes);
                        record_intel_pt_drain(
                            active.tid,
                            drain,
                            self.perf_overlay.pt_decode_config.as_ref(),
                            active.resolver.as_mut(),
                            &mut self.perf_overlay.data,
                            &mut self.perf_overlay.unavailable,
                            &mut self.perf_overlay.pt_last_decoded_instructions,
                            &mut self.perf_overlay.pt_last_resolved_instructions,
                            &mut self.perf_overlay.pt_last_unresolved_instructions,
                        );
                    }
                    Err(err) => append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        format!("Intel PT drain failed for tid {}: {err}", active.tid),
                    ),
                }
            }

            wall_ns = wall_ns.max(
                active
                    .started_at
                    .elapsed()
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64,
            );
        }

        match self.debugger.as_ref().map(|dbg| dbg.thread_tids()) {
            Some(Ok(current_tids)) => {
                self.perf_overlay.unsampled_thread_count = current_tids
                    .into_iter()
                    .filter(|tid| !sampled_tids.contains(tid))
                    .count();
                if self.perf_overlay.unsampled_thread_count != 0 {
                    append_unavailable(
                        &mut self.perf_overlay.unavailable,
                        format!(
                            "{} currently attached thread(s) had no perf ring in this run",
                            self.perf_overlay.unsampled_thread_count
                        ),
                    );
                }
            }
            Some(Err(err)) => append_unavailable(
                &mut self.perf_overlay.unavailable,
                format!("thread coverage check failed after stop: {err}"),
            ),
            None => {}
        }
        let instructions_arg = if instructions_seen {
            Some(total_instructions)
        } else {
            None
        };
        self.perf_overlay
            .data
            .finish_stop_full(0, wall_ns, None, instructions_arg);
    }

    /// Darwin perf-run stop. Drains the poll sampler (Tier 1b),
    /// attributes each PC to source, then closes the run-to-stop
    /// window with a CPU-time figure from the rusage delta
    /// (Tier 2). Samples must be pushed before `finish_stop_*`
    /// because that call snapshots `last_run` into history.
    #[cfg(all(feature = "perf", target_os = "macos"))]
    pub(super) fn finish_perf_stop(&mut self) {
        // Thaw the isolation guard first (the step is done) — its Drop
        // resumes the frozen threads. Runs on every path, incl. early return.
        self.perf_overlay.freeze_guard = None;
        let Some(run) = self.perf_overlay.darwin_run.take() else {
            self.perf_overlay.last_run_sampler_active = false;
            return;
        };
        // Snapshot sampler liveness before `run` (and its sampler) is
        // consumed below — perf_mode_label reads this after the fact.
        self.perf_overlay.last_run_sampler_active = run.sampler.is_some();

        // 1) Drain poll-sampler samples and attribute them. Even
        // if the resolver is missing (no load slide / no .debug_line),
        // we count the samples as unresolved so the user sees that
        // sampling happened.
        if let Some(sampler) = run.sampler {
            let drain = sampler.stop_and_drain();
            let mut resolver = run.resolver;
            for pc in drain.samples {
                if let Some(resolver) = resolver.as_mut() {
                    if let Some(resolved) = resolver.resolve(pc) {
                        self.perf_overlay.data.record_resolved_pc(&resolved);
                    } else {
                        self.perf_overlay.data.record_unresolved_pc();
                    }
                } else {
                    self.perf_overlay.data.record_unresolved_pc();
                }
            }
            if drain.failed_snapshots != 0 {
                self.perf_overlay
                    .data
                    .record_unresolved_samples(drain.failed_snapshots);
            }
            // Name the wait: the dominant blocking syscall sampled in
            // the window (idle runtime threads sit in mach traps, so we
            // prefer a positive BSD blocking syscall — see fn).
            self.perf_overlay.last_wait_syscall = dominant_wait_syscall(&drain.syscalls);
        }

        // 2) Take the rusage end snapshot and close the window.
        let Some(proc_pid) = self.debugger.as_ref().map(|dbg| dbg.process().pid()) else {
            // Debugger gone — finalise with what we have.
            let wall_ns = run
                .rusage_start
                .wall
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64;
            self.perf_overlay
                .data
                .finish_stop_full(0, wall_ns, None, None);
            self.perf_overlay.darwin_last_delta = DarwinLastDelta::default();
            return;
        };
        match bs_perf::darwin::ProcessSnapshot::capture(proc_pid.as_raw()) {
            Ok(end) => {
                let delta = end.delta_since(run.rusage_start);
                let instructions = if delta.instructions != 0 {
                    Some(delta.instructions)
                } else {
                    None
                };
                self.perf_overlay.data.finish_stop_full(
                    delta.cycles,
                    delta.wall_ns,
                    Some(delta.cpu_time_ns),
                    instructions,
                );
                self.perf_overlay.darwin_last_delta = DarwinLastDelta {
                    pageins: delta.pageins,
                    disk_bytes_read: delta.disk_bytes_read,
                    disk_bytes_written: delta.disk_bytes_written,
                    phys_footprint_delta: delta.phys_footprint_delta,
                };
            }
            Err(err) => {
                let wall_ns = run
                    .rusage_start
                    .wall
                    .elapsed()
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                self.perf_overlay
                    .data
                    .finish_stop_full(0, wall_ns, None, None);
                self.perf_overlay.darwin_last_delta = DarwinLastDelta::default();
                append_unavailable(
                    &mut self.perf_overlay.unavailable,
                    format!("rusage snapshot at run end unavailable: {err}"),
                );
            }
        }
    }

    #[cfg(all(feature = "perf", not(any(target_os = "linux", target_os = "macos"))))]
    pub(super) fn finish_perf_stop(&mut self) {}

    #[cfg(not(feature = "perf"))]
    pub(super) fn finish_perf_stop(&mut self) {}

    #[cfg(feature = "perf")]
    pub(super) fn handle_perf_overlay(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let perf_req = parse_perf_overlay_request(req)?;
        let response = bs_perf::dap::perf_overlay(&self.perf_overlay.data, &perf_req);
        let lines = response
            .lines
            .into_iter()
            .map(|line| {
                json!({
                    "line": line.line,
                    "sampleCount": line.sample_count,
                    "sampleShare": line.sample_share,
                    "heat": line.heat,
                    "hottest": line.hottest,
                })
            })
            .collect::<Vec<_>>();

        self.send_success_body(
            req,
            json!({
                "enabled": self.perf_overlay.enabled,
                "source": response.source,
                "lines": lines,
                "totalResolvedSamples": response.total_resolved_samples,
                "unresolvedSamples": response.unresolved_samples,
                "unavailable": self.perf_overlay.unavailable.clone(),
                "activeThreadCount": active_thread_count(&self.perf_overlay),
                "unsampledThreadCount": unsampled_thread_count(&self.perf_overlay),
                "intelPt": intel_pt_probe_body(&self.perf_overlay),
                "kperf": kperf_probe_body(),
            }),
        )
    }

    #[cfg(not(feature = "perf"))]
    pub(super) fn handle_perf_overlay(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let source = parse_perf_overlay_source(req).unwrap_or_else(|_| String::new());
        self.send_success_body(
            req,
            json!({
                "enabled": false,
                "unavailable": PERF_UNAVAILABLE,
                "source": source,
                "lines": [],
                "totalResolvedSamples": 0,
                "unresolvedSamples": 0,
            }),
        )
    }

    #[cfg(feature = "perf")]
    pub(super) fn handle_perf_overlay_enable(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let enable_req = parse_perf_overlay_enable_request(req);
        self.perf_overlay.enabled = true;
        self.perf_overlay.intel_pt_requested = enable_req.intel_pt;
        self.perf_overlay.unavailable = None;
        #[cfg(target_os = "macos")]
        if enable_req.intel_pt {
            // Tier 2 macOS collects whole-process CPU time only; PMU
            // sampling isn't bound until the kperf tier. Honour the
            // base enable, surface the PT degradation, and continue.
            self.perf_overlay.unavailable = Some(
                "Intel PT requested on macOS; rusage Tier 2 is the only macOS path today \
                 (kperf tier pending)"
                    .to_owned(),
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.perf_overlay.enabled = false;
            self.perf_overlay.unavailable =
                Some("perf overlay live collection unavailable on this platform".to_owned());
        }
        let response = bs_perf::dap::enable(&enable_req);
        self.send_success_body(
            req,
            json!({
                "enabled": self.perf_overlay.enabled && response.enabled,
                "unavailable": self.perf_overlay.unavailable.clone(),
                "intelPt": intel_pt_probe_body(&self.perf_overlay),
                "kperf": kperf_probe_body(),
            }),
        )
    }

    #[cfg(not(feature = "perf"))]
    pub(super) fn handle_perf_overlay_enable(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.send_success_body(
            req,
            json!({
                "enabled": false,
                "unavailable": PERF_UNAVAILABLE,
            }),
        )
    }

    #[cfg(feature = "perf")]
    pub(super) fn handle_perf_overlay_disable(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.perf_overlay.enabled = false;
        self.perf_overlay.intel_pt_requested = false;
        self.perf_overlay.unavailable = None;
        #[cfg(target_os = "linux")]
        for mut active in self.perf_overlay.active.drain(..) {
            if let Some(mut capture) = active.pt_capture.take() {
                let _ = capture.stop_and_drain();
            }
            if let Some(mut counter) = active.instructions.take() {
                let _ = counter.disable();
            }
            let _ = active.monitor.disable();
        }
        #[cfg(target_os = "linux")]
        {
            self.perf_overlay.pt_decode_config = None;
            self.perf_overlay.pt_decode_unavailable = None;
            self.perf_overlay.pt_last_aux_bytes = 0;
            self.perf_overlay.pt_last_decoded_instructions = 0;
            self.perf_overlay.pt_last_resolved_instructions = 0;
            self.perf_overlay.pt_last_unresolved_instructions = 0;
        }
        #[cfg(target_os = "macos")]
        if let Some(run) = self.perf_overlay.darwin_run.take()
            && let Some(sampler) = run.sampler
        {
            let _ = sampler.stop_and_drain();
        }
        let response = bs_perf::dap::disable(&bs_perf::dap::PerfOverlayDisableRequest {});
        self.send_success_body(
            req,
            json!({
                "enabled": response.enabled,
            }),
        )
    }

    #[cfg(not(feature = "perf"))]
    pub(super) fn handle_perf_overlay_disable(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        self.send_success_body(
            req,
            json!({
                "enabled": false,
            }),
        )
    }

    #[cfg(feature = "perf")]
    pub(super) fn perf_stopped_summary_body(&self) -> Option<Value> {
        if !self.perf_overlay.enabled {
            return None;
        }
        let summary = bs_perf::dap::stopped_summary(&self.perf_overlay.data)?;
        let ipc = ipc_for(&summary);
        let diagnosis = diagnose(&summary, &self.perf_overlay);
        // Memory-footprint delta for the window (bytes, signed). macOS
        // only — Linux has no equivalent in the rusage path yet, so null.
        #[cfg(target_os = "macos")]
        let phys_footprint_delta: Option<i64> = Some(self.perf_overlay.darwin_last_delta.phys_footprint_delta);
        #[cfg(not(target_os = "macos"))]
        let phys_footprint_delta: Option<i64> = None;
        Some(json!({
            "mode": perf_mode_label(&self.perf_overlay),
            "runCycles": summary.run_cycles,
            "runWallNs": summary.run_wall_ns,
            "runCpuTimeNs": summary.run_cpu_time_ns,
            "runInstructions": summary.run_instructions,
            "ipc": ipc,
            "diagnosis": diagnosis,
            "hot": summary.hot.map(|hot| json!({
                "source": hot.source,
                "line": hot.line,
                "sampleCount": hot.sample_count,
                "sampleShare": hot.sample_share,
            })),
            "unresolvedSamples": summary.unresolved_samples,
            "unsampledThreadCount": unsampled_thread_count(&self.perf_overlay),
            "physFootprintDelta": phys_footprint_delta,
        }))
    }

    #[cfg(not(feature = "perf"))]
    pub(super) fn perf_stopped_summary_body(&self) -> Option<Value> {
        None
    }
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn open_active_perf_run(
    tid: Pid,
    resolver: Option<bs_perf::decoder::SourceResolver>,
) -> anyhow::Result<ActivePerfRun> {
    let mut monitor = bs_perf::open_cycles_for_pid(tid.as_raw())?;
    let ring = monitor.mmap_ring(bs_perf::linux::ring::DEFAULT_RING_DATA_PAGES)?;
    monitor.reset().and_then(|_| monitor.enable())?;
    // Best-effort instructions counter alongside the sampler. If
    // the kernel refuses (perf_event_paranoid changed mid-flight,
    // rare), we still get cycles+IP — runInstructions stays None.
    let instructions = match bs_perf::linux::open_instructions_for_pid(tid.as_raw()) {
        Ok(mut counter) => {
            let init = counter.reset().and_then(|_| counter.enable());
            if let Err(err) = init {
                log::debug!(target: "perf", "instructions counter enable failed for tid {tid}: {err}");
                None
            } else {
                Some(counter)
            }
        }
        Err(err) => {
            log::debug!(target: "perf", "instructions counter open failed for tid {tid}: {err}");
            None
        }
    };
    Ok(ActivePerfRun {
        tid,
        monitor,
        ring,
        pt_capture: None,
        resolver,
        started_at: Instant::now(),
        instructions,
    })
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn open_active_intel_pt_capture(
    tid: Pid,
    pmu_type: u32,
) -> anyhow::Result<bs_perf::linux::IntelPtCapture> {
    let mut capture = bs_perf::linux::IntelPtCapture::open_for_pid_with_pmu_type(
        tid.as_raw(),
        pmu_type,
        bs_perf::linux::default_intel_pt_data_pages()?,
        bs_perf::linux::DEFAULT_INTEL_PT_AUX_BYTES,
    )?;
    capture.start()?;
    Ok(capture)
}

#[cfg(all(feature = "perf", target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
fn record_intel_pt_drain(
    tid: Pid,
    drain: bs_perf::linux::IntelPtCaptureDrain,
    decode_config: Option<&bs_perf::pt_decode::IntelPtDecodeConfig>,
    resolver: Option<&mut bs_perf::decoder::SourceResolver>,
    data: &mut bs_perf::aggregator::PerfData,
    unavailable: &mut Option<String>,
    last_decoded_instructions: &mut u64,
    last_resolved_instructions: &mut u64,
    last_unresolved_instructions: &mut u64,
) {
    if drain.aux_bytes.is_empty() {
        return;
    }
    let Some(decode_config) = decode_config else {
        data.record_unresolved_samples(1);
        append_unavailable(
            unavailable,
            format!("Intel PT decode skipped for tid {tid}: missing executable image map"),
        );
        return;
    };

    let decoded = match bs_perf::pt_decode::decode_intel_pt_instructions(
        &drain.aux_bytes,
        decode_config,
    ) {
        Ok(decoded) => decoded,
        Err(bs_perf::PerfError::Unsupported) => {
            data.record_unresolved_samples(1);
            append_unavailable(
                unavailable,
                format!(
                    "Intel PT decode unavailable for tid {tid}: build Linux x86/x86_64 with --features intel-pt"
                ),
            );
            return;
        }
        Err(err) => {
            data.record_unresolved_samples(1);
            append_unavailable(
                unavailable,
                format!("Intel PT decode failed for tid {tid}: {err}"),
            );
            return;
        }
    };
    *last_decoded_instructions =
        last_decoded_instructions.saturating_add(decoded.instructions.len() as u64);

    let Some(resolver) = resolver else {
        let unresolved = decoded.instructions.len() as u64;
        data.record_unresolved_samples(unresolved);
        *last_unresolved_instructions = last_unresolved_instructions.saturating_add(unresolved);
        append_unavailable(
            unavailable,
            format!(
                "Intel PT source attribution skipped for tid {tid}: source resolver unavailable"
            ),
        );
        return;
    };

    let resolved = resolver.resolve_decoded_pt_trace(&decoded);
    let stats = data.record_resolved_pt_trace(&resolved);
    *last_resolved_instructions =
        last_resolved_instructions.saturating_add(stats.resolved_instructions);
    *last_unresolved_instructions =
        last_unresolved_instructions.saturating_add(stats.unresolved_instructions);
    if stats.decode_skipped_errors != 0 {
        append_unavailable(
            unavailable,
            format!(
                "Intel PT decode for tid {tid} skipped {} packet error(s)",
                stats.decode_skipped_errors
            ),
        );
    }
    if stats.decode_truncated {
        append_unavailable(
            unavailable,
            format!("Intel PT decode for tid {tid} reached its instruction limit"),
        );
    }
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn active_thread_count(session: &PerfOverlaySession) -> usize {
    session.active.len()
}

#[cfg(all(feature = "perf", not(target_os = "linux")))]
fn active_thread_count(_session: &PerfOverlaySession) -> usize {
    0
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn unsampled_thread_count(session: &PerfOverlaySession) -> usize {
    session.unsampled_thread_count
}

#[cfg(all(feature = "perf", not(target_os = "linux")))]
fn unsampled_thread_count(_session: &PerfOverlaySession) -> usize {
    0
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn intel_pt_probe_body(session: &PerfOverlaySession) -> Value {
    let probe = bs_perf::linux::probe_intel_pt();
    let (status, reason) = match &probe.status {
        bs_perf::linux::IntelPtStatus::Available => ("available", Value::Null),
        bs_perf::linux::IntelPtStatus::PermissionLikelyRequired { .. } => {
            ("permissionLikelyRequired", Value::Null)
        }
        bs_perf::linux::IntelPtStatus::Unavailable(reason) => {
            ("unavailable", intel_pt_unavailable_reason_body(reason))
        }
    };

    json!({
        "status": status,
        "pmuType": probe.pmu_type,
        "perfEventParanoid": probe.perf_event_paranoid,
        "reason": reason,
        "requested": session.intel_pt_requested,
        "activeThreadCount": session
            .active
            .iter()
            .filter(|active| active.pt_capture.is_some())
            .count(),
        "decodeImageSectionCount": session
            .pt_decode_config
            .as_ref()
            .map(|config| config.image_sections.len()),
        "decodeImageUnavailable": session.pt_decode_unavailable.clone(),
        "lastAuxBytes": session.pt_last_aux_bytes,
        "lastDecodedInstructions": session.pt_last_decoded_instructions,
        "lastResolvedInstructions": session.pt_last_resolved_instructions,
        "lastUnresolvedInstructions": session.pt_last_unresolved_instructions,
    })
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn intel_pt_unavailable_reason_body(reason: &bs_perf::linux::IntelPtUnavailableReason) -> Value {
    match reason {
        bs_perf::linux::IntelPtUnavailableReason::UnsupportedArchitecture { arch } => {
            json!({
                "kind": "unsupportedArchitecture",
                "arch": arch,
            })
        }
        bs_perf::linux::IntelPtUnavailableReason::PmuMissing { path } => {
            json!({
                "kind": "pmuMissing",
                "path": path,
            })
        }
        bs_perf::linux::IntelPtUnavailableReason::InvalidPmuType { path, value } => {
            json!({
                "kind": "invalidPmuType",
                "path": path,
                "value": value,
            })
        }
        bs_perf::linux::IntelPtUnavailableReason::InvalidPerfEventParanoid { path, value } => {
            json!({
                "kind": "invalidPerfEventParanoid",
                "path": path,
                "value": value,
            })
        }
        bs_perf::linux::IntelPtUnavailableReason::Io { path, error } => {
            json!({
                "kind": "io",
                "path": path,
                "error": error,
            })
        }
    }
}

#[cfg(all(feature = "perf", not(target_os = "linux")))]
fn intel_pt_probe_body(_session: &PerfOverlaySession) -> Value {
    Value::Null
}

/// macOS Tier 1 kperf probe body. Available on every host that
/// links against the perf feature; on macOS reports library load +
/// PMU permission state, elsewhere `null`. Clients can use it to
/// distinguish "kperf permission-blocked" (signing missing) from
/// "kperf dylib missing" (wrong macOS).
#[cfg(all(feature = "perf", target_os = "macos"))]
fn kperf_probe_body() -> Value {
    use bs_perf::darwin::{KperfStatus, KperfUnavailableReason, probe_kperf};
    match probe_kperf() {
        KperfStatus::Available {
            library_path,
            configurable_counters,
        } => json!({
            "status": "available",
            "libraryPath": library_path,
            "configurableCounters": configurable_counters,
            "reason": Value::Null,
        }),
        KperfStatus::PermissionLikelyRequired {
            library_path,
            configurable_counters,
            force_set_errno,
        } => json!({
            "status": "permissionLikelyRequired",
            "libraryPath": library_path,
            "configurableCounters": configurable_counters,
            "reason": json!({
                "kind": "kpcForceAllCtrsSetRefused",
                "errno": force_set_errno,
                "hint": "kpc_force_all_ctrs_set returned EPERM/EBUSY — the bs binary likely \
                        needs the com.apple.private.kpc.read-or-trace entitlement or to run \
                        with elevated privileges.",
            }),
        }),
        KperfStatus::Unavailable(KperfUnavailableReason::LibraryNotFound { dlerror }) => json!({
            "status": "unavailable",
            "reason": json!({
                "kind": "libraryNotFound",
                "dlerror": dlerror,
            }),
        }),
        KperfStatus::Unavailable(KperfUnavailableReason::MissingSymbol {
            path,
            symbol,
            dlerror,
        }) => json!({
            "status": "unavailable",
            "reason": json!({
                "kind": "missingSymbol",
                "path": path,
                "symbol": symbol,
                "dlerror": dlerror,
            }),
        }),
    }
}

#[cfg(all(feature = "perf", not(target_os = "macos")))]
fn kperf_probe_body() -> Value {
    Value::Null
}

/// Label describing which collection tier produced the latest
/// summary. The VSCode client renders this in its tooltip so users
/// can tell at a glance whether the gutter heat-map should be
/// expected (Linux cycles, macOS poll) or always-empty (macOS
/// rusage-only fallback).
///
/// Values:
/// - `linux-cycles` — Linux PMU cycles+IP via perf_event_open.
/// - `macos-poll` — macOS Tier 1b polling sampler is active.
/// - `macos-rusage-only` — macOS Tier 2 only (sampler unavailable
///   or disabled); body carries CPU time but no per-line samples.
/// - `disabled` — overlay not enabled.
#[cfg(all(feature = "perf", target_os = "linux"))]
fn perf_mode_label(session: &PerfOverlaySession) -> &'static str {
    if !session.enabled {
        "disabled"
    } else {
        "linux-cycles"
    }
}

#[cfg(all(feature = "perf", target_os = "macos"))]
fn perf_mode_label(session: &PerfOverlaySession) -> &'static str {
    if !session.enabled {
        "disabled"
    } else if session.last_run_sampler_active {
        // The live `darwin_run`/sampler are consumed in finish_perf_stop
        // before this runs, so we trust the snapshot taken there rather
        // than the now-empty `darwin_run`.
        "macos-poll"
    } else {
        "macos-rusage-only"
    }
}

#[cfg(all(feature = "perf", not(any(target_os = "linux", target_os = "macos"))))]
fn perf_mode_label(_session: &PerfOverlaySession) -> &'static str {
    "disabled"
}

/// Instructions-per-cycle, the most diagnostic single perf number.
/// `None` when either counter is missing or zero (can't divide).
#[cfg(feature = "perf")]
fn ipc_for(summary: &bs_perf::dap::PerfStoppedSummary) -> Option<f64> {
    let instructions = summary.run_instructions?;
    if summary.run_cycles == 0 || instructions == 0 {
        return None;
    }
    Some(instructions as f64 / summary.run_cycles as f64)
}

/// Emoji-coded root-cause hint, in the rustc-helpful-error tradition.
/// Looks at IPC and any platform-specific diagnostic counters
/// (page-ins, disk I/O on macOS; cache/branch misses on Linux once
/// those land) to suggest what bottleneck the last run hit.
///
/// Returns `null` when there's no useful signal — better silence
/// than misleading guesses on tiny runs with single-digit cycles.
#[cfg(feature = "perf")]
fn diagnose(
    summary: &bs_perf::dap::PerfStoppedSummary,
    #[allow(unused_variables)] session: &PerfOverlaySession,
) -> Value {
    // Need a meaningful window — under ~10µs the counters are too
    // noisy to read causality from. Stop tries that happen on stop-
    // on-entry, instant breakpoints, etc. would otherwise dominate
    // the diagnosis output with garbage.
    if summary.run_wall_ns < 10_000 {
        return Value::Null;
    }

    let cpu_share = if summary.run_wall_ns != 0 {
        summary
            .run_cpu_time_ns
            .map(|cpu| cpu as f64 / summary.run_wall_ns as f64)
    } else {
        None
    };
    let ipc = ipc_for(summary);

    // Platform-specific diagnostic signals.
    #[cfg(target_os = "macos")]
    let diag = session.darwin_last_delta;
    #[cfg(target_os = "macos")]
    {
        if diag.disk_bytes_read >= 64 * 1024 {
            return diagnosis_body(
                "📀",
                "disk-read",
                &format!(
                    "{} read from disk during this run",
                    format_bytes(diag.disk_bytes_read)
                ),
                "hot data is being demand-paged or freshly opened; consider memory-mapping or warming the cache",
            );
        }
        if diag.disk_bytes_written >= 64 * 1024 {
            return diagnosis_body(
                "💿",
                "disk-write",
                &format!(
                    "{} written to disk during this run",
                    format_bytes(diag.disk_bytes_written)
                ),
                "consider batching writes or moving them off the hot path",
            );
        }
        if diag.pageins >= 16 {
            return diagnosis_body(
                "💾",
                "page-ins",
                &format!("{} page-in(s) — memory paged from disk", diag.pageins),
                "working set may exceed RAM, or you're hitting a freshly-mapped region",
            );
        }
    }

    // Generic CPU-bound vs wait-bound classification, available on
    // both Linux and macOS once CPU time is reported.
    if let Some(share) = cpu_share
        && share < 0.25
    {
        let summary_text = format!("CPU active for {:.0}% of wall time", share * 100.0);
        // Go one level deeper: the sampled syscall names the wait
        // (lock / sleep / I/O / IPC). Falls back to the generic line
        // when nothing classifiable was sampled.
        #[cfg(target_os = "macos")]
        if let Some(w) = session.last_wait_syscall.and_then(classify_wait) {
            return diagnosis_body(w.emoji, w.label, &summary_text, w.hint);
        }
        return diagnosis_body(
            "💤",
            "mostly-waiting",
            &summary_text,
            "blocked on I/O, sleep, lock contention, or syscall — sampling won't help; check thread state",
        );
    }

    // IPC-based classification.
    if let Some(ipc) = ipc {
        if ipc < 0.5 {
            return diagnosis_body(
                "🐌",
                "low-ipc",
                &format!("IPC {ipc:.2} — CPU stalls dominate"),
                "likely memory-bound or branch-mispredict; try smaller hot structs, better locality, or `perf record` for confirmation",
            );
        }
        if ipc > 2.5 {
            return diagnosis_body(
                "🚀",
                "high-ipc",
                &format!("IPC {ipc:.2} — CPU running healthy"),
                "compute-bound; optimisation gains come from doing fewer instructions, not making them cheaper",
            );
        }
        return diagnosis_body(
            "⚖️",
            "balanced",
            &format!("IPC {ipc:.2} — no single bottleneck"),
            "neither CPU-bound nor wait-bound; profile a longer window if you need more signal",
        );
    }

    Value::Null
}

#[cfg(feature = "perf")]
fn diagnosis_body(emoji: &str, label: &str, summary: &str, hint: &str) -> Value {
    json!({
        "emoji": emoji,
        "label": label,
        "summary": summary,
        "hint": hint,
    })
}

/// A named wait cause derived from the blocking syscall.
#[cfg(all(feature = "perf", target_os = "macos"))]
struct WaitClass {
    emoji: &'static str,
    label: &'static str,
    hint: &'static str,
}

/// Classify a blocking syscall (`x16`) into a wait cause. Numbers from
/// macOS `bsd/kern/syscalls.master` (arm64); a negative value is a mach
/// trap. `None` for syscalls we don't recognise as a blocking wait.
#[cfg(all(feature = "perf", target_os = "macos"))]
fn classify_wait(syscall: i64) -> Option<WaitClass> {
    if syscall < 0 {
        return Some(WaitClass {
            emoji: "📨",
            label: "ipc/mach wait",
            hint: "parked in a mach trap (mach_msg / semaphore) — waiting on IPC or a dispatch queue; the work is in another thread or process",
        });
    }
    let (emoji, label, hint) = match syscall {
        // __psynch_{mutexwait, cvwait, rw_rdlock, rw_wrlock}, __ulock_wait{,2}
        301 | 302 | 304 | 305 | 515 | 516 => (
            "🔒",
            "lock contention",
            "blocked acquiring a mutex/condvar/rwlock — another thread holds it; shrink the critical section or reduce shared state",
        ),
        // __semwait_signal — nanosleep, sem_wait, timed condvar wait
        334 => (
            "😴",
            "sleep / semaphore",
            "parked in __semwait_signal — an explicit sleep, sem_wait, or timed wait; expected if you meant to block",
        ),
        // read, recvmsg, recvfrom, readv, accept, connect, select, poll, kevent{,_qos,_id}
        3 | 27 | 29 | 120 | 30 | 98 | 93 | 230 | 363 | 374 | 375 => (
            "🌐",
            "i/o wait",
            "blocked on read/recv/kevent/poll — file or socket I/O; the latency is external, not your CPU",
        ),
        _ => return None,
    };
    Some(WaitClass { emoji, label, hint })
}

/// Pick the syscall to report from a window's `x16` samples. Prefers the
/// most frequent *recognised positive* BSD syscall — that's the stepped
/// thread's real wait. Idle runtime threads sit in mach traps (negative),
/// so we only fall back to those when no positive wait was sampled.
#[cfg(all(feature = "perf", target_os = "macos"))]
fn dominant_wait_syscall(samples: &[i64]) -> Option<i64> {
    use std::collections::HashMap;
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for &s in samples {
        *counts.entry(s).or_default() += 1;
    }
    let positive = counts
        .iter()
        .filter_map(|(&s, &c)| (s > 0 && classify_wait(s).is_some()).then_some((s, c)))
        .max_by_key(|&(_, c)| c)
        .map(|(s, _)| s);
    positive.or_else(|| {
        counts
            .iter()
            .filter_map(|(&s, &c)| (s < 0).then_some((s, c)))
            .max_by_key(|&(_, c)| c)
            .map(|(s, _)| s)
    })
}

// --- EXPERIMENTAL thread isolation (BS_PERF_ISOLATE_THREAD) ---------------
// Freeze every non-focus thread during a step so the process-wide rusage
// delta reflects only the stepped thread (otherwise a runtime worker's
// instructions get billed to the stepped line). A watchdog thaws on a stall:
// a step that blocks on a frozen thread never reaches finish_perf_stop (the
// step's exception receive is infinite-timeout), so the main loop would hang.

/// Thaw if a step hasn't completed by here. Generous — most steps finish in
/// well under a frame; this only fires on a genuine block-on-frozen-thread.
#[cfg(all(feature = "perf", target_os = "macos"))]
const FREEZE_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(1);

/// Shared between the guard (thawed in finish_perf_stop / on drop) and the
/// watchdog thread. Whichever calls `thaw` first resumes + releases the
/// suspended thread send rights; the other is a no-op.
#[cfg(all(feature = "perf", target_os = "macos"))]
#[derive(Debug)]
struct FreezeInner {
    suspended: std::sync::Mutex<Vec<mach2::mach_types::thread_act_t>>,
    thawed: std::sync::atomic::AtomicBool,
}

#[cfg(all(feature = "perf", target_os = "macos"))]
impl FreezeInner {
    fn thaw(&self) {
        use std::sync::atomic::Ordering;
        if self.thawed.swap(true, Ordering::SeqCst) {
            return;
        }
        let ports = std::mem::take(&mut *self.suspended.lock().unwrap());
        for t in ports {
            let _ = crate::debugger::darwin_mach::thread_resume(t);
            // SAFETY: `t` is a send right obtained from task_threads; release it.
            let _ = unsafe {
                mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), t)
            };
        }
    }
}

#[cfg(all(feature = "perf", target_os = "macos"))]
#[derive(Debug)]
struct ThreadFreezeGuard {
    inner: std::sync::Arc<FreezeInner>,
}

#[cfg(all(feature = "perf", target_os = "macos"))]
impl Drop for ThreadFreezeGuard {
    fn drop(&mut self) {
        self.inner.thaw();
    }
}

/// Suspend every thread except the one backing `focus_pid`, returning a
/// guard that thaws on drop. Focus is matched by **thread id** (not port
/// name — task_threads hands out distinct send rights for the same thread),
/// so the stepped thread is never frozen. Spawns a watchdog that thaws after
/// [`FREEZE_WATCHDOG`] in case the step blocks on a frozen thread. Returns
/// `None` when single-threaded or the focus thread can't be identified.
#[cfg(all(feature = "perf", target_os = "macos"))]
fn install_thread_freeze(task: mach2::mach_types::task_t, focus_pid: nix::unistd::Pid) -> Option<ThreadFreezeGuard> {
    use crate::debugger::darwin_mach;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    let dealloc = |t: mach2::mach_types::thread_act_t| {
        // SAFETY: `t` is a send right we own from task_threads.
        let _ = unsafe { mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), t) };
    };

    let focus_port = darwin_mach::thread_port_for_pid_or_first(focus_pid).ok()?;
    let focus_tid = darwin_mach::thread_identity(focus_port).ok()?.thread_id;

    let mut suspended = Vec::new();
    for t in darwin_mach::task_threads_vec(task).ok()? {
        if darwin_mach::thread_identity(t).ok().map(|id| id.thread_id) == Some(focus_tid) {
            dealloc(t); // never freeze the stepped thread
            continue;
        }
        if darwin_mach::thread_suspend(t).is_ok() {
            suspended.push(t);
        } else {
            dealloc(t);
        }
    }
    if suspended.is_empty() {
        return None; // single-threaded — nothing to isolate
    }

    let inner = Arc::new(FreezeInner {
        suspended: Mutex::new(suspended),
        thawed: AtomicBool::new(false),
    });
    let watchdog = inner.clone();
    let _ = std::thread::Builder::new()
        .name("bs-perf-freeze-watchdog".to_owned())
        .spawn(move || {
            std::thread::sleep(FREEZE_WATCHDOG);
            watchdog.thaw(); // no-op if finish_perf_stop already thawed
        });
    Some(ThreadFreezeGuard { inner })
}

#[cfg(all(feature = "perf", target_os = "macos"))]
fn format_bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024 * 1024 {
        format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0))
    } else if b >= 1024 {
        format!("{:.1} KiB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

#[cfg(all(feature = "perf", any(target_os = "linux", target_os = "macos")))]
fn append_unavailable(slot: &mut Option<String>, msg: String) {
    if let Some(existing) = slot {
        existing.push_str("; ");
        existing.push_str(&msg);
    } else {
        *slot = Some(msg);
    }
}

/// First-image (main executable) load address from dyld. Used as
/// the load bias for the source resolver — sampled PCs are runtime
/// addresses, DWARF rows are file-relative, the slide is what
/// turns the former into the latter.
#[cfg(all(feature = "perf", target_os = "macos"))]
fn darwin_main_image_load_addr(task: mach2::port::mach_port_t) -> Option<u64> {
    use crate::debugger::darwin_mach;
    let images = darwin_mach::dyld_image_list(task).ok()?;
    images.first().map(|image| image.load_addr as u64)
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn source_resolver_for_process(
    proc_pid: Pid,
    program: &Path,
) -> anyhow::Result<bs_perf::decoder::SourceResolver> {
    let mut resolver = bs_perf::decoder::SourceResolver::from_object_path(program)?;
    if object_needs_load_bias(program)?
        && let Some(load_bias) = executable_load_bias(proc_pid, program)
    {
        resolver = resolver.with_load_bias(load_bias);
    }
    Ok(resolver)
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn intel_pt_decode_config_for_process(
    proc_pid: Pid,
) -> anyhow::Result<bs_perf::pt_decode::IntelPtDecodeConfig> {
    let maps = proc_maps::get_process_maps(proc_pid.as_raw())?;
    let mut config = bs_perf::pt_decode::IntelPtDecodeConfig::new();
    for map in maps {
        if let Some(section) = intel_pt_image_section_from_map(&map) {
            config.image_sections.push(section);
        }
    }
    Ok(config)
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn intel_pt_image_section_from_map(
    map: &proc_maps::MapRange,
) -> Option<bs_perf::pt_decode::IntelPtImageSection> {
    if !map.is_exec() || map.size() == 0 {
        return None;
    }
    let path = map.filename()?;
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    Some(bs_perf::pt_decode::IntelPtImageSection::new(
        canonical_or_self(path),
        map.offset as u64,
        map.size() as u64,
        map.start() as u64,
    ))
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn object_needs_load_bias(program: &Path) -> anyhow::Result<bool> {
    use object::Object;

    let bytes = std::fs::read(program)?;
    let file = object::File::parse(bytes.as_slice())?;
    Ok(file.kind() == object::ObjectKind::Dynamic)
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn executable_load_bias(proc_pid: Pid, program: &Path) -> Option<u64> {
    let maps = proc_maps::get_process_maps(proc_pid.as_raw()).ok()?;
    let wanted = canonical_or_self(program);
    maps.into_iter()
        .filter(|map| map.is_exec())
        .find_map(|map| {
            let path = map.filename()?;
            if !same_file_or_suffix(path, &wanted) {
                return None;
            }
            let start = map.start() as u64;
            let offset = map.offset as u64;
            Some(start.saturating_sub(offset))
        })
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(all(feature = "perf", target_os = "linux"))]
fn same_file_or_suffix(actual: &Path, wanted: &Path) -> bool {
    let actual = canonical_or_self(actual);
    actual == wanted
        || actual.ends_with(wanted)
        || wanted.ends_with(&actual)
        || actual
            .file_name()
            .is_some_and(|name| wanted.file_name() == Some(name))
}

#[cfg(feature = "perf")]
fn parse_perf_overlay_enable_request(req: &DapRequest) -> bs_perf::dap::PerfOverlayEnableRequest {
    let intel_pt = req
        .arguments
        .get("intelPt")
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.get("enabled").and_then(Value::as_bool))
        })
        .or_else(|| req.arguments.get("precise").and_then(Value::as_bool))
        .unwrap_or(false);

    bs_perf::dap::PerfOverlayEnableRequest { intel_pt }
}

#[cfg(feature = "perf")]
fn parse_perf_overlay_request(
    req: &DapRequest,
) -> anyhow::Result<bs_perf::dap::PerfOverlayRequest> {
    Ok(bs_perf::dap::PerfOverlayRequest {
        source: parse_perf_overlay_source(req)?,
        window: parse_perf_overlay_window(req)?,
    })
}

fn parse_perf_overlay_source(req: &DapRequest) -> anyhow::Result<String> {
    if let Some(source) = req.arguments.get("source") {
        if let Some(path) = source.as_str() {
            return Ok(path.to_owned());
        }
        if let Some(path) = source.get("path").and_then(Value::as_str) {
            return Ok(path.to_owned());
        }
    }
    if let Some(path) = req.arguments.get("path").and_then(Value::as_str) {
        return Ok(path.to_owned());
    }
    Err(anyhow!(
        "bs/perfOverlay: missing arguments.source.path or arguments.path"
    ))
}

#[cfg(feature = "perf")]
fn parse_perf_overlay_window(req: &DapRequest) -> anyhow::Result<bs_perf::dap::PerfOverlayWindow> {
    match req.arguments.get("window").and_then(Value::as_str) {
        None | Some("lastRun") | Some("last-run") => Ok(bs_perf::dap::PerfOverlayWindow::LastRun),
        Some("cumulative") => Ok(bs_perf::dap::PerfOverlayWindow::Cumulative),
        Some(other) => Err(anyhow!(
            "bs/perfOverlay: unknown arguments.window {other:?}; expected lastRun or cumulative"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(arguments: Value) -> DapRequest {
        DapRequest {
            seq: 1,
            r#type: "request".to_owned(),
            command: "bs/perfOverlay".to_owned(),
            arguments,
        }
    }

    #[test]
    fn perf_overlay_source_accepts_dap_source_object() {
        let req = request(json!({
            "source": { "path": "/work/src/main.rs" }
        }));

        assert_eq!(
            parse_perf_overlay_source(&req).expect("source"),
            "/work/src/main.rs"
        );
    }

    #[test]
    fn perf_overlay_source_accepts_path_shorthand() {
        let req = request(json!({
            "path": "src/main.rs"
        }));

        assert_eq!(
            parse_perf_overlay_source(&req).expect("source"),
            "src/main.rs"
        );
    }

    #[test]
    fn perf_overlay_source_rejects_missing_path() {
        let req = request(json!({}));

        let err = parse_perf_overlay_source(&req).expect_err("missing path");
        assert!(err.to_string().contains("missing arguments.source.path"));
    }

    // Regression: finish_perf_stop take()s darwin_run and drains the
    // sampler, so by the time the stopped summary calls perf_mode_label
    // the live sampler is gone. The label must rely on the captured
    // last_run_sampler_active flag, not the (now None) darwin_run — else
    // it wrongly reports rusage-only even though the poll sampler ran.
    // (The full ordering repro needs a live Mach sampler + debuggee, so
    // this asserts the label decision directly with the post-stop state.)
    #[cfg(all(feature = "perf", target_os = "macos"))]
    #[test]
    fn perf_mode_label_reports_poll_after_sampler_consumed() {
        let mut session = PerfOverlaySession::default();
        session.enabled = true;
        session.darwin_run = None; // sampler already consumed at stop
        session.last_run_sampler_active = true;
        assert_eq!(perf_mode_label(&session), "macos-poll");

        session.last_run_sampler_active = false;
        assert_eq!(perf_mode_label(&session), "macos-rusage-only");

        session.enabled = false;
        assert_eq!(perf_mode_label(&session), "disabled");
    }

    #[cfg(all(feature = "perf", target_os = "macos"))]
    #[test]
    fn classify_wait_maps_syscalls_to_causes() {
        assert_eq!(classify_wait(302).unwrap().label, "lock contention"); // __psynch_mutexwait
        assert_eq!(classify_wait(515).unwrap().label, "lock contention"); // __ulock_wait
        assert_eq!(classify_wait(334).unwrap().label, "sleep / semaphore"); // __semwait_signal
        assert_eq!(classify_wait(3).unwrap().label, "i/o wait"); // read
        assert_eq!(classify_wait(363).unwrap().label, "i/o wait"); // kevent
        assert_eq!(classify_wait(-31).unwrap().label, "ipc/mach wait"); // mach_msg trap
        assert!(classify_wait(20).is_none()); // getpid — not a blocking wait
    }

    #[cfg(all(feature = "perf", target_os = "macos"))]
    #[test]
    fn dominant_wait_prefers_positive_blocking_over_idle_mach() {
        // idle threads parked in mach_msg (-31), one stepped thread in read(3):
        // the real wait wins over the mach-trap noise.
        assert_eq!(dominant_wait_syscall(&[-31, -31, -31, -31, -31, 3]), Some(3));
        // only mach traps → fall back to the mach trap.
        assert_eq!(dominant_wait_syscall(&[-31, -31]), Some(-31));
        // unrecognised positive syscalls don't win; mach fallback applies.
        assert_eq!(dominant_wait_syscall(&[20, 20, -31]), Some(-31));
        // nothing sampled → nothing to report.
        assert_eq!(dominant_wait_syscall(&[]), None);
    }

    // The watchdog and finish_perf_stop can both reach thaw(); only the
    // first may resume/release ports. (Real suspend/resume needs the live
    // debugger thread-port registry — exercised by BS_PERF_ISOLATE_THREAD.)
    #[cfg(all(feature = "perf", target_os = "macos"))]
    #[test]
    fn freeze_thaw_is_idempotent() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let inner = Arc::new(FreezeInner {
            suspended: Mutex::new(Vec::new()),
            thawed: AtomicBool::new(false),
        });
        inner.thaw();
        inner.thaw(); // must be a no-op, not a double free / panic
        assert!(inner.thawed.load(Ordering::SeqCst));
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_window_accepts_known_values() {
        let last_run = request(json!({ "path": "src/main.rs" }));
        assert_eq!(
            parse_perf_overlay_window(&last_run).expect("default"),
            bs_perf::dap::PerfOverlayWindow::LastRun
        );

        let cumulative = request(json!({
            "path": "src/main.rs",
            "window": "cumulative"
        }));
        assert_eq!(
            parse_perf_overlay_window(&cumulative).expect("cumulative"),
            bs_perf::dap::PerfOverlayWindow::Cumulative
        );
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_enable_defaults_to_cycles_sampling() {
        let req = request(json!({}));

        assert_eq!(
            parse_perf_overlay_enable_request(&req),
            bs_perf::dap::PerfOverlayEnableRequest { intel_pt: false }
        );
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_enable_accepts_intel_pt_boolean() {
        let req = request(json!({
            "intelPt": true
        }));

        assert_eq!(
            parse_perf_overlay_enable_request(&req),
            bs_perf::dap::PerfOverlayEnableRequest { intel_pt: true }
        );
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_enable_accepts_intel_pt_object() {
        let req = request(json!({
            "intelPt": { "enabled": true }
        }));

        assert_eq!(
            parse_perf_overlay_enable_request(&req),
            bs_perf::dap::PerfOverlayEnableRequest { intel_pt: true }
        );
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_enable_accepts_precise_alias() {
        let req = request(json!({
            "precise": true
        }));

        assert_eq!(
            parse_perf_overlay_enable_request(&req),
            bs_perf::dap::PerfOverlayEnableRequest { intel_pt: true }
        );
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_overlay_enable_explicit_intel_pt_overrides_precise_alias() {
        let req = request(json!({
            "intelPt": false,
            "precise": true
        }));

        assert_eq!(
            parse_perf_overlay_enable_request(&req),
            bs_perf::dap::PerfOverlayEnableRequest { intel_pt: false }
        );
    }

    #[cfg(all(feature = "perf", target_os = "linux"))]
    #[test]
    fn unavailable_messages_are_appended_in_order() {
        let mut slot = None;

        append_unavailable(&mut slot, "first".to_owned());
        append_unavailable(&mut slot, "second".to_owned());

        assert_eq!(slot.as_deref(), Some("first; second"));
    }

    #[cfg(all(feature = "perf", not(target_os = "linux")))]
    #[test]
    fn intel_pt_probe_body_is_null_off_linux() {
        assert_eq!(
            intel_pt_probe_body(&PerfOverlaySession::default()),
            Value::Null
        );
    }
}

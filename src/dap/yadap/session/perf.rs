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

    #[cfg(all(feature = "perf", not(target_os = "linux")))]
    pub(super) fn begin_perf_run(&mut self) {
        if self.perf_overlay.enabled {
            self.perf_overlay.unavailable =
                Some("perf overlay live collection is Linux-only in this phase".to_owned());
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
        self.perf_overlay.data.finish_stop(0, wall_ns);
    }

    #[cfg(all(feature = "perf", not(target_os = "linux")))]
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
        #[cfg(not(target_os = "linux"))]
        {
            self.perf_overlay.enabled = false;
            self.perf_overlay.unavailable =
                Some("perf overlay live collection is Linux-only in this phase".to_owned());
        }
        let response = bs_perf::dap::enable(&enable_req);
        self.send_success_body(
            req,
            json!({
                "enabled": self.perf_overlay.enabled && response.enabled,
                "unavailable": self.perf_overlay.unavailable.clone(),
                "intelPt": intel_pt_probe_body(&self.perf_overlay),
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
        Some(json!({
            "runCycles": summary.run_cycles,
            "runWallNs": summary.run_wall_ns,
            "hot": summary.hot.map(|hot| json!({
                "source": hot.source,
                "line": hot.line,
                "sampleCount": hot.sample_count,
                "sampleShare": hot.sample_share,
            })),
            "unresolvedSamples": summary.unresolved_samples,
            "unsampledThreadCount": unsampled_thread_count(&self.perf_overlay),
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
    Ok(ActivePerfRun {
        tid,
        monitor,
        ring,
        pt_capture: None,
        resolver,
        started_at: Instant::now(),
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

#[cfg(all(feature = "perf", target_os = "linux"))]
fn append_unavailable(slot: &mut Option<String>, msg: String) {
    if let Some(existing) = slot {
        existing.push_str("; ");
        existing.push_str(&msg);
    } else {
        *slot = Some(msg);
    }
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

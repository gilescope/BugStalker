// SPDX-License-Identifier: MIT
//! Rust shapes for Phase 6 perf-overlay DAP integration.
//!
//! These are owned Rust request/response types, deliberately kept
//! serde-free like `bs_replay_driver::dap`. The DAP server converts
//! them to/from JSON at its boundary; this crate owns the stable
//! semantics for `bs/perfOverlay`, stopped-event `body.bs_perf`, and
//! overlay enable/disable acknowledgement.

use crate::aggregator::{PerfData, SampleCount};
use crate::overlay::{self, OverlayWindow};

/// Custom DAP command for source heat-map rows.
pub const PERF_OVERLAY_COMMAND: &str = "bs/perfOverlay";
/// Custom DAP command for enabling overlay collection/rendering.
pub const PERF_OVERLAY_ENABLE_COMMAND: &str = "bs/perfOverlayEnable";
/// Custom DAP command for disabling overlay collection/rendering.
pub const PERF_OVERLAY_DISABLE_COMMAND: &str = "bs/perfOverlayDisable";

/// Request: return perf overlay rows for one source file.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PerfOverlayRequest {
    /// Source path as supplied by the DAP client. It may be
    /// absolute even when DWARF rows are relative; the handler
    /// applies the same exact-then-suffix matching as the console
    /// overlay.
    pub source: String,
    /// Aggregation window to project.
    pub window: PerfOverlayWindow,
}

/// DAP-facing mirror of [`OverlayWindow`].
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum PerfOverlayWindow {
    /// Samples since the last continue/resume boundary.
    #[default]
    LastRun,
    /// Samples since attach.
    Cumulative,
}

impl From<PerfOverlayWindow> for OverlayWindow {
    fn from(window: PerfOverlayWindow) -> Self {
        match window {
            PerfOverlayWindow::LastRun => OverlayWindow::LastRun,
            PerfOverlayWindow::Cumulative => OverlayWindow::Cumulative,
        }
    }
}

/// Response: heat-map rows for the requested source.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfOverlayResponse {
    /// Source path echoed from the request.
    pub source: String,
    /// Lines with non-zero samples.
    pub lines: Vec<PerfOverlayLine>,
    /// Resolved samples in the selected window across all files.
    pub total_resolved_samples: SampleCount,
    /// Samples that could not be resolved to a source line.
    pub unresolved_samples: SampleCount,
}

/// One DAP heat-map line.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfOverlayLine {
    /// One-based source line.
    pub line: u64,
    /// Samples attributed to this line.
    pub sample_count: SampleCount,
    /// Share of all resolved samples in the selected window.
    pub sample_share: f64,
    /// Per-source heat value relative to the hottest line in this
    /// source.
    pub heat: f64,
    /// True when this line is tied for hottest in this source.
    pub hottest: bool,
}

/// Perf summary attached to a DAP `StoppedEvent` body as
/// `body.bs_perf`.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfStoppedSummary {
    /// Cycle count for the run-to-stop window.
    pub run_cycles: u64,
    /// Wall-clock duration for the run-to-stop window.
    pub run_wall_ns: u64,
    /// Whole-process CPU time (user + system, ns) for the run, when
    /// the collector reports time rather than cycles. macOS Tier 2
    /// fills this from `proc_pid_rusage`. `None` on Linux cycles+IP.
    pub run_cpu_time_ns: Option<u64>,
    /// Hottest resolved source line, if one exists.
    pub hot: Option<PerfHotLine>,
    /// Samples that did not resolve to source in this stop.
    pub unresolved_samples: SampleCount,
}

/// Hottest-line object used in stopped-event summaries.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfHotLine {
    /// Source path.
    pub source: String,
    /// One-based source line.
    pub line: u64,
    /// Samples attributed to this line.
    pub sample_count: SampleCount,
    /// Share of resolved samples in this stop/window.
    pub sample_share: f64,
}

/// Request: enable perf overlay collection/rendering.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct PerfOverlayEnableRequest {
    /// Request Intel PT capture when the host and build support it.
    ///
    /// Cycles sampling remains the default and continues to run if PT
    /// capture or decode fails.
    pub intel_pt: bool,
}

/// Request: disable perf overlay collection/rendering.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct PerfOverlayDisableRequest {}

/// Response for enable/disable commands.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PerfOverlayToggleResponse {
    /// Whether overlay collection/rendering is enabled after the
    /// request.
    pub enabled: bool,
}

/// Handle `bs/perfOverlay` against current aggregate data.
pub fn perf_overlay(data: &PerfData, req: &PerfOverlayRequest) -> PerfOverlayResponse {
    let overlay = overlay::source_overlay(data, &req.source, req.window.into());
    PerfOverlayResponse {
        source: req.source.clone(),
        lines: overlay
            .lines
            .into_iter()
            .map(|line| PerfOverlayLine {
                line: line.line,
                sample_count: line.samples,
                sample_share: line.sample_share,
                heat: line.heat,
                hottest: line.hottest,
            })
            .collect(),
        total_resolved_samples: overlay.total_resolved_samples,
        unresolved_samples: overlay.unresolved_samples,
    }
}

/// Build the DAP stopped-event perf summary for the latest stop, if
/// a stop has been recorded.
pub fn stopped_summary(data: &PerfData) -> Option<PerfStoppedSummary> {
    overlay::latest_stop_status(data).map(|status| PerfStoppedSummary {
        run_cycles: status.run_cycles,
        run_wall_ns: status.run_wall_ns,
        run_cpu_time_ns: status.run_cpu_time_ns,
        hot: status.hot.map(|hot| PerfHotLine {
            source: hot.file.display().to_string(),
            line: hot.line,
            sample_count: hot.samples,
            sample_share: hot.sample_share,
        }),
        unresolved_samples: status.unresolved_samples,
    })
}

/// Acknowledge `bs/perfOverlayEnable`.
pub fn enable(_: &PerfOverlayEnableRequest) -> PerfOverlayToggleResponse {
    PerfOverlayToggleResponse { enabled: true }
}

/// Acknowledge `bs/perfOverlayDisable`.
pub fn disable(_: &PerfOverlayDisableRequest) -> PerfOverlayToggleResponse {
    PerfOverlayToggleResponse { enabled: false }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::decoder::{ResolvedPc, SourceFrame};

    fn resolved(path: &str, line: u64) -> ResolvedPc {
        ResolvedPc {
            object_pc: 0x10,
            primary: SourceFrame {
                file: PathBuf::from(path),
                line,
                column: 0,
            },
            inlined: Vec::new(),
            is_stmt: true,
            prologue_end: false,
            epilogue_begin: false,
        }
    }

    #[test]
    fn perf_overlay_request_projects_source_rows() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_resolved_pc(&resolved("src/lib.rs", 30));
        data.record_unresolved_pc();

        let rsp = perf_overlay(
            &data,
            &PerfOverlayRequest {
                source: "/work/src/main.rs".to_owned(),
                window: PerfOverlayWindow::LastRun,
            },
        );

        assert_eq!(rsp.source, "/work/src/main.rs");
        assert_eq!(rsp.total_resolved_samples, 3);
        assert_eq!(rsp.unresolved_samples, 1);
        assert_eq!(rsp.lines.len(), 1);
        assert_eq!(rsp.lines[0].line, 10);
        assert_eq!(rsp.lines[0].sample_count, 2);
        assert_eq!(rsp.lines[0].sample_share, 2.0 / 3.0);
        assert_eq!(rsp.lines[0].heat, 1.0);
        assert!(rsp.lines[0].hottest);
    }

    #[test]
    fn stopped_summary_matches_latest_stop() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_unresolved_pc();
        data.finish_stop(100, 200);

        let summary = stopped_summary(&data).expect("summary");
        assert_eq!(summary.run_cycles, 100);
        assert_eq!(summary.run_wall_ns, 200);
        assert_eq!(summary.unresolved_samples, 1);
        let hot = summary.hot.expect("hot");
        assert_eq!(hot.source, "src/main.rs");
        assert_eq!(hot.line, 10);
        assert_eq!(hot.sample_count, 1);
        assert_eq!(hot.sample_share, 1.0);
    }

    #[test]
    fn toggle_responses_echo_resulting_state() {
        assert!(enable(&PerfOverlayEnableRequest { intel_pt: false }).enabled);
        assert!(!disable(&PerfOverlayDisableRequest {}).enabled);
    }
}

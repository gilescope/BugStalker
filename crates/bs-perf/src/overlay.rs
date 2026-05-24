// SPDX-License-Identifier: MIT
//! UI-facing view models for the Phase 6 perf overlay.
//!
//! The collector, decoder, and aggregator deliberately stop at
//! counters. This module is the next boundary: it turns
//! [`PerfData`](crate::aggregator::PerfData) into stable rows that
//! the console, TUI, and DAP server can render without each layer
//! re-defining "hottest", sample share, history numbering, or
//! unresolved-sample visibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::aggregator::{HotLine, PerfData, SampleCount, SourceLine, StopSummary};

/// Which aggregation window should be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayWindow {
    /// Samples collected since the last continue/resume boundary.
    LastRun,
    /// Samples collected since attach.
    Cumulative,
}

/// One source line's overlay data.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayLine {
    /// One-based source line.
    pub line: u64,
    /// Samples attributed to this line in the selected window.
    pub samples: SampleCount,
    /// `samples / total_resolved_samples` for the selected window.
    pub sample_share: f64,
    /// `samples / hottest_line_samples` within the selected source.
    /// TUI gutter colour scales should use this value.
    pub heat: f64,
    /// True for the hottest line or lines in this source.
    pub hottest: bool,
}

/// Overlay rows for a single source file.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceOverlay {
    /// Source path requested by the caller.
    pub source: PathBuf,
    /// Lines with non-zero samples, sorted by line number.
    pub lines: Vec<OverlayLine>,
    /// Resolved samples in the selected window, across all files.
    pub total_resolved_samples: SampleCount,
    /// Unresolved samples in the selected window.
    pub unresolved_samples: SampleCount,
}

/// Hottest-line data with a resolved file path.
#[derive(Debug, Clone, PartialEq)]
pub struct HotLineView {
    /// Source path.
    pub file: PathBuf,
    /// One-based source line.
    pub line: u64,
    /// Samples attributed to this line.
    pub samples: SampleCount,
    /// Share of resolved samples in that stop/window.
    pub sample_share: f64,
}

/// Latest stop summary for console status lines and DAP
/// `StoppedEvent.body.bs_perf`.
#[derive(Debug, Clone, PartialEq)]
pub struct StopStatusView {
    /// Cycle count for the run-to-stop window.
    pub run_cycles: u64,
    /// Wall-clock duration for the run-to-stop window.
    pub run_wall_ns: u64,
    /// Whole-process CPU time (user + system, ns) for the run, when
    /// the collector reports time rather than cycles (macOS Tier 2).
    pub run_cpu_time_ns: Option<u64>,
    /// Retired instructions during the run-to-stop window. Pairs
    /// with `run_cycles` to give IPC.
    pub run_instructions: Option<u64>,
    /// Hottest resolved source line, if any.
    pub hot: Option<HotLineView>,
    /// Samples that did not resolve to a source line.
    pub unresolved_samples: SampleCount,
}

/// One retained `perf history` row.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    /// One-based stop number since attach.
    pub stop_number: u64,
    /// Cycle count for the run-to-stop window.
    pub run_cycles: u64,
    /// Wall-clock duration for the run-to-stop window.
    pub run_wall_ns: u64,
    /// Whole-process CPU time (user + system, ns) for the run, when
    /// the collector reports time rather than cycles.
    pub run_cpu_time_ns: Option<u64>,
    /// Retired instructions during the run-to-stop window.
    pub run_instructions: Option<u64>,
    /// Hottest resolved source line for this stop, if any.
    pub hot: Option<HotLineView>,
    /// Samples that did not resolve to source in this stop.
    pub unresolved_samples: SampleCount,
}

/// Build overlay rows for `source` from the selected aggregation
/// window. Matching is exact first, then suffix-based so a DAP source
/// path like `/work/src/lib.rs` can match a DWARF row recorded as
/// `src/lib.rs`.
pub fn source_overlay(
    data: &PerfData,
    source: impl AsRef<Path>,
    window: OverlayWindow,
) -> SourceOverlay {
    let source = source.as_ref();
    let counts = counts_for_window(data, window);
    let unresolved_samples = match window {
        OverlayWindow::LastRun => data.unresolved_last_run,
        OverlayWindow::Cumulative => data.unresolved_cumulative,
    };
    let total_resolved_samples = counts.values().copied().sum();
    let mut matched = collect_source_lines(data, source, counts, MatchKind::Exact);
    if matched.is_empty() {
        matched = collect_source_lines(data, source, counts, MatchKind::Suffix);
    }

    let hottest_samples = matched.values().copied().max().unwrap_or(0);
    let mut lines = matched
        .into_iter()
        .map(|(line, samples)| OverlayLine {
            line,
            samples,
            sample_share: ratio(samples, total_resolved_samples),
            heat: ratio(samples, hottest_samples),
            hottest: hottest_samples != 0 && samples == hottest_samples,
        })
        .collect::<Vec<_>>();
    lines.sort_by_key(|line| line.line);

    SourceOverlay {
        source: source.to_path_buf(),
        lines,
        total_resolved_samples,
        unresolved_samples,
    }
}

/// Return the most recent stop status, if any stop has been
/// finished.
pub fn latest_stop_status(data: &PerfData) -> Option<StopStatusView> {
    data.stops
        .back()
        .map(|summary| stop_status_view(data, summary))
}

/// Return retained history rows, oldest to newest. Stop numbers are
/// absolute since attach even when old rows have been evicted from
/// the bounded history.
pub fn history_rows(data: &PerfData) -> Vec<HistoryRow> {
    let retained = data.stops.len() as u64;
    let first_stop = data
        .total_stops()
        .saturating_sub(retained)
        .saturating_add(1);
    data.stops
        .iter()
        .enumerate()
        .map(|(idx, summary)| {
            let status = stop_status_view(data, summary);
            HistoryRow {
                stop_number: first_stop + idx as u64,
                run_cycles: status.run_cycles,
                run_wall_ns: status.run_wall_ns,
                run_cpu_time_ns: status.run_cpu_time_ns,
                run_instructions: status.run_instructions,
                hot: status.hot,
                unresolved_samples: status.unresolved_samples,
            }
        })
        .collect()
}

/// Format a cycle count for compact console output.
pub fn format_cycles(cycles: u64) -> String {
    format_scaled(cycles, "cy")
}

/// Format a sample count for compact console output.
pub fn format_samples(samples: SampleCount) -> String {
    format_scaled(samples, "samples")
}

/// Format a nanosecond duration for compact console output.
pub fn format_duration_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.1} s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.1} ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{} us", ns / 1_000)
    } else {
        format!("{ns} ns")
    }
}

/// Render the compact per-stop summary fragment used by console
/// status lines.
pub fn render_stop_status(status: &StopStatusView) -> String {
    let mut out = format!(
        "run cost {} / {}",
        format_cost(status.run_cycles, status.run_cpu_time_ns),
        format_duration_ns(status.run_wall_ns),
    );
    if let Some(hot) = &status.hot {
        out.push_str(&format!(" / hot {}:{}", hot.file.display(), hot.line));
    }
    if status.unresolved_samples != 0 {
        out.push_str(&format!(
            " / unresolved {}",
            format_samples(status.unresolved_samples)
        ));
    }
    out
}

/// Render one `perf history` row.
pub fn render_history_row(row: &HistoryRow) -> String {
    let hot = row
        .hot
        .as_ref()
        .map(|hot| format!("hot={}:{}", hot.file.display(), hot.line))
        .unwrap_or_else(|| "hot=<none>".to_owned());
    let unresolved = if row.unresolved_samples == 0 {
        String::new()
    } else {
        format!(" unresolved={}", format_samples(row.unresolved_samples))
    };
    format!(
        "stop #{}: {}   {}   {hot}{unresolved}",
        row.stop_number,
        format_cost(row.run_cycles, row.run_cpu_time_ns),
        format_duration_ns(row.run_wall_ns),
    )
}

/// Prefer cycles when the collector measured them; fall back to CPU
/// time (e.g. macOS rusage tier). Returns "-" when neither is set,
/// so the column never disappears.
fn format_cost(run_cycles: u64, run_cpu_time_ns: Option<u64>) -> String {
    if run_cycles != 0 {
        return format_cycles(run_cycles);
    }
    if let Some(ns) = run_cpu_time_ns {
        return format!("{} cpu", format_duration_ns(ns));
    }
    "-".to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    Exact,
    Suffix,
}

fn counts_for_window(data: &PerfData, window: OverlayWindow) -> &HashMap<SourceLine, SampleCount> {
    match window {
        OverlayWindow::LastRun => &data.last_run,
        OverlayWindow::Cumulative => &data.cumulative,
    }
}

fn collect_source_lines(
    data: &PerfData,
    source: &Path,
    counts: &HashMap<SourceLine, SampleCount>,
    kind: MatchKind,
) -> HashMap<u64, SampleCount> {
    let mut out = HashMap::new();
    for (&source_line, &samples) in counts {
        let Some(path) = data.file_path(source_line.file) else {
            continue;
        };
        if path_matches(path, source, kind) {
            let count = out.entry(source_line.line).or_insert(0);
            *count = SampleCount::saturating_add(*count, samples);
        }
    }
    out
}

fn path_matches(actual: &Path, requested: &Path, kind: MatchKind) -> bool {
    match kind {
        MatchKind::Exact => actual == requested,
        MatchKind::Suffix => actual.ends_with(requested) || requested.ends_with(actual),
    }
}

fn stop_status_view(data: &PerfData, summary: &StopSummary) -> StopStatusView {
    let resolved_total = summary
        .top_lines
        .iter()
        .map(|line| line.samples)
        .sum::<SampleCount>();
    StopStatusView {
        run_cycles: summary.run_cycles,
        run_wall_ns: summary.run_wall_ns,
        run_cpu_time_ns: summary.run_cpu_time_ns,
        run_instructions: summary.run_instructions,
        hot: summary
            .top_lines
            .first()
            .and_then(|line| hot_line_view(data, line, resolved_total)),
        unresolved_samples: summary.unresolved_samples,
    }
}

fn hot_line_view(
    data: &PerfData,
    line: &HotLine,
    resolved_total: SampleCount,
) -> Option<HotLineView> {
    Some(HotLineView {
        file: data.file_path(line.source.file)?.to_path_buf(),
        line: line.source.line,
        samples: line.samples,
        sample_share: ratio(line.samples, resolved_total),
    })
}

fn ratio(numerator: SampleCount, denominator: SampleCount) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn format_scaled(value: u64, unit: &str) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}G {unit}", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M {unit}", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}k {unit}", value / 1_000)
    } else {
        format!("{value} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::aggregator::PerfData;
    use crate::decoder::{ResolvedPc, SourceFrame};

    fn frame(path: &str, line: u64) -> SourceFrame {
        SourceFrame {
            file: PathBuf::from(path),
            line,
            column: 0,
        }
    }

    fn resolved(path: &str, line: u64) -> ResolvedPc {
        ResolvedPc {
            object_pc: 0x10,
            primary: frame(path, line),
            inlined: Vec::new(),
            is_stmt: true,
            prologue_end: false,
            epilogue_begin: false,
        }
    }

    #[test]
    fn source_overlay_marks_hottest_and_shares() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_resolved_pc(&resolved("src/main.rs", 20));
        data.record_resolved_pc(&resolved("src/main.rs", 20));
        data.record_resolved_pc(&resolved("src/lib.rs", 30));
        data.record_unresolved_pc();

        let overlay = source_overlay(&data, "src/main.rs", OverlayWindow::LastRun);

        assert_eq!(overlay.source, Path::new("src/main.rs"));
        assert_eq!(overlay.total_resolved_samples, 4);
        assert_eq!(overlay.unresolved_samples, 1);
        assert_eq!(overlay.lines.len(), 2);
        assert_eq!(overlay.lines[0].line, 10);
        assert_eq!(overlay.lines[0].samples, 1);
        assert_eq!(overlay.lines[0].sample_share, 0.25);
        assert_eq!(overlay.lines[0].heat, 0.5);
        assert!(!overlay.lines[0].hottest);
        assert_eq!(overlay.lines[1].line, 20);
        assert_eq!(overlay.lines[1].samples, 2);
        assert_eq!(overlay.lines[1].sample_share, 0.5);
        assert_eq!(overlay.lines[1].heat, 1.0);
        assert!(overlay.lines[1].hottest);
    }

    #[test]
    fn source_overlay_uses_suffix_match_after_exact_miss() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved("src/main.rs", 10));

        let overlay = source_overlay(
            &data,
            "/workspace/project/src/main.rs",
            OverlayWindow::LastRun,
        );

        assert_eq!(overlay.lines.len(), 1);
        assert_eq!(overlay.lines[0].line, 10);
    }

    #[test]
    fn latest_status_and_history_resolve_hot_paths() {
        let mut data = PerfData::new(2);
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_resolved_pc(&resolved("src/main.rs", 10));
        data.record_resolved_pc(&resolved("src/lib.rs", 30));
        data.record_unresolved_pc();
        data.finish_stop(3_200_000, 1_400_000);

        data.begin_run();
        data.record_resolved_pc(&resolved("src/lib.rs", 30));
        data.finish_stop(280_000, 130_000);

        data.begin_run();
        data.finish_stop(12, 900);

        let status = latest_stop_status(&data).expect("status");
        assert_eq!(status.run_cycles, 12);
        assert!(status.hot.is_none());

        let rows = history_rows(&data);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stop_number, 2);
        assert_eq!(rows[0].hot.as_ref().unwrap().file, Path::new("src/lib.rs"));
        assert_eq!(rows[0].hot.as_ref().unwrap().sample_share, 1.0);
        assert_eq!(rows[1].stop_number, 3);
        assert!(rows[1].hot.is_none());
    }

    #[test]
    fn compact_formatting_matches_console_shape() {
        let status = StopStatusView {
            run_cycles: 3_200_000,
            run_wall_ns: 1_400_000,
            run_cpu_time_ns: None,
            run_instructions: None,
            hot: Some(HotLineView {
                file: PathBuf::from("src/main.rs"),
                line: 45,
                samples: 20,
                sample_share: 0.5,
            }),
            unresolved_samples: 2,
        };
        assert_eq!(
            render_stop_status(&status),
            "run cost 3.2M cy / 1.4 ms / hot src/main.rs:45 / unresolved 2 samples"
        );

        let row = HistoryRow {
            stop_number: 7,
            run_cycles: 280_000,
            run_wall_ns: 130_000,
            run_cpu_time_ns: None,
            run_instructions: None,
            hot: status.hot,
            unresolved_samples: 0,
        };
        assert_eq!(
            render_history_row(&row),
            "stop #7: 280k cy   130 us   hot=src/main.rs:45"
        );
    }

    /// Tier 2 macOS path: cycles == 0 but rusage gave us CPU time.
    /// Status line should show CPU time instead of "0 cy".
    #[test]
    fn cpu_time_falls_back_when_cycles_absent() {
        let status = StopStatusView {
            run_cycles: 0,
            run_wall_ns: 1_400_000,
            run_cpu_time_ns: Some(1_200_000),
            run_instructions: None,
            hot: None,
            unresolved_samples: 0,
        };
        assert_eq!(render_stop_status(&status), "run cost 1.2 ms cpu / 1.4 ms");
    }
}

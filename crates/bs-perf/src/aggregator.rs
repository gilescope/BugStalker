// SPDX-License-Identifier: MIT
//! In-memory aggregation for the Phase 6 overlay.
//!
//! The collector/parser gives us sampled PCs, the decoder maps those
//! PCs to source frames, and this module turns those frames into the
//! line counters the console, TUI, and DAP layers will render.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::decoder::{ResolvedPc, ResolvedPtTrace, SourceFrame};

/// Default number of per-stop summaries kept for history views.
pub const DEFAULT_STOP_HISTORY: usize = 32;

/// Count of samples attributed to a source line.
pub type SampleCount = u64;

/// Stable id for a source file during one perf session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl FileId {
    /// Numeric id value.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Key for a source line in the overlay maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceLine {
    /// Interned file id.
    pub file: FileId,
    /// One-based source line.
    pub line: u64,
}

/// One row in a stop summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotLine {
    /// Source line.
    pub source: SourceLine,
    /// Samples attributed to this line.
    pub samples: SampleCount,
}

/// Per-stop summary retained for `perf history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopSummary {
    /// Cycle count for the run-to-stop window. Step 117 callers
    /// may pass zero until the PMU counter wiring lands.
    pub run_cycles: u64,
    /// Wall-clock duration for the run-to-stop window.
    pub run_wall_ns: u64,
    /// Hottest lines in this stop, sorted by sample count
    /// descending and then by source key for deterministic output.
    pub top_lines: Vec<HotLine>,
    /// Samples that could not be mapped to source in this stop.
    pub unresolved_samples: SampleCount,
}

/// Summary of a decoded PT trace ingested into [`PerfData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtTraceAggregation {
    /// Total decoded instructions observed before source lookup.
    pub total_instructions: u64,
    /// Instructions attributed to source lines.
    pub resolved_instructions: u64,
    /// Instructions that did not map to source.
    pub unresolved_instructions: u64,
    /// Synchronization points consumed by the PT decoder.
    pub decode_sync_points: usize,
    /// Decode errors skipped by resynchronizing forward.
    pub decode_skipped_errors: usize,
    /// True when decode output was truncated at its configured bound.
    pub decode_truncated: bool,
}

/// Session-level perf data.
#[derive(Debug, Clone)]
pub struct PerfData {
    files: Vec<PathBuf>,
    file_ids: HashMap<PathBuf, FileId>,
    /// Cumulative samples since attach.
    pub cumulative: HashMap<SourceLine, SampleCount>,
    /// Samples attributed to the most recent run-to-stop window.
    pub last_run: HashMap<SourceLine, SampleCount>,
    /// Per-stop summaries.
    pub stops: VecDeque<StopSummary>,
    max_stops: usize,
    total_stops: u64,
    /// Cumulative unresolved samples.
    pub unresolved_cumulative: SampleCount,
    /// Unresolved samples in the most recent run-to-stop window.
    pub unresolved_last_run: SampleCount,
}

impl Default for PerfData {
    fn default() -> Self {
        Self::new(DEFAULT_STOP_HISTORY)
    }
}

impl PerfData {
    /// Create a new aggregation state with a bounded stop history.
    pub fn new(max_stops: usize) -> Self {
        Self {
            files: Vec::new(),
            file_ids: HashMap::new(),
            cumulative: HashMap::new(),
            last_run: HashMap::new(),
            stops: VecDeque::new(),
            max_stops,
            total_stops: 0,
            unresolved_cumulative: 0,
            unresolved_last_run: 0,
        }
    }

    /// Clear per-run counters. Call this when the debuggee resumes.
    pub fn begin_run(&mut self) {
        self.last_run.clear();
        self.unresolved_last_run = 0;
    }

    /// Record one resolved sampled PC. Primary and inlined frames
    /// both receive attribution. Duplicate `(file, line)` frames
    /// within the same resolved PC are counted once, avoiding
    /// double-counting when inline expansion later reports the same
    /// location as both primary and caller.
    pub fn record_resolved_pc(&mut self, resolved: &ResolvedPc) {
        let mut seen = Vec::<SourceLine>::new();
        for frame in std::iter::once(&resolved.primary).chain(resolved.inlined.iter()) {
            let line = self.intern_frame(frame);
            if seen.contains(&line) {
                continue;
            }
            seen.push(line);
            increment(&mut self.cumulative, line);
            increment(&mut self.last_run, line);
        }
    }

    /// Record one sampled PC that could not be mapped to source.
    pub fn record_unresolved_pc(&mut self) {
        self.record_unresolved_samples(1);
    }

    /// Record one or more sampled PCs that could not be mapped to
    /// source. Used for PMU loss accounting where the kernel reports
    /// a count but no individual program counters.
    pub fn record_unresolved_samples(&mut self, samples: SampleCount) {
        self.unresolved_cumulative = self.unresolved_cumulative.saturating_add(samples);
        self.unresolved_last_run = self.unresolved_last_run.saturating_add(samples);
    }

    /// Record a source-resolved Intel PT instruction window.
    pub fn record_resolved_pt_trace(&mut self, trace: &ResolvedPtTrace) -> PtTraceAggregation {
        for resolved in &trace.resolved {
            self.record_resolved_pc(resolved);
        }
        if trace.unresolved_instructions != 0 {
            self.record_unresolved_samples(trace.unresolved_instructions);
        }

        PtTraceAggregation {
            total_instructions: trace.total_instructions,
            resolved_instructions: trace.resolved.len() as u64,
            unresolved_instructions: trace.unresolved_instructions,
            decode_sync_points: trace.decode_sync_points,
            decode_skipped_errors: trace.decode_skipped_errors,
            decode_truncated: trace.decode_truncated,
        }
    }

    /// Finish a run-to-stop window and retain a summary.
    pub fn finish_stop(&mut self, run_cycles: u64, run_wall_ns: u64) -> StopSummary {
        let mut top_lines = self
            .last_run
            .iter()
            .map(|(&source, &samples)| HotLine { source, samples })
            .collect::<Vec<_>>();
        top_lines.sort_by(|a, b| {
            b.samples
                .cmp(&a.samples)
                .then_with(|| a.source.cmp(&b.source))
        });

        let summary = StopSummary {
            run_cycles,
            run_wall_ns,
            top_lines,
            unresolved_samples: self.unresolved_last_run,
        };
        self.total_stops = self.total_stops.saturating_add(1);
        self.stops.push_back(summary.clone());
        while self.stops.len() > self.max_stops {
            self.stops.pop_front();
        }
        summary
    }

    /// Return the interned path for a file id.
    pub fn file_path(&self, id: FileId) -> Option<&Path> {
        self.files.get(id.0 as usize).map(PathBuf::as_path)
    }

    /// Number of interned source files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Total run-to-stop summaries recorded since attach. This is
    /// monotonic even when the bounded `stops` history evicts old
    /// summaries.
    pub fn total_stops(&self) -> u64 {
        self.total_stops
    }

    fn intern_frame(&mut self, frame: &SourceFrame) -> SourceLine {
        let id = if let Some(&id) = self.file_ids.get(&frame.file) {
            id
        } else {
            let id = FileId(self.files.len() as u32);
            self.files.push(frame.file.clone());
            self.file_ids.insert(frame.file.clone(), id);
            id
        };
        SourceLine {
            file: id,
            line: frame.line,
        }
    }
}

fn increment(map: &mut HashMap<SourceLine, SampleCount>, key: SourceLine) {
    let count = map.entry(key).or_insert(0);
    *count = count.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(path: &str, line: u64) -> SourceFrame {
        SourceFrame {
            file: PathBuf::from(path),
            line,
            column: 0,
        }
    }

    fn resolved(primary: SourceFrame, inlined: Vec<SourceFrame>) -> ResolvedPc {
        ResolvedPc {
            object_pc: 0x10,
            primary,
            inlined,
            is_stmt: true,
            prologue_end: false,
            epilogue_begin: false,
        }
    }

    fn resolved_pt_trace() -> ResolvedPtTrace {
        ResolvedPtTrace {
            resolved: vec![
                resolved(frame("src/main.rs", 10), vec![]),
                resolved(frame("src/main.rs", 12), vec![frame("src/inlined.rs", 30)]),
            ],
            unresolved_instructions: 3,
            total_instructions: 5,
            decode_sync_points: 2,
            decode_skipped_errors: 1,
            decode_truncated: true,
        }
    }

    #[test]
    fn records_primary_and_inlined_frames() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved(
            frame("src/main.rs", 10),
            vec![frame("src/lib.rs", 20)],
        ));

        assert_eq!(data.file_count(), 2);
        let main = SourceLine {
            file: FileId(0),
            line: 10,
        };
        let lib = SourceLine {
            file: FileId(1),
            line: 20,
        };
        assert_eq!(data.cumulative.get(&main), Some(&1));
        assert_eq!(data.cumulative.get(&lib), Some(&1));
        assert_eq!(data.last_run.get(&main), Some(&1));
        assert_eq!(data.file_path(FileId(0)), Some(Path::new("src/main.rs")));
    }

    #[test]
    fn deduplicates_same_line_within_one_pc() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved(
            frame("src/main.rs", 10),
            vec![frame("src/main.rs", 10)],
        ));
        assert_eq!(data.cumulative.values().copied().sum::<u64>(), 1);
    }

    #[test]
    fn begin_run_clears_only_last_run() {
        let mut data = PerfData::default();
        data.record_resolved_pc(&resolved(frame("src/main.rs", 10), vec![]));
        data.record_unresolved_samples(3);

        data.begin_run();

        assert_eq!(data.cumulative.values().copied().sum::<u64>(), 1);
        assert_eq!(data.unresolved_cumulative, 3);
        assert!(data.last_run.is_empty());
        assert_eq!(data.unresolved_last_run, 0);
    }

    #[test]
    fn records_resolved_pt_trace_and_returns_stats() {
        let mut data = PerfData::default();
        let trace = resolved_pt_trace();

        let stats = data.record_resolved_pt_trace(&trace);

        assert_eq!(
            stats,
            PtTraceAggregation {
                total_instructions: 5,
                resolved_instructions: 2,
                unresolved_instructions: 3,
                decode_sync_points: 2,
                decode_skipped_errors: 1,
                decode_truncated: true,
            }
        );
        assert_eq!(data.cumulative.values().copied().sum::<u64>(), 3);
        assert_eq!(data.last_run.values().copied().sum::<u64>(), 3);
        assert_eq!(data.unresolved_cumulative, 3);
        assert_eq!(data.unresolved_last_run, 3);
        assert_eq!(data.file_count(), 2);
    }

    #[test]
    fn finish_stop_sorts_and_bounds_history() {
        let mut data = PerfData::new(2);
        data.record_resolved_pc(&resolved(frame("b.rs", 2), vec![]));
        data.record_resolved_pc(&resolved(frame("a.rs", 1), vec![]));
        data.record_resolved_pc(&resolved(frame("a.rs", 1), vec![]));

        let first = data.finish_stop(10, 20);
        assert_eq!(first.top_lines[0].samples, 2);

        data.begin_run();
        data.record_unresolved_pc();
        data.finish_stop(11, 21);
        data.begin_run();
        data.finish_stop(12, 22);

        assert_eq!(data.stops.len(), 2);
        assert_eq!(data.total_stops(), 3);
        assert_eq!(data.stops[0].run_cycles, 11);
        assert_eq!(data.stops[0].unresolved_samples, 1);
        assert_eq!(data.stops[1].run_cycles, 12);
    }
}

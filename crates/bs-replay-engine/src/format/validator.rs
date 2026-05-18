// SPDX-License-Identifier: MIT
//! Trace validator — the support-diagnostic doctor.
//!
//! `validate(dir)` walks a trace top-to-bottom and emits a
//! [`ValidationReport`] of everything it found. The reader bails
//! on the first invariant failure (correct for hot-path use); the
//! validator deliberately keeps going so a human triaging a broken
//! trace gets the full picture in one pass.
//!
//! Severity ladder:
//!
//! - [`Severity::Error`] — replay against this trace will fail.
//! - [`Severity::Warning`] — replay can probably continue but
//!   something is unusual (e.g. a segment-index gap that may be
//!   benign rotation skip but more often is a missing file).
//! - [`Severity::Info`] — purely advisory totals (segment count,
//!   event count) handy for support tickets.

use core::fmt;
use std::fs;
use std::path::Path;

use super::manifest::{Manifest, ManifestParseError};
use super::segment::{MANIFEST_FILENAME, parse_segment_filename};
use super::trace_reader::{TraceReadError, TraceReader};

/// Severity level of a validator finding.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Severity {
    /// Replay against this trace will fail.
    Error,
    /// Trace is unusual but probably replayable.
    Warning,
    /// Advisory total or summary.
    Info,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        })
    }
}

/// Symbolic code for a validator finding.
///
/// Codes are stable so support workflows can grep on them.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum DiagKind {
    /// Trace dir doesn't have a `manifest.txt`.
    ManifestMissing,
    /// `manifest.txt` couldn't be read (permissions, IO).
    ManifestUnreadable,
    /// `manifest.txt` failed to parse.
    ManifestMalformed,
    /// Manifest declared a version this build can't read.
    UnsupportedVersion,
    /// No event-NNNNNN.lz4 files found.
    NoSegments,
    /// Hole in the segment-index sequence.
    SegmentGap,
    /// Segment file couldn't be read.
    SegmentUnreadable,
    /// Segment archive failed rkyv access (corruption).
    SegmentCorrupt,
    /// Segment header index disagrees with filename index.
    SegmentHeaderIndexMismatch,
    /// Segment header event_count disagrees with stored events.
    SegmentEventCountMismatch,
    /// Total segment count.
    TotalSegments,
    /// Total event count across all segments.
    TotalEvents,
    /// Total number of checkpoint snapshot files.
    TotalCheckpoints,
    /// Checkpoint file couldn't be read or decoded.
    CheckpointUnreadable,
    /// Checkpoint header index disagrees with filename index.
    CheckpointHeaderIndexMismatch,
    /// Recorded build-id does not match the host-supplied one.
    BuildIdMismatch,
    /// Host CPU lacks features the recording used.
    HostFeatureMissing,
}

impl DiagKind {
    /// Stable, lowercase string code (used in the Display impl and
    /// scriptable for grep). Format: `category-detail`.
    pub fn code(self) -> &'static str {
        match self {
            Self::ManifestMissing => "manifest-missing",
            Self::ManifestUnreadable => "manifest-unreadable",
            Self::ManifestMalformed => "manifest-malformed",
            Self::UnsupportedVersion => "unsupported-version",
            Self::NoSegments => "no-segments",
            Self::SegmentGap => "segment-gap",
            Self::SegmentUnreadable => "segment-unreadable",
            Self::SegmentCorrupt => "segment-corrupt",
            Self::SegmentHeaderIndexMismatch => "segment-header-index-mismatch",
            Self::SegmentEventCountMismatch => "segment-event-count-mismatch",
            Self::TotalSegments => "total-segments",
            Self::TotalEvents => "total-events",
            Self::TotalCheckpoints => "total-checkpoints",
            Self::CheckpointUnreadable => "checkpoint-unreadable",
            Self::CheckpointHeaderIndexMismatch => "checkpoint-header-index-mismatch",
            Self::BuildIdMismatch => "build-id-mismatch",
            Self::HostFeatureMissing => "host-feature-missing",
        }
    }
}

/// One validator finding.
#[derive(Debug, Clone)]
pub struct Diag {
    /// Severity bucket.
    pub severity: Severity,
    /// Symbolic code.
    pub kind: DiagKind,
    /// Human-readable detail.
    pub message: String,
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: [{}] {}",
            self.severity,
            self.kind.code(),
            self.message
        )
    }
}

/// Result of [`validate`]. Always returned (never an `Err`); inspect
/// `errors` / `warnings` / `info` for findings.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    /// Errors that will prevent replay.
    pub errors: Vec<Diag>,
    /// Warnings that may surprise but probably allow replay.
    pub warnings: Vec<Diag>,
    /// Advisory totals.
    pub info: Vec<Diag>,
}

impl ValidationReport {
    /// True iff there are no errors. Warnings do not flip this.
    pub fn is_replayable(&self) -> bool {
        self.errors.is_empty()
    }

    /// All findings, in severity order: errors first, then warnings,
    /// then info.
    pub fn all(&self) -> impl Iterator<Item = &Diag> {
        self.errors.iter().chain(&self.warnings).chain(&self.info)
    }

    fn push(&mut self, severity: Severity, kind: DiagKind, message: impl Into<String>) {
        let diag = Diag {
            severity,
            kind,
            message: message.into(),
        };
        match severity {
            Severity::Error => self.errors.push(diag),
            Severity::Warning => self.warnings.push(diag),
            Severity::Info => self.info.push(diag),
        }
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_replayable() && self.warnings.is_empty() {
            writeln!(
                f,
                "trace OK ({} segments, {} events)",
                self.info
                    .iter()
                    .find(|d| d.kind == DiagKind::TotalSegments)
                    .map(|d| d.message.as_str())
                    .unwrap_or("?"),
                self.info
                    .iter()
                    .find(|d| d.kind == DiagKind::TotalEvents)
                    .map(|d| d.message.as_str())
                    .unwrap_or("?"),
            )?;
            return Ok(());
        }
        for d in self.all() {
            writeln!(f, "{d}")?;
        }
        Ok(())
    }
}

/// Optional inputs that let the validator check replay-time host
/// invariants the trace alone can't speak to: build-id of the
/// binary the caller is about to replay against, and the CPU
/// feature list of the replay host.
#[derive(Debug, Default, Clone)]
pub struct ValidationOptions<'a> {
    /// Host-supplied build-id of the binary that will be replayed.
    /// If `Some`, the validator compares it against the manifest's
    /// `build_id` field; mismatch → [`DiagKind::BuildIdMismatch`].
    pub expected_build_id: Option<&'a str>,
    /// Host-supplied CPU feature list. If `Some`, the validator
    /// checks the recording's required features are a subset;
    /// every gap → [`DiagKind::HostFeatureMissing`].
    pub host_features: Option<&'a [&'a str]>,
}

/// Walk the trace at `dir` and report every consistency finding.
///
/// Equivalent to [`validate_with`] with default options (no host
/// checks). Always returns a [`ValidationReport`] — even when
/// nothing of the trace is readable. If you want a one-line "is
/// it OK?" answer, call [`ValidationReport::is_replayable`].
pub fn validate(dir: impl AsRef<Path>) -> ValidationReport {
    validate_with(dir, &ValidationOptions::default())
}

/// Like [`validate`], but also runs replay-time host checks
/// supplied through [`ValidationOptions`]. Use this from a doctor
/// CLI / integration test where the caller knows the expected
/// build-id and the host's feature list.
pub fn validate_with(dir: impl AsRef<Path>, opts: &ValidationOptions<'_>) -> ValidationReport {
    let dir = dir.as_ref();
    let mut report = ValidationReport::default();

    // ---- Manifest ----
    let manifest_path = dir.join(MANIFEST_FILENAME);
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(text) => match Manifest::from_text(&text) {
            Ok(m) => {
                if !m.format_version.is_supported() {
                    report.push(
                        Severity::Error,
                        DiagKind::UnsupportedVersion,
                        format!(
                            "manifest declares format v{} but this build supports up to v{}",
                            m.format_version.0,
                            super::version::MAX_SUPPORTED_FORMAT_VERSION.0,
                        ),
                    );
                }
                Some(m)
            }
            Err(ManifestParseError::Missing(key)) => {
                report.push(
                    Severity::Error,
                    DiagKind::ManifestMalformed,
                    format!("manifest missing required key `{key}`"),
                );
                None
            }
            Err(ManifestParseError::Malformed { line, detail }) => {
                report.push(
                    Severity::Error,
                    DiagKind::ManifestMalformed,
                    format!("manifest line {line}: {detail}"),
                );
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                Severity::Error,
                DiagKind::ManifestMissing,
                format!(
                    "expected `{}` at {}",
                    MANIFEST_FILENAME,
                    manifest_path.display()
                ),
            );
            None
        }
        Err(e) => {
            report.push(
                Severity::Error,
                DiagKind::ManifestUnreadable,
                format!("{}: {e}", manifest_path.display()),
            );
            None
        }
    };

    // ---- Replay-time host checks (only when the manifest parsed) ----
    if let Some(m) = manifest.as_ref() {
        if let Some(expected) = opts.expected_build_id
            && m.build_id != expected
        {
            report.push(
                Severity::Error,
                DiagKind::BuildIdMismatch,
                format!(
                    "manifest build_id {recorded} disagrees with expected {expected}",
                    recorded = m.build_id,
                ),
            );
        }
        if let Some(host) = opts.host_features {
            let missing = m.missing_host_features(host);
            for feat in missing {
                report.push(
                    Severity::Error,
                    DiagKind::HostFeatureMissing,
                    format!("host lacks CPU feature `{feat}` used by recording"),
                );
            }
        }
    }

    // ---- Segment enumeration ----
    let segments = match enumerate_segments(dir) {
        Ok(v) => v,
        Err(e) => {
            // Manifest-missing has already been reported; if the dir
            // itself is unreadable that's covered too. Surface the
            // listing failure as an additional finding.
            report.push(
                Severity::Error,
                DiagKind::SegmentUnreadable,
                format!("listing trace dir: {e}"),
            );
            Vec::new()
        }
    };

    if segments.is_empty() {
        report.push(
            Severity::Warning,
            DiagKind::NoSegments,
            "trace contains no event segments".to_owned(),
        );
    } else {
        // Strict-monotonic with gap detection.
        for w in segments.windows(2) {
            if w[1] > w[0] + 1 {
                report.push(
                    Severity::Warning,
                    DiagKind::SegmentGap,
                    format!("missing segment indices {}–{}", w[0] + 1, w[1] - 1),
                );
            }
        }
    }

    // ---- Per-segment + per-checkpoint validation ----
    // Re-open via TraceReader to reuse its parsing path. If the
    // manifest failed entirely we skip the per-file work (no reader
    // can be constructed) — those errors are already on the report.
    // Run even when there are zero segments: a trace might contain
    // only checkpoints (rare but legal until the recorder gets
    // wired up).
    if manifest.is_some() {
        match TraceReader::open(dir) {
            Ok(reader) => {
                report.push(
                    Severity::Info,
                    DiagKind::TotalCheckpoints,
                    reader.checkpoint_indices().len().to_string(),
                );
                for &idx in reader.checkpoint_indices() {
                    if let Err(e) = reader.open_checkpoint(idx) {
                        match e {
                            TraceReadError::CheckpointHeaderMismatch {
                                file_index,
                                header_index,
                            } => report.push(
                                Severity::Error,
                                DiagKind::CheckpointHeaderIndexMismatch,
                                format!(
                                    "checkpoint file {file_index} carries header index {header_index}",
                                ),
                            ),
                            other => report.push(
                                Severity::Error,
                                DiagKind::CheckpointUnreadable,
                                format!("checkpoint {idx}: {other}"),
                            ),
                        }
                    }
                }
                let mut total_events: u64 = 0;
                for &idx in reader.segment_indices() {
                    match reader.open_segment(idx) {
                        Ok(seg) => match seg.events() {
                            Ok(evs) => total_events += evs.len() as u64,
                            Err(e) => report.push(
                                Severity::Error,
                                DiagKind::SegmentCorrupt,
                                format!("segment {idx}: {e}"),
                            ),
                        },
                        Err(TraceReadError::HeaderMismatch {
                            file_index,
                            header_index,
                        }) => {
                            report.push(
                                Severity::Error,
                                DiagKind::SegmentHeaderIndexMismatch,
                                format!(
                                    "segment file {file_index} carries header index {header_index}",
                                ),
                            );
                        }
                        Err(TraceReadError::EventCountMismatch {
                            segment_index,
                            header_count,
                            actual_count,
                        }) => {
                            report.push(
                                Severity::Error,
                                DiagKind::SegmentEventCountMismatch,
                                format!(
                                    "segment {segment_index}: header says {header_count} events, found {actual_count}",
                                ),
                            );
                        }
                        Err(TraceReadError::Archive(e)) => {
                            report.push(
                                Severity::Error,
                                DiagKind::SegmentCorrupt,
                                format!("segment {idx}: {e}"),
                            );
                        }
                        Err(e) => {
                            report.push(
                                Severity::Error,
                                DiagKind::SegmentUnreadable,
                                format!("segment {idx}: {e}"),
                            );
                        }
                    }
                }
                report.push(
                    Severity::Info,
                    DiagKind::TotalSegments,
                    reader.segment_indices().len().to_string(),
                );
                report.push(
                    Severity::Info,
                    DiagKind::TotalEvents,
                    total_events.to_string(),
                );
            }
            Err(e) => {
                report.push(
                    Severity::Error,
                    DiagKind::SegmentUnreadable,
                    format!("could not open trace: {e}"),
                );
            }
        }
    }

    report
}

fn enumerate_segments(dir: &Path) -> std::io::Result<Vec<u64>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && let Some(idx) = parse_segment_filename(name)
        {
            out.push(idx);
        }
    }
    out.sort_unstable();
    Ok(out)
}

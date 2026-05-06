// SPDX-License-Identifier: MIT
//! Trace manifest — first file written, last validated.
//!
//! Carries the build-id of the recorded binary (cross-checked at
//! replay), kernel version, CPU feature set, recording engine
//! version, and the initial environment. See
//! `doc/plans/phase-5-time-travel.md` § "3A. Trace format and
//! storage".
//!
//! ## Format
//!
//! UTF-8 text. One key per line. `# …` is a comment, blank lines
//! ignored. Repeated keys append. Values run to end-of-line and
//! are not quoted — the only escape is `\n` for embedded newlines.
//!
//! ```text
//! # bs-replay manifest v1
//! format_version: 1
//! build_id: deadbeef0123…
//! kernel_release: 6.6.42
//! cpu_feature: sse2
//! cpu_feature: sse4_2
//! engine_version: 0.0.1
//! initial_cwd: /home/giles
//! initial_arg: --flag
//! env: PATH=/usr/bin
//! env: LANG=en_GB
//! ```
//!
//! Why hand-rolled? The manifest is a support diagnostic — humans
//! read it. The body of the trace uses rkyv for the speed; for
//! 10 fields written once we don't pull in a serialization
//! framework just to learn its quirks.
//!
//! ## env-key contract
//!
//! Environment-variable *keys* must not contain `=` — the `env: `
//! line uses the first `=` as the K/V separator and there is no
//! escape. This matches POSIX, which forbids `=` in env names
//! (`putenv`/`setenv` would themselves reject it). The writer
//! debug-asserts the contract; release builds silently produce a
//! malformed manifest if violated.

use core::fmt;
use core::fmt::Write as _;

use super::version::FormatVersion;

/// Top-level manifest written as `manifest.txt` at the trace root.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Manifest {
    /// On-disk format version. Replay rejects unsupported versions
    /// before reading any segment.
    pub format_version: FormatVersion,
    /// SHA-256 build-id of the recorded binary, hex-encoded.
    /// Cross-checked at replay; mismatch fails fast.
    pub build_id: String,
    /// `uname -r` at record time, e.g. "6.6.42-generic".
    pub kernel_release: String,
    /// CPU feature flags. Replay host must be a *superset*.
    pub cpu_features: Vec<String>,
    /// Recording engine version (`CARGO_PKG_VERSION` of this crate).
    pub engine_version: String,
    /// Environment variables snapshotted at record start, "K=V".
    pub initial_env: Vec<(String, String)>,
    /// Working directory at record start.
    pub initial_cwd: String,
    /// Process arguments at record start (`argv` minus argv[0]).
    pub initial_args: Vec<String>,
    /// Wall-clock instant the recording started, conventionally
    /// ISO-8601 / RFC 3339 (e.g. `2026-05-06T18:42:00Z`). The
    /// format crate does not validate the encoding — callers
    /// pick what they want and stay consistent. `None` for
    /// traces written before this field existed; new writers
    /// should populate it.
    pub recorded_at: Option<String>,
}

impl Manifest {
    /// Return the features this manifest names that `host_features`
    /// does *not* contain. Empty `Vec` means the host can replay
    /// the trace as far as CPU features are concerned.
    ///
    /// Plan §Invariants: "CPU feature subset: replay host must
    /// support the recording's features." A non-empty result
    /// means the host is missing capabilities the recorder used —
    /// replay would either trap on an unknown instruction or
    /// silently misexecute.
    ///
    /// String comparison is case-sensitive — match what
    /// `cpu_features` was populated with at record time. (Real
    /// recorders should source both ends from the same enumerator
    /// to avoid skew, e.g. `/proc/cpuinfo` flags or `cpuid` leafs
    /// canonicalised to one casing.)
    pub fn missing_host_features<S: AsRef<str>>(&self, host_features: &[S]) -> Vec<String> {
        self.cpu_features
            .iter()
            .filter(|f| !host_features.iter().any(|h| h.as_ref() == f.as_str()))
            .cloned()
            .collect()
    }

    /// Convenience — true iff [`Self::missing_host_features`] is empty.
    pub fn is_replayable_on<S: AsRef<str>>(&self, host_features: &[S]) -> bool {
        self.missing_host_features(host_features).is_empty()
    }

    /// Render the manifest to its on-disk text form.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("# bs-replay manifest v1\n");
        writeln!(out, "format_version: {}", self.format_version.0).unwrap();
        writeln!(out, "build_id: {}", escape(&self.build_id)).unwrap();
        writeln!(out, "kernel_release: {}", escape(&self.kernel_release)).unwrap();
        writeln!(out, "engine_version: {}", escape(&self.engine_version)).unwrap();
        writeln!(out, "initial_cwd: {}", escape(&self.initial_cwd)).unwrap();
        for f in &self.cpu_features {
            writeln!(out, "cpu_feature: {}", escape(f)).unwrap();
        }
        for a in &self.initial_args {
            writeln!(out, "initial_arg: {}", escape(a)).unwrap();
        }
        for (k, v) in &self.initial_env {
            // env-key contract: POSIX forbids `=` in env names and
            // the manifest format depends on that to split K/V. A
            // future format version could escape `=` if a use case
            // ever justifies it; for now we just enforce.
            debug_assert!(
                !k.contains('='),
                "env key {k:?} contains `=` — manifest cannot round-trip it",
            );
            writeln!(out, "env: {}={}", escape(k), escape(v)).unwrap();
        }
        if let Some(ts) = &self.recorded_at {
            writeln!(out, "recorded_at: {}", escape(ts)).unwrap();
        }
        out
    }

    /// Parse a manifest from its on-disk text form.
    ///
    /// Errors carry the 1-based line number plus what was expected
    /// vs. found — same kindness rustc shows.
    pub fn from_text(input: &str) -> Result<Self, ManifestParseError> {
        let mut format_version: Option<FormatVersion> = None;
        let mut build_id: Option<String> = None;
        let mut kernel_release: Option<String> = None;
        let mut recorded_at: Option<String> = None;
        let mut engine_version: Option<String> = None;
        let mut initial_cwd: Option<String> = None;
        let mut cpu_features: Vec<String> = Vec::new();
        let mut initial_args: Vec<String> = Vec::new();
        let mut initial_env: Vec<(String, String)> = Vec::new();

        for (idx, raw) in input.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once(':').ok_or_else(|| {
                ManifestParseError::malformed(line_no, "expected `key: value`")
            })?;
            let key = key.trim();
            // Strip exactly the one separator space the writer emits.
            // `trim()` would discard intentional leading whitespace
            // a caller put inside a value; we want to round-trip
            // every valid String, so consume one space at most.
            let raw_value = value.strip_prefix(' ').unwrap_or(value);
            let value = unescape(raw_value);

            match key {
                "format_version" => {
                    let n: u32 = value.parse().map_err(|_| {
                        ManifestParseError::malformed(
                            line_no,
                            "format_version must be a u32",
                        )
                    })?;
                    format_version = Some(FormatVersion(n));
                }
                "build_id" => build_id = Some(value),
                "kernel_release" => kernel_release = Some(value),
                "engine_version" => engine_version = Some(value),
                "initial_cwd" => initial_cwd = Some(value),
                "cpu_feature" => cpu_features.push(value),
                "initial_arg" => initial_args.push(value),
                "env" => {
                    let (k, v) = value.split_once('=').ok_or_else(|| {
                        ManifestParseError::malformed(
                            line_no,
                            "env value must be `KEY=VALUE`",
                        )
                    })?;
                    initial_env.push((k.to_owned(), v.to_owned()));
                }
                "recorded_at" => recorded_at = Some(value),
                other => {
                    return Err(ManifestParseError::malformed(
                        line_no,
                        format!("unknown key `{other}`"),
                    ));
                }
            }
        }

        Ok(Self {
            format_version: format_version
                .ok_or_else(|| ManifestParseError::missing("format_version"))?,
            build_id: build_id
                .ok_or_else(|| ManifestParseError::missing("build_id"))?,
            kernel_release: kernel_release
                .ok_or_else(|| ManifestParseError::missing("kernel_release"))?,
            cpu_features,
            engine_version: engine_version
                .ok_or_else(|| ManifestParseError::missing("engine_version"))?,
            initial_env,
            initial_cwd: initial_cwd
                .ok_or_else(|| ManifestParseError::missing("initial_cwd"))?,
            initial_args,
            recorded_at,
        })
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Manifest parse failure with a precise location.
#[derive(thiserror::Error, Debug, Eq, PartialEq)]
pub enum ManifestParseError {
    /// The named key is required but did not appear.
    #[error("manifest missing required key `{0}`")]
    Missing(&'static str),
    /// A line was syntactically malformed.
    #[error("manifest line {line}: {detail}")]
    Malformed {
        /// 1-based line number.
        line: usize,
        /// Human-readable explanation.
        detail: String,
    },
}

impl ManifestParseError {
    fn missing(key: &'static str) -> Self {
        Self::Missing(key)
    }
    fn malformed(line: usize, detail: impl Into<String>) -> Self {
        Self::Malformed { line, detail: detail.into() }
    }
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format_version: FormatVersion::V1,
            build_id: "deadbeef".repeat(8),
            kernel_release: "6.6.42-test".to_owned(),
            cpu_features: vec!["sse2".into(), "sse4_2".into()],
            engine_version: "0.0.1".to_owned(),
            initial_env: vec![
                ("PATH".into(), "/usr/bin".into()),
                ("LANG".into(), "en_GB".into()),
            ],
            initial_cwd: "/home/giles".to_owned(),
            initial_args: vec!["--flag".into(), "--also".into()],
            recorded_at: None,
        }
    }

    #[test]
    fn recorded_at_round_trips_when_present() {
        let mut m = sample();
        m.recorded_at = Some("2026-05-06T18:42:00Z".to_owned());
        let s = m.to_text();
        let back = Manifest::from_text(&s).unwrap();
        assert_eq!(back.recorded_at, Some("2026-05-06T18:42:00Z".to_owned()));
        assert_eq!(m, back);
    }

    #[test]
    fn recorded_at_absent_means_old_v1_trace() {
        // A manifest text written before recorded_at existed must
        // parse cleanly with recorded_at = None — the additive-
        // optional contract.
        let s = "format_version: 1\n\
                 build_id: ab\n\
                 kernel_release: r\n\
                 engine_version: e\n\
                 initial_cwd: /\n";
        let m = Manifest::from_text(s).unwrap();
        assert_eq!(m.recorded_at, None);
    }

    #[test]
    fn recorded_at_emitted_only_when_some() {
        let m = sample(); // recorded_at = None
        let s = m.to_text();
        assert!(!s.contains("recorded_at"), "should not emit empty recorded_at line");
    }

    #[test]
    fn missing_host_features_empty_when_host_is_superset() {
        let m = sample();
        let host = ["sse2", "sse4_2", "avx", "avx2"];
        assert!(m.missing_host_features(&host).is_empty());
        assert!(m.is_replayable_on(&host));
    }

    #[test]
    fn missing_host_features_lists_gaps() {
        let m = sample(); // wants sse2, sse4_2
        let host = ["sse2"];
        let missing = m.missing_host_features(&host);
        assert_eq!(missing, vec!["sse4_2"]);
        assert!(!m.is_replayable_on(&host));
    }

    #[test]
    fn missing_host_features_handles_empty_recording() {
        let mut m = sample();
        m.cpu_features.clear();
        // A trace that named no required features is replayable
        // anywhere — even on a host with zero advertised features.
        let host: [&str; 0] = [];
        assert!(m.is_replayable_on(&host));
        assert!(m.missing_host_features(&host).is_empty());
    }

    #[test]
    fn missing_host_features_is_case_sensitive() {
        let m = sample(); // wants "sse2"
        let host = ["SSE2"]; // wrong case
        let missing = m.missing_host_features(&host);
        assert!(missing.contains(&"sse2".to_owned()));
    }

    #[test]
    fn text_roundtrip_preserves_all_fields() {
        let m = sample();
        let s = m.to_text();
        let back = Manifest::from_text(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let s = "# header\n\n\
                 format_version: 1\n\
                 # another comment\n\
                 build_id: ab\n\
                 kernel_release: r\n\
                 engine_version: e\n\
                 initial_cwd: /\n";
        let m = Manifest::from_text(s).unwrap();
        assert_eq!(m.format_version, FormatVersion::V1);
        assert_eq!(m.build_id, "ab");
    }

    #[test]
    fn missing_required_key_reported() {
        let s = "format_version: 1\nbuild_id: ab\nkernel_release: r\n";
        let err = Manifest::from_text(s).unwrap_err();
        assert!(matches!(err, ManifestParseError::Missing("engine_version")));
    }

    #[test]
    fn malformed_line_reports_line_number() {
        let s = "format_version: 1\nbuild_id ab\nkernel_release: r\n";
        let err = Manifest::from_text(s).unwrap_err();
        assert!(
            matches!(err, ManifestParseError::Malformed { line: 2, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_key_reports_name() {
        let s = "format_version: 1\nbuild_id: ab\nfoobar: x\n";
        let err = Manifest::from_text(s).unwrap_err();
        match err {
            ManifestParseError::Malformed { line: 3, detail } => {
                assert!(detail.contains("foobar"), "{detail}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn embedded_newline_survives_roundtrip() {
        let mut m = sample();
        m.build_id = "with\nnewline".into();
        let s = m.to_text();
        // No bare newline inside the build_id line.
        assert_eq!(s.matches("\nbuild_id:").count(), 1);
        let back = Manifest::from_text(&s).unwrap();
        assert_eq!(back.build_id, "with\nnewline");
    }
}

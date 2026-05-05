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
}

impl Manifest {
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
            writeln!(out, "env: {}={}", escape(k), escape(v)).unwrap();
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
            let value = unescape(value.trim());

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
        }
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

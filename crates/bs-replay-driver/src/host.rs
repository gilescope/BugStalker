// SPDX-License-Identifier: MIT
//! Host-OS CPU-feature enumerator.
//!
//! Linux: parses `/proc/cpuinfo`. The format is a per-cpu block of
//! `key : value` lines; we look for the first `flags:` line on
//! x86 / x86-64 or `Features:` on aarch64 — kernel-canonical feature
//! lists for the host CPU. Each token is one feature name (e.g.
//! `sse2`, `neon`, `aes`).
//!
//! Other OSes return [`HostDetectError::Unsupported`]; the
//! Darwin path (selected `sysctl hw.optional.*` entries via
//! `sysctlbyname`) lands in a later iteration when there's a real
//! macOS consumer asking for it. Callers that already know their
//! host features can side-step this and supply them directly to
//! [`crate::TraceReplayer::check_host_compatibility`].

#[cfg(target_os = "linux")]
use std::fs;

/// Best-effort enumeration of the running host's CPU feature
/// flags, sorted, deduplicated. Empty result is unusual but legal
/// (the host's `/proc/cpuinfo` may simply have no `flags:` line).
pub fn host_features() -> Result<Vec<String>, HostDetectError> {
    detect()
}

#[cfg(target_os = "linux")]
fn detect() -> Result<Vec<String>, HostDetectError> {
    let text = fs::read_to_string("/proc/cpuinfo")
        .map_err(|e| HostDetectError::Read(format!("/proc/cpuinfo: {e}")))?;
    Ok(parse_cpuinfo(&text))
}

#[cfg(not(target_os = "linux"))]
fn detect() -> Result<Vec<String>, HostDetectError> {
    Err(HostDetectError::Unsupported(format!(
        "host_features() not implemented on {os}; \
         supply features directly to check_host_compatibility \
         (e.g. via sysctl hw.optional.* on Darwin)",
        os = std::env::consts::OS,
    )))
}

#[cfg(any(target_os = "linux", test))]
fn parse_cpuinfo(text: &str) -> Vec<String> {
    // The first matching line is enough — every CPU block carries
    // the same flags. `flags:` for x86, `Features:` for aarch64.
    // Some kernels also use lowercase `features` so accept both.
    let line = text.lines().find_map(|raw| {
        let line = raw.trim_start();
        for prefix in ["flags", "Features", "features"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let rest = rest.trim_start();
                if let Some(value) = rest.strip_prefix(':') {
                    return Some(value.trim());
                }
            }
        }
        None
    });
    let mut feats: Vec<String> = match line {
        Some(s) => s
            .split_ascii_whitespace()
            .map(|t| t.to_owned())
            .collect(),
        None => Vec::new(),
    };
    feats.sort();
    feats.dedup();
    feats
}

/// Failure modes for [`host_features`].
#[derive(thiserror::Error, Debug)]
pub enum HostDetectError {
    /// The host source (e.g. `/proc/cpuinfo`) couldn't be read.
    #[error("could not read host CPU info: {0}")]
    Read(String),
    /// Detection isn't implemented on this host OS.
    #[error("host CPU-feature detection unsupported: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpuinfo_x86_flags_line() {
        let sample = "\
processor   : 0
vendor_id   : GenuineIntel
flags       : fpu vme de pse tsc sse sse2 sse4_1 sse4_2 avx
bogomips    : 4800
";
        let feats = parse_cpuinfo(sample);
        // Sorted + deduplicated — sse2 stays, sse appears once.
        assert!(feats.contains(&"sse2".to_owned()));
        assert!(feats.contains(&"avx".to_owned()));
        assert!(feats.contains(&"fpu".to_owned()));
        // Sorted check.
        let mut copy = feats.clone();
        copy.sort();
        assert_eq!(feats, copy);
    }

    #[test]
    fn parse_cpuinfo_aarch64_features_line() {
        let sample = "\
processor   : 0
Features    : fp asimd evtstrm aes pmull sha1 sha2 crc32
CPU implementer : 0x41
";
        let feats = parse_cpuinfo(sample);
        assert!(feats.contains(&"asimd".to_owned()));
        assert!(feats.contains(&"crc32".to_owned()));
        assert!(feats.contains(&"aes".to_owned()));
    }

    #[test]
    fn parse_cpuinfo_returns_empty_when_no_flags_line() {
        let sample = "\
processor   : 0
vendor_id   : GenuineIntel
bogomips    : 4800
";
        let feats = parse_cpuinfo(sample);
        assert!(feats.is_empty());
    }

    #[test]
    fn parse_cpuinfo_dedups_repeated_flags() {
        let sample = "flags : sse2 sse2 avx avx avx2\n";
        let feats = parse_cpuinfo(sample);
        assert_eq!(feats, vec!["avx".to_owned(), "avx2".to_owned(), "sse2".to_owned()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_features_on_linux_returns_non_empty() {
        // The CI Linux box should have *some* feature flags. If
        // /proc/cpuinfo were missing we'd error rather than empty;
        // this asserts the happy path.
        let feats = host_features().unwrap();
        assert!(!feats.is_empty(), "expected at least one CPU feature");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn host_features_on_non_linux_reports_unsupported() {
        let err = host_features().unwrap_err();
        match err {
            HostDetectError::Unsupported(msg) => {
                // Message names the OS so the caller knows what to
                // implement next.
                assert!(
                    msg.contains(std::env::consts::OS),
                    "expected OS name in Unsupported message, got: {msg}",
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}

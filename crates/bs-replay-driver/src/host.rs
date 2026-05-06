// SPDX-License-Identifier: MIT
//! Host-OS CPU-feature enumerator.
//!
//! Linux: parses `/proc/cpuinfo`. The format is a per-cpu block of
//! `key : value` lines; we look for the first `flags:` line on
//! x86 / x86-64 or `Features:` on aarch64 — kernel-canonical feature
//! lists for the host CPU. Each token is one feature name (e.g.
//! `sse2`, `neon`, `aes`).
//!
//! Darwin: probes a curated list of `hw.optional.*` `sysctlbyname`
//! keys covering both Apple Silicon (neon, arm64, armv8_*) and
//! Intel-mac (sse2, sse4_2, avx1_0, avx2_0) feature surfaces.
//! Reports the keys whose value is `1`.
//!
//! Other OSes return [`HostDetectError::Unsupported`]. Callers that
//! already know their host features can side-step this entirely
//! and supply them directly to
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

#[cfg(target_os = "macos")]
fn detect() -> Result<Vec<String>, HostDetectError> {
    Ok(darwin::probe_hw_optional())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect() -> Result<Vec<String>, HostDetectError> {
    Err(HostDetectError::Unsupported(format!(
        "host_features() not implemented on {os}; \
         supply features directly to check_host_compatibility",
        os = std::env::consts::OS,
    )))
}

#[cfg(target_os = "macos")]
mod darwin {
    use std::ffi::CString;

    /// `(canonical_name, sysctl_key)` for the curated probe set.
    /// Names match the Linux `/proc/cpuinfo` flag where one exists
    /// so a recording made on Linux can replay on macOS (or vice
    /// versa) without a feature-name translation table.
    const CANDIDATES: &[(&str, &str)] = &[
        // Apple Silicon (aarch64) — `hw.optional.*` keys per
        // Apple's documented sysctl interface.
        ("neon", "hw.optional.neon"),
        ("arm64", "hw.optional.arm64"),
        ("fp", "hw.optional.floatingpoint"),
        ("armv8_1_atomics", "hw.optional.armv8_1_atomics"),
        ("armv8_crc32", "hw.optional.armv8_crc32"),
        ("armv8_2_fhm", "hw.optional.armv8_2_fhm"),
        ("armv8_2_sha512", "hw.optional.armv8_2_sha512"),
        ("armv8_2_sha3", "hw.optional.armv8_2_sha3"),
        // Intel-mac.
        ("sse2", "hw.optional.sse2"),
        ("sse3", "hw.optional.sse3"),
        ("ssse3", "hw.optional.supplementalsse3"),
        ("sse4_1", "hw.optional.sse4_1"),
        ("sse4_2", "hw.optional.sse4_2"),
        ("avx1_0", "hw.optional.avx1_0"),
        ("avx2_0", "hw.optional.avx2_0"),
        ("aes", "hw.optional.aes"),
    ];

    pub(super) fn probe_hw_optional() -> Vec<String> {
        let mut out = Vec::new();
        for &(name, key) in CANDIDATES {
            if sysctl_int(key) == Some(1) {
                out.push(name.to_owned());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Read a sysctl entry that returns a single `int`. `None` if
    /// the key doesn't exist or the call fails for any reason —
    /// the caller treats absence as "feature not present" which is
    /// the correct conservative behaviour for replay-host checks.
    fn sysctl_int(key: &str) -> Option<i32> {
        let c_key = CString::new(key).ok()?;
        let mut value: libc::c_int = 0;
        let mut size: libc::size_t = std::mem::size_of::<libc::c_int>();
        // SAFETY: c_key is null-terminated; size is initialised to
        // sizeof(c_int) and the buffer points at a valid c_int.
        // sysctlbyname writes at most `size` bytes and updates
        // `size` to bytes written. Failure returns -1 and we drop
        // the (possibly partial) read.
        let r = unsafe {
            libc::sysctlbyname(
                c_key.as_ptr(),
                &mut value as *mut libc::c_int as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 && size == std::mem::size_of::<libc::c_int>() {
            Some(value)
        } else {
            None
        }
    }
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

    #[cfg(target_os = "macos")]
    #[test]
    fn host_features_on_macos_returns_some_known_feature() {
        // Every Mac (Apple Silicon or Intel) advertises *something*
        // in hw.optional.*. On Apple Silicon we expect at least
        // `arm64` + `neon`; on Intel we expect at least `sse2`.
        // Either way the result must be non-empty.
        let feats = host_features().unwrap();
        assert!(!feats.is_empty(), "expected at least one CPU feature");
        #[cfg(target_arch = "aarch64")]
        {
            assert!(
                feats.iter().any(|f| f == "arm64") || feats.iter().any(|f| f == "neon"),
                "Apple Silicon should report arm64 or neon, got: {feats:?}",
            );
        }
        #[cfg(target_arch = "x86_64")]
        {
            assert!(
                feats.iter().any(|f| f == "sse2"),
                "Intel mac should report sse2, got: {feats:?}",
            );
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn host_features_on_other_os_reports_unsupported() {
        let err = host_features().unwrap_err();
        match err {
            HostDetectError::Unsupported(msg) => {
                assert!(
                    msg.contains(std::env::consts::OS),
                    "expected OS name in Unsupported message, got: {msg}",
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}

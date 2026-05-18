// SPDX-License-Identifier: MIT
//! Writer-side host capture: build a [`Manifest`] from the running
//! process + kernel + CPU features.
//!
//! Symmetric to the replay-time host checks. Same `host_features()`
//! enumerator feeds both: at write time we stamp what the recorder
//! observed; at replay time we verify the host can play it back.

use bs_replay_engine::VERSION as ENGINE_VERSION;
use bs_replay_engine::format::manifest::Manifest;
use bs_replay_engine::format::version::FormatVersion;

use crate::host::host_features;

/// Build a manifest from the host environment. The caller supplies
/// the binary's build-id; everything else is captured automatically:
///
/// - `format_version`: [`FormatVersion::V1`].
/// - `build_id`: the caller-supplied value.
/// - `kernel_release`: `/proc/sys/kernel/osrelease` on Linux,
///   `"unknown"` elsewhere (no fake detection).
/// - `cpu_features`: `host_features()`, or empty on
///   detection-unsupported hosts.
/// - `engine_version`: the engine crate's `CARGO_PKG_VERSION`.
/// - `initial_cwd`: `std::env::current_dir()` lossily decoded.
/// - `initial_args`: `std::env::args()` minus argv[0].
/// - `initial_env`: every `(K, V)` from `std::env::vars()` whose
///   key does *not* contain `=`. POSIX disallows that anyway, but
///   filtering belt-and-braces means the manifest is always
///   serialisable (the format's env-key contract from step 10).
///
/// Infallible by design — host detection failures fall back to
/// safe defaults rather than blocking the recorder.
pub fn capture_host_manifest(build_id: impl Into<String>) -> Manifest {
    let cpu_features = host_features().unwrap_or_default();
    let kernel_release = kernel_release_best_effort();
    let initial_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<unknown>".to_owned());
    let initial_args: Vec<String> = std::env::args().skip(1).collect();
    let initial_env: Vec<(String, String)> =
        std::env::vars().filter(|(k, _)| !k.contains('=')).collect();

    Manifest {
        format_version: FormatVersion::V1,
        build_id: build_id.into(),
        kernel_release,
        cpu_features,
        engine_version: ENGINE_VERSION.to_owned(),
        initial_env,
        initial_cwd,
        initial_args,
        // RFC 3339 / ISO-8601 wall-clock instant. The format
        // crate stores it as an opaque string; chrono is a driver-
        // level implementation detail. UTC chosen so traces are
        // comparable across hosts in different time zones.
        recorded_at: Some(chrono::Utc::now().to_rfc3339()),
        initial_fds: vec![],
    }
}

fn kernel_release_best_effort() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_owned();
            }
        }
    }
    "unknown".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_uses_supplied_build_id() {
        let m = capture_host_manifest("00ff");
        assert_eq!(m.build_id, "00ff");
    }

    #[test]
    fn capture_format_version_is_v1() {
        let m = capture_host_manifest("x");
        assert_eq!(m.format_version, FormatVersion::V1);
    }

    #[test]
    fn captured_manifest_round_trips_through_text() {
        // The captured env can be large; the round-trip exercises
        // the format's env-key sanitiser end-to-end.
        let m = capture_host_manifest("rt");
        let text = m.to_text();
        let back = Manifest::from_text(&text).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn engine_version_matches_engine_crate() {
        let m = capture_host_manifest("v");
        assert_eq!(m.engine_version, bs_replay_engine::VERSION);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_release_on_linux_is_non_unknown() {
        let m = capture_host_manifest("k");
        assert_ne!(m.kernel_release, "unknown");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn kernel_release_off_linux_is_unknown_sentinel() {
        // Honest about not detecting — no fake string.
        let m = capture_host_manifest("k");
        assert_eq!(m.kernel_release, "unknown");
    }

    #[test]
    fn recorded_at_is_populated_and_rfc3339() {
        let m = capture_host_manifest("ts");
        let ts = m
            .recorded_at
            .expect("recorded_at must be Some after step 27");
        // Round-trip through chrono confirms the format is what
        // we claim. UTC means the offset must be `+00:00` or `Z`.
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).expect("recorded_at not RFC 3339");
        assert_eq!(parsed.timezone(), chrono::FixedOffset::east_opt(0).unwrap());
    }

    #[test]
    fn recorded_at_round_trips_through_manifest_text() {
        // The format crate stores recorded_at as an opaque string,
        // but a captured timestamp must survive serialise + parse.
        let m = capture_host_manifest("rt-ts");
        let s = m.to_text();
        let back = Manifest::from_text(&s).unwrap();
        assert_eq!(back.recorded_at, m.recorded_at);
    }

    #[test]
    fn captured_env_has_no_equals_in_keys() {
        // Belt-and-braces check that the env-key contract holds.
        let m = capture_host_manifest("e");
        for (k, _) in &m.initial_env {
            assert!(!k.contains('='), "leaked env key with `=`: {k}");
        }
    }
}

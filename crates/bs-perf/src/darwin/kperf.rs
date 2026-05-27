// SPDX-License-Identifier: MIT
//! macOS Tier 1 perf scaffold — kperf cycles + IP sampling probe.
//!
//! What this module is:
//!
//! - The structured *probe* surface for kperf availability on the
//!   running host: dylib loadable? configurable PMU counters
//!   reported? entitlement-blocked? It's the macOS analog of
//!   `bs_perf::linux::probe_intel_pt`.
//! - The shape the future sampler will hang off
//!   (`KperfMonitor::open_for_pid`).
//!
//! What this module is **not** yet:
//!
//! - A working cycles+IP sampler. Action programming, timer
//!   programming, per-thread snapshot reads, source attribution,
//!   and teardown all live in a follow-up that pins exactly which
//!   samply commit's binding we're tracking. Calling
//!   [`KperfMonitor::open_for_pid`] today returns
//!   [`PerfError::Unsupported`] with a message saying so. The
//!   plan calls this out: "Apple's `kperf` is undocumented; pin to
//!   a specific commit of `samply` for the binding shape"
//!   (`doc/plans/phase-6-perf-overlay.md`, last line).
//!
//! Why ship the probe before the sampler? Two reasons. First, the
//! probe is genuinely useful by itself — the macOS DAP body can
//! tell the client what's blocking the gutter heat-map (no dylib,
//! no PMU, entitlement missing) instead of a black-box "Tier 1
//! pending". Second, the probe locks in the symbols we'll need; if
//! Apple ships a macOS release that strips one, we hear about it at
//! probe time rather than discovering it inside the hot sampling
//! path.

use crate::PerfError;
use crate::darwin::symbols::{KperfSymbolError, kpc_class, library};

/// Single error path for kperf operations. Most variants are
/// scaffold-time — we'll grow this as the sampler lands.
#[derive(Debug, thiserror::Error)]
pub enum KperfError {
    /// The dylib could not be loaded or a required symbol was missing.
    #[error("kperf symbols unavailable: {0}")]
    Symbols(String),
    /// `kpc_get_counter_count(CONFIGURABLE)` returned zero — the
    /// PMU is reachable but no configurable counters are exposed.
    /// On Apple Silicon this typically means the host is in a
    /// power-restricted state or an OS update has tightened the
    /// surface.
    #[error("kperf reports no configurable PMU counters on this host")]
    NoConfigurableCounters,
    /// Sampling not yet implemented in this commit. The scaffold
    /// ships only the probe surface.
    #[error("kperf sampler not yet wired (scaffold-only build)")]
    NotYetImplemented,
}

impl From<KperfError> for PerfError {
    fn from(err: KperfError) -> Self {
        match err {
            KperfError::Symbols(_)
            | KperfError::NoConfigurableCounters
            | KperfError::NotYetImplemented => PerfError::Unsupported,
        }
    }
}

/// What the probe found out about the running host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KperfStatus {
    /// All probed symbols resolved and the PMU reports configurable
    /// counters. Sampling can be attempted (but is scaffolded —
    /// see [`KperfMonitor::open_for_pid`]).
    Available {
        /// Library path that loaded.
        library_path: String,
        /// `kpc_get_counter_count(CONFIGURABLE)` result.
        configurable_counters: u32,
    },
    /// Counters exist but the kernel refused `kpc_force_all_ctrs_set`
    /// — the process likely needs the
    /// `com.apple.private.kpc.read-or-trace` entitlement or to be
    /// running with elevated privileges. samply documents the same
    /// requirement.
    PermissionLikelyRequired {
        /// Library path that loaded.
        library_path: String,
        /// `kpc_get_counter_count(CONFIGURABLE)` result.
        configurable_counters: u32,
        /// `errno` from the `kpc_force_all_ctrs_set` attempt.
        force_set_errno: i32,
    },
    /// kperf is not usable. `reason` is a structured cause.
    Unavailable(KperfUnavailableReason),
}

/// Structured "why not" for [`KperfStatus::Unavailable`]. Matches
/// the spirit of `IntelPtUnavailableReason` on Linux so DAP clients
/// can do the same kind of diagnosis flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KperfUnavailableReason {
    /// `dlopen` failed for every candidate path.
    LibraryNotFound {
        /// `dlerror()` text from the final attempt.
        dlerror: String,
    },
    /// A required symbol was missing from the loaded library —
    /// typically a macOS-version skew.
    MissingSymbol {
        /// Path that loaded successfully.
        path: String,
        /// Symbol name that was looked up.
        symbol: &'static str,
        /// `dlerror()` text for the missing symbol.
        dlerror: String,
    },
}

/// Run the probe. Cheap to call repeatedly — the library load is
/// cached behind a `OnceLock`, and `kpc_get_counter_count` is a
/// non-mutating kernel syscall.
pub fn probe_kperf() -> KperfStatus {
    let lib = match library() {
        Ok(lib) => lib,
        Err(err) => return KperfStatus::Unavailable(unavailable_from_symbols(err)),
    };

    // SAFETY: function pointer was resolved at library load.
    let counters = unsafe { (lib.kpc_get_counter_count)(kpc_class::CONFIGURABLE) };
    let counter_count = if counters < 0 { 0 } else { counters as u32 };

    // Quick PMU-grab probe: try to force-take, then restore. Any
    // non-zero return means the kernel refused — almost always an
    // entitlement issue. We don't *leave* the PMU forced; this is a
    // probe.
    // SAFETY: function pointer was resolved at library load.
    let force_attempt = unsafe { (lib.kpc_force_all_ctrs_set)(1) };
    if force_attempt != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return KperfStatus::PermissionLikelyRequired {
            library_path: lib.path().to_owned(),
            configurable_counters: counter_count,
            force_set_errno: errno,
        };
    }
    // Best-effort restore. Ignoring the result here is fine — if
    // this fails, the worst case is we leave the PMU "owned" by us,
    // which a subsequent reset clears.
    // SAFETY: function pointer was resolved at library load.
    let _ = unsafe { (lib.kpc_force_all_ctrs_set)(0) };

    KperfStatus::Available {
        library_path: lib.path().to_owned(),
        configurable_counters: counter_count,
    }
}

fn unavailable_from_symbols(err: &KperfSymbolError) -> KperfUnavailableReason {
    match err {
        KperfSymbolError::Dlopen { last_dlerror } => KperfUnavailableReason::LibraryNotFound {
            dlerror: last_dlerror.clone(),
        },
        KperfSymbolError::MissingSymbol {
            path,
            symbol,
            dlerror,
        } => KperfUnavailableReason::MissingSymbol {
            path: path.clone(),
            symbol,
            dlerror: dlerror.clone(),
        },
    }
}

/// Future per-process / per-thread sampler. Today an empty marker;
/// the open call below returns Unsupported until the sampler lands.
#[derive(Debug)]
pub struct KperfMonitor {
    _private: (),
}

impl KperfMonitor {
    /// Open a cycles+IP sampling session for `pid`. Currently
    /// returns [`PerfError::Unsupported`] — the scaffold lands the
    /// dlsym shim and the probe surface; the action/timer/sampler
    /// programming is the next commit.
    pub fn open_for_pid(_pid: i32) -> Result<Self, PerfError> {
        Err(KperfError::NotYetImplemented.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe should always return *something* — never panic, never
    /// loop. We accept any variant because CI environments vary
    /// (sandboxed Mac runners typically can't load private dylibs).
    #[test]
    fn probe_returns_one_of_the_documented_variants() {
        let status = probe_kperf();
        // Exhaustive match keeps this honest: if we add a new
        // variant the test fails until we acknowledge it here.
        match status {
            KperfStatus::Available {
                configurable_counters,
                ..
            } => {
                eprintln!("kperf available; {configurable_counters} configurable counter(s)");
            }
            KperfStatus::PermissionLikelyRequired {
                force_set_errno, ..
            } => {
                eprintln!("kperf permission-blocked; errno={force_set_errno}");
            }
            KperfStatus::Unavailable(KperfUnavailableReason::LibraryNotFound { dlerror }) => {
                eprintln!("kperf dylib not found: {dlerror}");
            }
            KperfStatus::Unavailable(KperfUnavailableReason::MissingSymbol {
                path,
                symbol,
                dlerror,
            }) => {
                eprintln!("kperf missing {symbol} in {path}: {dlerror}");
            }
        }
    }

    #[test]
    fn open_for_pid_is_unsupported_for_now() {
        let r = KperfMonitor::open_for_pid(std::process::id() as i32);
        assert!(matches!(r, Err(PerfError::Unsupported)));
    }
}

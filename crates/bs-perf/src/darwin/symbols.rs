// SPDX-License-Identifier: MIT
//! `libkperf.dylib` symbol shim for the Phase 6 Darwin Tier 1
//! (cycles + IP sampling) path.
//!
//! Apple's `kperf` is a private system framework: no header, no
//! docs, no stability promise — the symbol set drifts between macOS
//! releases. We therefore bind it at **runtime** via `libloading`
//! (`dlopen` + `dlsym` under the hood) rather than link-time, so a
//! host missing a symbol gives us a clean `Err` to surface in the
//! probe instead of a `dyld` load fault.
//!
//! References for the symbol shapes are XNU's `osfmk/kperf/` and the
//! community `kpc_demo.c` — **not** samply, which deliberately
//! samples by polling (`thread_suspend` + `thread_get_state`) and
//! binds no kperf at all. That polling path is our Tier 1b sampler;
//! this kperf shim is the entitled/root Tier 1a probe.
//!
//! ### Scope
//!
//! Probe surface only. The struct holds typed function pointers and
//! [`KperfLibrary::open`] resolves them; [`super::kperf`] uses them
//! to report availability/permission. **No sampling is wired in** —
//! `KperfMonitor::open_for_pid` returns `Unsupported`, and the
//! cross-process sampler is out of scope (root-only + the
//! undocumented kdebug buffer; see `doc/plans/phase-6-perf-overlay.md`).

use std::os::raw::c_int;
use std::sync::OnceLock;

use libloading::{Library, Symbol};
use thiserror::Error;

/// Paths to try, in order. The framework path is the fallback
/// because some macOS releases ship the dylib only under the
/// framework bundle; the `/usr/lib/system/...` path is what Apple's
/// own subsystems link against and so tends to be the cached one.
const LIBKPERF_PATHS: &[&str] = &[
    "/usr/lib/system/libkperf.dylib",
    "/System/Library/PrivateFrameworks/kperf.framework/kperf",
];

/// Counter classes from `<kperf/kpc.h>` (Apple private). Masks; OR
/// together when configuring `kpc_set_counting` / friends.
pub mod kpc_class {
    /// Fixed-function counters (cycles, instructions retired).
    pub const FIXED: u32 = 1;
    /// Configurable counters (PMC0..PMCn — user-selectable events).
    pub const CONFIGURABLE: u32 = 2;
    /// Power-domain counters (energy, frequency).
    pub const POWER: u32 = 4;
    /// Raw PMU counters — direct PMC register access.
    pub const RAWPMU: u32 = 8;
}

/// Sampler bits from `<kperf/kperf_samplers.h>` (Apple private).
/// `PMC_THREAD` is the one we want — per-thread PMC snapshot at
/// each timer fire, the foundation of the cycles+IP heat-map.
pub mod kperf_sampler {
    /// Per-thread descriptive info — name, pid, tid, qos.
    pub const TH_INFO: u32 = 1 << 0;
    /// Per-thread schedstate snapshot at sample time.
    pub const TH_SNAPSHOT: u32 = 1 << 1;
    /// Kernel-side call stack at sample time.
    pub const KSTACK: u32 = 1 << 2;
    /// User-side call stack — what flame-graph profilers build from.
    pub const USTACK: u32 = 1 << 3;
    /// Per-thread PMC values at sample time — cycles+IP heat-map source.
    pub const PMC_THREAD: u32 = 1 << 4;
    /// Per-CPU PMC values; lower-overhead than `PMC_THREAD`.
    pub const PMC_CPU: u32 = 1 << 5;
    /// PMC configuration (event-select registers) at sample time.
    pub const PMC_CONFIG: u32 = 1 << 6;
    /// Per-thread memory-pressure snapshot.
    pub const MEMINFO: u32 = 1 << 7;
}

/// Loaded `libkperf.dylib` plus the typed function pointers we use.
///
/// `_lib` keeps the library mapped for as long as the bare function
/// pointers extracted from it live — both are in this struct, the
/// struct lives in a process-lifetime `OnceLock`, and we never move
/// the library out, so the pointers stay valid. `libloading::Library`
/// is `Send + Sync` and bare `extern "C" fn` pointers are too, so the
/// whole struct is auto-`Send + Sync` — no hand-written `unsafe impl`.
pub struct KperfLibrary {
    _lib: Library,
    path: String,
    /// `int kpc_get_counter_count(uint32_t classes)` — counters the
    /// kernel reports for the class mask (CONFIGURABLE > 0 means the
    /// PMU is exposed at all).
    pub kpc_get_counter_count: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_set_counting(uint32_t classes)` — enable PMU counting
    /// for the class mask process-wide.
    pub kpc_set_counting: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_set_thread_counting(uint32_t classes)` — enable
    /// counting on the calling thread.
    pub kpc_set_thread_counting: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_force_all_ctrs_set(int val)` — force-take the PMU.
    /// Requires the kpc entitlement or root on modern macOS.
    pub kpc_force_all_ctrs_set: unsafe extern "C" fn(c_int) -> c_int,
    /// `int kpc_get_thread_counters(uint32_t tid, uint32_t buf_count, uint64_t *buf)`
    /// — snapshot the calling thread's accumulated PMC values into `buf`
    /// (`buf_count` u64 slots). `tid` is informational; the kernel returns the
    /// current thread's counters. Needs `kpc_set_thread_counting` enabled first.
    pub kpc_get_thread_counters: unsafe extern "C" fn(u32, u32, *mut u64) -> c_int,
    /// `int kpc_set_config(uint32_t classes, uint64_t *config)` — program the
    /// configurable counters' event-select words (sized to
    /// `kpc_get_counter_count(classes)`). The config words come from the KPEP
    /// database (`kpep_config_kpc`).
    pub kpc_set_config: unsafe extern "C" fn(u32, *mut u64) -> c_int,
    /// `int kperf_action_count_set(uint32_t count)` — allocate N
    /// action slots.
    pub kperf_action_count_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_action_samplers_set(uint32_t action, uint32_t samplers)`
    pub kperf_action_samplers_set: unsafe extern "C" fn(u32, u32) -> c_int,
    /// `int kperf_timer_count_set(uint32_t count)` — allocate N timer slots.
    pub kperf_timer_count_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_timer_period_set(uint32_t timer, uint64_t period_ticks)`
    pub kperf_timer_period_set: unsafe extern "C" fn(u32, u64) -> c_int,
    /// `int kperf_timer_action_set(uint32_t timer, uint32_t action)`
    pub kperf_timer_action_set: unsafe extern "C" fn(u32, u32) -> c_int,
    /// `int kperf_sample_set(uint32_t enable)` — flip the sampler on/off.
    pub kperf_sample_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_reset(void)` — clear all kperf state. Teardown.
    pub kperf_reset: unsafe extern "C" fn() -> c_int,
    /// `uint64_t kperf_ns_to_ticks(uint64_t ns)` — sampling period in
    /// nanoseconds → mach absolute ticks.
    pub kperf_ns_to_ticks: unsafe extern "C" fn(u64) -> u64,
}

impl std::fmt::Debug for KperfLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KperfLibrary")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Failure modes when loading `libkperf.dylib` or resolving its
/// symbols. Surfaced through [`KperfStatus::Unavailable`].
#[derive(Debug, Error)]
pub enum KperfSymbolError {
    /// Loading the dylib failed for every candidate path. `last_dlerror`
    /// holds the loader error from the final attempt.
    #[error("dlopen failed for libkperf.dylib: {last_dlerror}")]
    Dlopen {
        /// Loader error text from the last failed attempt.
        last_dlerror: String,
    },
    /// A symbol we need was missing from the loaded library.
    /// This typically means Apple removed it in a macOS update.
    #[error("symbol \"{symbol}\" not found in {path}: {dlerror}")]
    MissingSymbol {
        /// Library path that loaded successfully.
        path: String,
        /// Name of the symbol that was missing.
        symbol: &'static str,
        /// Loader error text from the failed lookup.
        dlerror: String,
    },
}

/// Process-wide loaded library handle. Cached so probes and any
/// later callers share the one `dlopen` result.
static LIBRARY: OnceLock<Result<KperfLibrary, KperfSymbolError>> = OnceLock::new();

/// Load (or return the cached) `libkperf.dylib`. First call wins.
pub fn library() -> Result<&'static KperfLibrary, &'static KperfSymbolError> {
    LIBRARY.get_or_init(KperfLibrary::open).as_ref()
}

impl KperfLibrary {
    fn open() -> Result<Self, KperfSymbolError> {
        let (lib, path) = open_first_available()?;

        // Resolve one symbol into a bare `extern "C" fn` pointer.
        //
        // SAFETY: each use asserts that the named symbol has the C
        // ABI type given — the type comes from XNU's <kperf/*.h>.
        // This type assertion is the irreducible unsafety of binding
        // an undocumented C API; everything around it (dlsym, the
        // missing-symbol `Err` instead of a crash) is libloading's
        // job. The bare pointer copied out by `*symbol` stays valid
        // as long as `lib` — stored in the returned struct — lives.
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let nul = concat!($name, "\0").as_bytes();
                let symbol: Symbol<$ty> =
                    unsafe { lib.get(nul) }.map_err(|e| KperfSymbolError::MissingSymbol {
                        path: path.clone(),
                        symbol: $name,
                        dlerror: e.to_string(),
                    })?;
                *symbol
            }};
        }

        Ok(Self {
            kpc_get_counter_count: sym!(
                "kpc_get_counter_count",
                unsafe extern "C" fn(u32) -> c_int
            ),
            kpc_set_counting: sym!("kpc_set_counting", unsafe extern "C" fn(u32) -> c_int),
            kpc_set_thread_counting: sym!(
                "kpc_set_thread_counting",
                unsafe extern "C" fn(u32) -> c_int
            ),
            kpc_force_all_ctrs_set: sym!(
                "kpc_force_all_ctrs_set",
                unsafe extern "C" fn(c_int) -> c_int
            ),
            kpc_get_thread_counters: sym!(
                "kpc_get_thread_counters",
                unsafe extern "C" fn(u32, u32, *mut u64) -> c_int
            ),
            kpc_set_config: sym!(
                "kpc_set_config",
                unsafe extern "C" fn(u32, *mut u64) -> c_int
            ),
            kperf_action_count_set: sym!(
                "kperf_action_count_set",
                unsafe extern "C" fn(u32) -> c_int
            ),
            kperf_action_samplers_set: sym!(
                "kperf_action_samplers_set",
                unsafe extern "C" fn(u32, u32) -> c_int
            ),
            kperf_timer_count_set: sym!(
                "kperf_timer_count_set",
                unsafe extern "C" fn(u32) -> c_int
            ),
            kperf_timer_period_set: sym!(
                "kperf_timer_period_set",
                unsafe extern "C" fn(u32, u64) -> c_int
            ),
            kperf_timer_action_set: sym!(
                "kperf_timer_action_set",
                unsafe extern "C" fn(u32, u32) -> c_int
            ),
            kperf_sample_set: sym!("kperf_sample_set", unsafe extern "C" fn(u32) -> c_int),
            kperf_reset: sym!("kperf_reset", unsafe extern "C" fn() -> c_int),
            kperf_ns_to_ticks: sym!("kperf_ns_to_ticks", unsafe extern "C" fn(u64) -> u64),
            _lib: lib,
            path,
        })
    }

    /// Library path that loaded successfully.
    pub fn path(&self) -> &str {
        &self.path
    }
}

fn open_first_available() -> Result<(Library, String), KperfSymbolError> {
    let mut last_dlerror = String::from("no candidate paths attempted");
    for &candidate in LIBKPERF_PATHS {
        // SAFETY: `Library::new` runs the dylib's initialisers on
        // load. libkperf is a system framework with no hostile init,
        // so loading it is sound; a missing file is a returned `Err`.
        match unsafe { Library::new(candidate) } {
            Ok(lib) => return Ok((lib, candidate.to_owned())),
            Err(e) => last_dlerror = format!("{candidate}: {e}"),
        }
    }
    Err(KperfSymbolError::Dlopen { last_dlerror })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On macOS 13/14/15 the dylib should load. We don't assert on
    /// individual symbols because they drift across releases — the
    /// individual MissingSymbol paths are exercised by the probe.
    /// We DO assert that *if* it loads, the path is one of the two
    /// candidates we recognise.
    #[test]
    fn library_loads_or_reports_dlopen_failure() {
        match library() {
            Ok(lib) => {
                assert!(LIBKPERF_PATHS.iter().any(|p| *p == lib.path()));
            }
            Err(err) => {
                // CI runners and sandboxed test environments may
                // refuse the load. Treat that as not-an-error here;
                // the higher-level probe surfaces the cause.
                eprintln!("kperf library unavailable in this environment: {err}");
            }
        }
    }
}

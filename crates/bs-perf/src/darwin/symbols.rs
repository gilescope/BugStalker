// SPDX-License-Identifier: MIT
//! `libkperf.dylib` symbol shim for the Phase 6 Darwin Tier 1
//! (cycles + IP sampling) path.
//!
//! Apple's `kperf` is a private system framework. There is no
//! header, no docs, no stability promise — the symbol set drifts
//! between macOS releases. The plan (`doc/plans/phase-6-perf-overlay.md`)
//! pins us to [`mstange/samply`](https://github.com/mstange/samply)
//! for the binding shape, on the principle that samply already
//! tracks the API churn for everyone.
//!
//! This module is the dlsym shim. It tries the documented library
//! paths (`/usr/lib/system/libkperf.dylib` first, falling back to
//! the framework path), loads the function pointers we care about,
//! and reports missing symbols by name so a future macOS version
//! removal is debuggable rather than a black-box "kperf
//! unavailable".
//!
//! ### Scope of this commit
//!
//! Scaffold only. The struct holds typed function pointers and
//! [`KperfLibrary::open`] does the lookup; [`super::kperf`] uses it
//! through a probe today. **No sampling is wired in.** The
//! `KperfMonitor::open_for_pid` entry point still returns
//! `PerfError::Unsupported` until the periodic-timer + action
//! programming lands in a follow-up.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use thiserror::Error;

/// Paths to try, in order. The framework path is the fallback
/// because some macOS releases ship the dylib only under the
/// framework bundle. samply tries the framework path first; we
/// flip the order because Apple's own subsystems link against the
/// `/usr/lib/system/...` path and so it tends to be the cached one.
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
    /// User-side call stack — what samply uses to build flame graphs.
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

/// One handle to the loaded `libkperf.dylib` plus the typed function
/// pointers we use. Dropped on shutdown; `dlclose` is best-effort.
pub struct KperfLibrary {
    handle: *mut c_void,
    path: String,
    /// `int kpc_get_counter_count(uint32_t classes)` — number of
    /// counters reported by the kernel for the requested class
    /// mask. Useful sanity check (CONFIGURABLE > 0 means the PMU
    /// is exposed at all).
    pub kpc_get_counter_count: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_set_counting(uint32_t classes)` — enable PMU
    /// counting for the given class mask process-wide.
    pub kpc_set_counting: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_set_thread_counting(uint32_t classes)` — enable
    /// counting on the calling thread.
    pub kpc_set_thread_counting: unsafe extern "C" fn(u32) -> c_int,
    /// `int kpc_force_all_ctrs_set(int val)` — force-take the PMU.
    /// Requires entitlement on modern macOS.
    pub kpc_force_all_ctrs_set: unsafe extern "C" fn(c_int) -> c_int,
    /// `int kperf_action_count_set(uint32_t count)` — allocate N
    /// action slots. samply uses 1 (one action covering all
    /// samplers).
    pub kperf_action_count_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_action_samplers_set(uint32_t action, uint32_t samplers)`
    pub kperf_action_samplers_set: unsafe extern "C" fn(u32, u32) -> c_int,
    /// `int kperf_timer_count_set(uint32_t count)` — allocate N
    /// timer slots.
    pub kperf_timer_count_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_timer_period_set(uint32_t timer, uint64_t period_ticks)`
    pub kperf_timer_period_set: unsafe extern "C" fn(u32, u64) -> c_int,
    /// `int kperf_timer_action_set(uint32_t timer, uint32_t action)`
    pub kperf_timer_action_set: unsafe extern "C" fn(u32, u32) -> c_int,
    /// `int kperf_sample_set(uint32_t enable)` — flip the sampler on/off.
    pub kperf_sample_set: unsafe extern "C" fn(u32) -> c_int,
    /// `int kperf_reset(void)` — clear all kperf state. Called on teardown.
    pub kperf_reset: unsafe extern "C" fn() -> c_int,
    /// `uint64_t kperf_ns_to_ticks(uint64_t ns)` — convert sampling
    /// period from a wall-clock figure to mach absolute ticks.
    pub kperf_ns_to_ticks: unsafe extern "C" fn(u64) -> u64,
}

impl std::fmt::Debug for KperfLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KperfLibrary")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

// SAFETY: the inner function pointers point into a dylib we never
// unload until process exit; calling them from multiple threads is
// kperf's own responsibility (most calls are documented to require
// kperf_lock, which the library serialises internally).
unsafe impl Send for KperfLibrary {}
unsafe impl Sync for KperfLibrary {}

/// Failure modes when loading `libkperf.dylib` or resolving its
/// symbols. Surfaced through [`KperfStatus::Unavailable`].
#[derive(Debug, Error)]
pub enum KperfSymbolError {
    /// `dlopen` failed for every candidate path. `last_dlerror`
    /// holds the `dlerror()` string from the final attempt.
    #[error("dlopen failed for libkperf.dylib: {last_dlerror}")]
    Dlopen {
        /// `dlerror()` text from the last failed attempt.
        last_dlerror: String,
    },
    /// A symbol we need was missing from the loaded library.
    /// This typically means Apple removed it in a macOS update.
    #[error("dlsym(\"{symbol}\") returned NULL in {path}: {dlerror}")]
    MissingSymbol {
        /// Library path that loaded successfully.
        path: String,
        /// Name of the symbol that was missing.
        symbol: &'static str,
        /// `dlerror()` text from the failed lookup.
        dlerror: String,
    },
}

/// Process-wide loaded library handle. We cache it so probes and
/// later samplers share the same dlopen result — repeated dlopens
/// of system frameworks aren't free.
static LIBRARY: OnceLock<Result<KperfLibrary, KperfSymbolError>> = OnceLock::new();

/// Load (or return the cached) `libkperf.dylib`. First call wins.
pub fn library() -> Result<&'static KperfLibrary, &'static KperfSymbolError> {
    LIBRARY.get_or_init(KperfLibrary::open).as_ref()
}

impl KperfLibrary {
    fn open() -> Result<Self, KperfSymbolError> {
        let (handle, path) = open_first_available()?;

        // Helper to resolve one symbol or surface a structured
        // failure with the dlerror text. dlerror() returns the
        // last error; we clear it before each call so the message
        // we report is the one from *this* dlsym.
        let resolve = |name: &'static str| -> Result<*mut c_void, KperfSymbolError> {
            // SAFETY: `dlerror()` has thread-local storage and is
            // safe to call from any thread.
            unsafe { libc::dlerror() };
            let c_name = CString::new(name).expect("kperf symbol names are static ASCII");
            // SAFETY: handle is a valid dlopen result; name is a
            // null-terminated C string.
            let sym = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
            if sym.is_null() {
                // SAFETY: dlerror returns a thread-local C string
                // pointer that's stable until the next dlerror call.
                let err = unsafe { libc::dlerror() };
                let msg = if err.is_null() {
                    String::from("symbol not found")
                } else {
                    // SAFETY: pointer non-null and points at a NUL-
                    // terminated C string owned by libdl.
                    unsafe { CStr::from_ptr(err) }
                        .to_string_lossy()
                        .into_owned()
                };
                return Err(KperfSymbolError::MissingSymbol {
                    path: path.clone(),
                    symbol: name,
                    dlerror: msg,
                });
            }
            Ok(sym)
        };

        // SAFETY for all transmutes below: the C ABIs match the
        // declared `extern "C" fn` signatures one-for-one against
        // `<kperf/*.h>` (Apple private). If Apple changes a
        // signature, we'd only catch it at runtime, not here —
        // hence the version-tracked samply pin.
        let kpc_get_counter_count = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kpc_get_counter_count",
            )?)
        };
        let kpc_set_counting = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kpc_set_counting",
            )?)
        };
        let kpc_set_thread_counting = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kpc_set_thread_counting",
            )?)
        };
        let kpc_force_all_ctrs_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int) -> c_int>(resolve(
                "kpc_force_all_ctrs_set",
            )?)
        };
        let kperf_action_count_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kperf_action_count_set",
            )?)
        };
        let kperf_action_samplers_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, u32) -> c_int>(resolve(
                "kperf_action_samplers_set",
            )?)
        };
        let kperf_timer_count_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kperf_timer_count_set",
            )?)
        };
        let kperf_timer_period_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, u64) -> c_int>(resolve(
                "kperf_timer_period_set",
            )?)
        };
        let kperf_timer_action_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, u32) -> c_int>(resolve(
                "kperf_timer_action_set",
            )?)
        };
        let kperf_sample_set = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32) -> c_int>(resolve(
                "kperf_sample_set",
            )?)
        };
        let kperf_reset = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> c_int>(resolve(
                "kperf_reset",
            )?)
        };
        let kperf_ns_to_ticks = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u64) -> u64>(resolve(
                "kperf_ns_to_ticks",
            )?)
        };

        Ok(Self {
            handle,
            path,
            kpc_get_counter_count,
            kpc_set_counting,
            kpc_set_thread_counting,
            kpc_force_all_ctrs_set,
            kperf_action_count_set,
            kperf_action_samplers_set,
            kperf_timer_count_set,
            kperf_timer_period_set,
            kperf_timer_action_set,
            kperf_sample_set,
            kperf_reset,
            kperf_ns_to_ticks,
        })
    }

    /// Library path that loaded successfully.
    pub fn path(&self) -> &str {
        &self.path
    }
}

fn open_first_available() -> Result<(*mut c_void, String), KperfSymbolError> {
    let mut last_dlerror = String::from("no candidate paths attempted");
    for &candidate in LIBKPERF_PATHS {
        let c_path = CString::new(candidate).expect("static ASCII path");
        // SAFETY: dlopen accepts NULL or a valid NUL-terminated path.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_LAZY) };
        if !handle.is_null() {
            return Ok((handle, candidate.to_owned()));
        }
        // SAFETY: dlerror() returns thread-local storage; ptr stable
        // until next dlerror call.
        let err = unsafe { libc::dlerror() };
        last_dlerror = if err.is_null() {
            format!("{candidate}: dlopen returned NULL but dlerror was empty")
        } else {
            let msg = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            format!("{candidate}: {msg}")
        };
    }
    Err(KperfSymbolError::Dlopen { last_dlerror })
}

impl Drop for KperfLibrary {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: handle was a successful dlopen result. We're
            // the only owner because this lives in OnceLock and is
            // never moved.
            let _ = unsafe { libc::dlclose(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

// Force unused warnings to stay quiet on the c_char import even if
// the file's first build doesn't reference it directly. The CString
// path takes &CStr → *const c_char internally.
#[allow(dead_code)]
fn _force_c_char_used(_: c_char) {}

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

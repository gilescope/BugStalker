// SPDX-License-Identifier: MIT
//! Feasibility probe for the macOS kperf/PMU tier (`debug-step-costs.md` #3,
//! "doing better with signing"). Prints what `bs_perf::darwin::probe_kperf`
//! finds on the running host. Run as the current user and via `sudo` to see
//! whether elevated privileges unlock the configurable PMU counters that a
//! user-mode-only instruction count would need:
//!
//!   cargo build -p bs-perf --example kperf_probe
//!   ./target/debug/examples/kperf_probe
//!   sudo ./target/debug/examples/kperf_probe
fn main() {
    #[cfg(target_os = "macos")]
    {
        let euid = unsafe { libc::geteuid() };
        println!("euid={euid} ({})", if euid == 0 { "root" } else { "user" });
        println!("probe_kperf() => {:#?}", bs_perf::darwin::probe_kperf());
    }
    #[cfg(not(target_os = "macos"))]
    println!("kperf probe is macOS-only");
}

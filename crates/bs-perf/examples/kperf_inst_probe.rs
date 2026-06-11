// SPDX-License-Identifier: MIT
//! PMU instruction-counter feasibility probe (`debug-step-costs.md` #3, the
//! "precise / signed-Apple" tier). Root-only. Answers two questions that decide
//! whether the kperf path can replace the `TrapFloor` approximation, and what
//! the privileged helper's shape must be:
//!
//!   1. Does a FIXED PMU counter, read via `kpc_get_thread_counters`, track a
//!      known-N workload linearly (i.e. is it a usable instruction count)?
//!   2. Are the FIXED counters user+kernel or user-only? Run a syscall-heavy
//!      loop: if the per-iteration count balloons vs a pure-user loop, FIXED
//!      counts kernel (EL1) too — so we'd need *configurable user-only* events
//!      to make the ~35k/trap kernel floor disappear. If it doesn't balloon,
//!      FIXED is already what we want.
//!
//!   cargo build -p bs-perf --example kperf_inst_probe
//!   sudo ./target/debug/examples/kperf_inst_probe
//!
//! This reads the *probe's own* thread counters (self-thread). Cross-process
//! attribution (reading the debuggee's thread from bs) is the next probe; this
//! one establishes whether the counter is clean and what the kernel does to it.
fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    run();
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    println!("kperf instruction probe is macOS/aarch64-only");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run() {
    use bs_perf::darwin::symbols::{KperfLibrary, kpc_class, library};
    use std::hint::black_box;

    let euid = unsafe { libc::geteuid() };
    println!("euid={euid} ({})", if euid == 0 { "root" } else { "user" });

    let lib = match library() {
        Ok(lib) => lib,
        Err(e) => {
            eprintln!("kperf unavailable: {e}");
            return;
        }
    };

    // FIXED class = always-on cycles + instructions; no chip-specific event
    // programming needed (that's the CONFIGURABLE path + KPEP db).
    // SAFETY: resolved fn pointer, plain integer arg/return.
    let fixed = unsafe { (lib.kpc_get_counter_count)(kpc_class::FIXED) };
    println!("FIXED counters reported: {fixed}");
    if fixed <= 0 {
        eprintln!("no FIXED counters — cannot probe");
        return;
    }
    let n = usize::try_from(fixed).unwrap();

    // Take the PMU and enable counting (process-wide + per-thread accumulation).
    // SAFETY: resolved fn pointers; the class mask is a documented kpc constant.
    let force = unsafe { (lib.kpc_force_all_ctrs_set)(1) };
    if force != 0 {
        eprintln!(
            "kpc_force_all_ctrs_set failed (errno {}). Run under sudo.",
            std::io::Error::last_os_error()
        );
        return;
    }
    // SAFETY: as above.
    unsafe {
        let _ = (lib.kpc_set_counting)(kpc_class::FIXED);
        let _ = (lib.kpc_set_thread_counting)(kpc_class::FIXED);
    }

    // --- Q1: does a FIXED counter track a known-N user workload linearly? ---
    println!("\n--- pure-user workload (wrapping_add loop) ---");
    println!("{:>12}  per-counter Δ (and Δ/iter)", "iters");
    for iters in [100_000u64, 1_000_000, 10_000_000] {
        let before = read_thread_counters(lib, n);
        black_box(user_workload(black_box(iters)));
        let after = read_thread_counters(lib, n);
        print_delta(iters, &before, &after);
    }

    // --- Q2: are FIXED counters user+kernel? syscall-heavy loop ---
    println!("\n--- syscall-heavy workload (close(-1) loop) ---");
    let iters = 1_000_000u64;
    let before = read_thread_counters(lib, n);
    syscall_workload(iters);
    let after = read_thread_counters(lib, n);
    print_delta(iters, &before, &after);
    println!(
        "(if Δ/iter here ≫ the pure-user Δ/iter, FIXED counts kernel (EL1) too →\n \
         user-only CONFIGURABLE events are required to kill the trap floor)"
    );

    // Restore: hand the PMU back.
    // SAFETY: resolved fn pointer, plain integer arg.
    unsafe {
        let _ = (lib.kpc_force_all_ctrs_set)(0);
    }
    println!("\nPMU released.");

    /// Snapshot the calling thread's FIXED counters. `n` == FIXED counter count.
    fn read_thread_counters(lib: &KperfLibrary, n: usize) -> Vec<u64> {
        let mut buf = vec![0u64; n];
        // SAFETY: `buf` has `n` slots and we pass `n` as the count; the pointer
        // is valid for the call. `kpc_set_thread_counting(FIXED)` was enabled.
        let rc = unsafe {
            (lib.kpc_get_thread_counters)(0, u32::try_from(n).unwrap(), buf.as_mut_ptr())
        };
        assert_eq!(
            rc,
            0,
            "kpc_get_thread_counters failed: {}",
            std::io::Error::last_os_error()
        );
        buf
    }

    fn print_delta(iters: u64, before: &[u64], after: &[u64]) {
        print!("{iters:>12} ");
        for (i, (a, b)) in before.iter().zip(after).enumerate() {
            let d = b.saturating_sub(*a);
            let per = d as f64 / iters as f64;
            print!(" | c{i}: {d:>11} ({per:.2}/it)");
        }
        println!();
    }

    /// ~Known instruction count: a tight wrapping-add loop, black-boxed so the
    /// optimizer can't fold or vectorize it. Pure user-mode, no syscalls.
    fn user_workload(n: u64) -> u64 {
        let mut acc = 0u64;
        let mut k = 0u64;
        while k < n {
            acc = acc.wrapping_add(black_box(k));
            k = k.wrapping_add(1);
        }
        acc
    }

    /// One real syscall per iteration: `close(-1)` returns `EBADF` with no side
    /// effect but does a full user→kernel→user round trip.
    fn syscall_workload(n: u64) {
        for _ in 0..n {
            // SAFETY: FFI call to close(2); -1 is an invalid fd, so it just
            // returns EBADF — no resource is touched.
            unsafe {
                libc::close(black_box(-1));
            }
        }
    }
}

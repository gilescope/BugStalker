// SPDX-License-Identifier: MIT
//! Darwin Tier 1b — polling cycles+IP sampler.
//!
//! Two paths to cycles+IP sampling on macOS:
//!
//! - **Tier 1a (kperf)** — Apple's PMU. Zero debuggee overhead, but
//!   `kpc_force_all_ctrs_set` requires the
//!   `com.apple.private.kpc.read-or-trace` entitlement. On a
//!   stock-signed `bs` binary this fails with EPERM; the kperf
//!   probe in [`super::kperf`] surfaces that diagnosis. Useful when
//!   bs is run with the entitlement; not the default path.
//! - **Tier 1b (poll)** — *this module*. Periodically suspends each
//!   debuggee thread via `task_threads` + `thread_suspend` /
//!   `thread_get_state` / `thread_resume`, captures the user PC,
//!   resumes. Needs no entitlement beyond `task_for_pid`, which
//!   BugStalker already requires for everything else on macOS. ~5%
//!   debuggee overhead at the default 1 kHz tick — the same
//!   trade-off `samply` makes for the same reason.
//!
//! The poll sampler is the macOS default path. The kperf path is a
//! future opt-in for users who codesign with the kpc entitlement.
//!
//! ### What this module gives you
//!
//! - [`PollSampler::new`] / [`PollSampler::start`] /
//!   [`PollSampler::stop_and_drain`]: lifecycle.
//! - [`PollSampler::sample_once`]: a single, synchronous pass over
//!   every debuggee thread — useful for unit testing without a
//!   background loop.
//!
//! ### What this module deliberately does NOT do
//!
//! - **Source resolution.** The sampler returns raw PCs. Mapping
//!   them to `(file, line)` is the DAP session's job because it
//!   already owns the `.debug_line` resolver and the dyld load
//!   slide. Keeps `bs-perf` free of bugstalker's mach-image
//!   plumbing.
//! - **Stack walking.** Each sample is one PC, not a callstack.
//!   Flame graphs are a follow-up — the gutter heat-map only needs
//!   per-line PCs and that's all we collect here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_init::mach_thread_self;
use mach2::mach_port::mach_port_deallocate;
use mach2::mach_types::{thread_act_array_t, thread_act_t};
use mach2::message::mach_msg_type_number_t;
use mach2::port::mach_port_t;
use mach2::task::task_threads;
use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
use mach2::traps::mach_task_self;
use mach2::vm::mach_vm_deallocate;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

use crate::PerfError;

/// Default polling period — 1 kHz, matching samply.
pub const DEFAULT_POLL_PERIOD: Duration = Duration::from_millis(1);

/// One raw PC sample. The DAP session attributes these to source
/// frames via its own DWARF + dyld-slide pipeline.
pub type RawPc = u64;

/// AArch64 PAC strip mask. Apple Silicon signs pointers with the
/// top bits of the virtual address; the hardware strips them at
/// load but the value saved into `arm_thread_state64.__pc` retains
/// them. 47-bit virtual addresses are the standard on macOS 13+;
/// stripping with this mask matches samply's behaviour for the
/// common case and falls back to "samples don't resolve" if the
/// host's VA size is larger. The DAP layer reports unresolved
/// samples explicitly, so the failure mode is visible.
#[cfg(target_arch = "aarch64")]
const PAC_MASK_AARCH64: u64 = 0x0000_7FFF_FFFF_FFFF;

/// Background polling sampler. Owned per `begin_perf_run`.
pub struct PollSampler {
    task: mach_port_t,
    period: Duration,
    samples: Arc<Mutex<Vec<RawPc>>>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Best-effort count of thread snapshots that failed because the
    /// kernel rejected `thread_suspend` / `thread_get_state` —
    /// typically because the thread exited mid-sample. The DAP
    /// layer surfaces this as "unresolved".
    failed_snapshots: Arc<Mutex<u64>>,
}

impl PollSampler {
    /// Build a sampler bound to `task`. `task` must be a Mach send
    /// right the caller already owns; the sampler does NOT take
    /// ownership — the caller continues to own the port for the
    /// rest of its lifecycle. Defaults to [`DEFAULT_POLL_PERIOD`]
    /// if `period` is zero.
    pub fn new(task: mach_port_t, period: Duration) -> Self {
        let period = if period.is_zero() {
            DEFAULT_POLL_PERIOD
        } else {
            period
        };
        Self {
            task,
            period,
            samples: Arc::new(Mutex::new(Vec::new())),
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            failed_snapshots: Arc::new(Mutex::new(0)),
        }
    }

    /// Spawn the background sampling thread. Returns immediately;
    /// the thread keeps going until [`Self::stop_and_drain`] is
    /// called.
    pub fn start(&mut self) -> Result<(), PerfError> {
        let task = self.task;
        let period = self.period;
        let samples = self.samples.clone();
        let stop_flag = self.stop_flag.clone();
        let failed_snapshots = self.failed_snapshots.clone();
        let handle = thread::Builder::new()
            .name("bs-perf-poll-darwin".to_owned())
            .spawn(move || sampler_loop(task, period, samples, stop_flag, failed_snapshots))
            .map_err(|err| {
                PerfError::Open(std::io::Error::other(format!(
                    "spawn bs-perf-poll-darwin: {err}"
                )))
            })?;
        self.handle = Some(handle);
        Ok(())
    }

    /// Take one pass over the target task's threads now, on the
    /// calling thread. Returns the number of PCs pushed. The
    /// calling thread is filtered out so this is safe to call even
    /// when `task` is the caller's own task — useful for tests.
    pub fn sample_once(&self) -> usize {
        // SAFETY: mach_thread_self always returns a valid send right.
        let own_thread = unsafe { mach_thread_self() };
        let pushed = sample_pass(self.task, own_thread, &self.samples, &self.failed_snapshots);
        // SAFETY: own_thread came from mach_thread_self; balance.
        let _ = unsafe { mach_port_deallocate(mach_task_self(), own_thread) };
        pushed
    }

    /// Stop the background thread, join it, and return every PC
    /// captured so far. Consumes the sampler — restart from a
    /// fresh `new`.
    pub fn stop_and_drain(mut self) -> PollDrain {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let samples = {
            let mut guard = self.samples.lock().expect("samples mutex");
            std::mem::take(&mut *guard)
        };
        let failed_snapshots = {
            let guard = self.failed_snapshots.lock().expect("failed mutex");
            *guard
        };
        PollDrain {
            samples,
            failed_snapshots,
        }
    }
}

/// Result of [`PollSampler::stop_and_drain`].
#[derive(Debug, Clone)]
pub struct PollDrain {
    /// Raw PC samples captured during the run.
    pub samples: Vec<RawPc>,
    /// Thread snapshots the kernel rejected mid-sample (thread
    /// exited, port stale, etc.). The DAP layer reports these as
    /// unresolved.
    pub failed_snapshots: u64,
}

fn sampler_loop(
    task: mach_port_t,
    period: Duration,
    samples: Arc<Mutex<Vec<RawPc>>>,
    stop_flag: Arc<AtomicBool>,
    failed_snapshots: Arc<Mutex<u64>>,
) {
    // SAFETY: mach_thread_self returns a stable send right for the
    // calling thread. We capture it once for the lifetime of the
    // sampler so we can recognise (and skip) ourselves if the
    // target task happens to include the sampler thread — which
    // happens any time the caller mistakenly passes
    // `mach_task_self()` as the target, e.g. in tests. Without
    // this filter the sampler would suspend itself and deadlock.
    let own_thread = unsafe { mach_thread_self() };

    while !stop_flag.load(Ordering::Relaxed) {
        sample_pass(task, own_thread, &samples, &failed_snapshots);
        thread::sleep(period);
    }

    // SAFETY: own_thread was acquired via mach_thread_self; balance
    // its send right at shutdown.
    let _ = unsafe { mach_port_deallocate(mach_task_self(), own_thread) };
}

/// One pass: enumerate threads, snapshot each, push PCs. Returns
/// the number of PCs pushed in this pass. Skips `own_thread` —
/// suspending the sampler's own thread would deadlock.
fn sample_pass(
    task: mach_port_t,
    own_thread: thread_act_t,
    samples: &Mutex<Vec<RawPc>>,
    failed_snapshots: &Mutex<u64>,
) -> usize {
    let threads = match collect_task_threads(task) {
        Ok(threads) => threads,
        Err(_) => {
            // task gone (process exited) — caller's stop loop will
            // notice. Returning early avoids hammering a dead port.
            return 0;
        }
    };

    let mut pushed = 0;
    let mut failures: u64 = 0;
    for thread_act in &threads {
        if *thread_act == own_thread {
            // Self-suspend would deadlock. Always skip — the
            // sampler thread is never an interesting sample.
        } else {
            match sample_one_thread(*thread_act) {
                Some(pc) => {
                    if let Ok(mut buf) = samples.lock() {
                        buf.push(pc);
                        pushed += 1;
                    }
                }
                None => {
                    failures += 1;
                }
            }
        }
        // Drop the thread send right we got from task_threads — the
        // kernel allocated it for us, the kernel won't reclaim it
        // unless we deallocate.
        // SAFETY: `*thread_act` is a port we own from task_threads.
        let _ = unsafe { mach_port_deallocate(mach_task_self(), *thread_act) };
    }

    if failures != 0
        && let Ok(mut guard) = failed_snapshots.lock()
    {
        *guard = guard.saturating_add(failures);
    }
    pushed
}

/// Wrap `task_threads` and deallocate the returned array's backing
/// VM. Returns the thread send rights as a `Vec`.
fn collect_task_threads(task: mach_port_t) -> Result<Vec<thread_act_t>, i32> {
    let mut list: thread_act_array_t = std::ptr::null_mut();
    let mut count: mach_msg_type_number_t = 0;
    // SAFETY: `&mut list` and `&mut count` are valid for the syscall.
    let kr = unsafe { task_threads(task, &mut list, &mut count) };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }
    let n = count as usize;
    // SAFETY: kernel returned KERN_SUCCESS with `list`/`count`
    // pointing at an allocation of `count` send rights.
    let slice = unsafe { std::slice::from_raw_parts(list, n) };
    let out = slice.to_vec();
    // Free the kernel-allocated array. The send rights inside are
    // copied above and individually deallocated by the caller.
    let array_bytes = (n * std::mem::size_of::<thread_act_t>()) as mach_vm_size_t;
    // SAFETY: `list` came from the kernel with the byte size above.
    let _ = unsafe { mach_vm_deallocate(mach_task_self(), list as mach_vm_address_t, array_bytes) };
    Ok(out)
}

fn sample_one_thread(thread: thread_act_t) -> Option<RawPc> {
    // SAFETY: thread_suspend takes a valid send right; the kernel
    // tells us if it isn't, via a non-success kr.
    let kr = unsafe { thread_suspend(thread) };
    if kr != KERN_SUCCESS {
        return None;
    }
    let pc = read_thread_pc(thread);
    // SAFETY: thread_resume balances the thread_suspend above.
    let _ = unsafe { thread_resume(thread) };
    pc
}

#[cfg(target_arch = "aarch64")]
fn read_thread_pc(thread: thread_act_t) -> Option<RawPc> {
    use mach2::structs::arm_thread_state64_t;
    use mach2::thread_status::thread_state_t;
    const ARM_THREAD_STATE64: u32 = 6;

    // SAFETY: zeroed init is valid for arm_thread_state64_t; the
    // kernel fills it on success.
    let mut state: arm_thread_state64_t = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<arm_thread_state64_t>() / 4) as mach_msg_type_number_t;
    // SAFETY: state outlives the call; the kernel writes exactly
    // `count` u32s of state.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64 as i32,
            &mut state as *mut _ as thread_state_t,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return None;
    }
    Some(state.__pc & PAC_MASK_AARCH64)
}

#[cfg(target_arch = "x86_64")]
fn read_thread_pc(thread: thread_act_t) -> Option<RawPc> {
    use mach2::structs::x86_thread_state64_t;
    use mach2::thread_status::thread_state_t;
    const X86_THREAD_STATE64: u32 = 4;

    // SAFETY: zeroed init is valid for x86_thread_state64_t.
    let mut state: x86_thread_state64_t = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<x86_thread_state64_t>() / 4) as mach_msg_type_number_t;
    // SAFETY: state outlives the call.
    let kr = unsafe {
        thread_get_state(
            thread,
            X86_THREAD_STATE64 as i32,
            &mut state as *mut _ as thread_state_t,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return None;
    }
    Some(state.__rip)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn read_thread_pc(_: thread_act_t) -> Option<RawPc> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mach2::traps::task_for_pid;
    use std::process::{Command, Stdio};

    /// Lifecycle smoke test against `mach_task_self()`. The self-
    /// filter in `sample_pass` ensures we never suspend our own
    /// thread, so this is safe. We don't assert on sample count —
    /// other test threads (e.g. cargo test's worker pool) may or
    /// may not exist, and on a single-test-thread run there might
    /// be no other threads to sample. We only assert the lifecycle
    /// completes without deadlock.
    #[test]
    fn start_then_stop_terminates_within_a_few_periods() {
        // SAFETY: mach_task_self always returns a valid task port.
        let task = unsafe { mach_task_self() };
        let mut sampler = PollSampler::new(task, Duration::from_millis(2));
        sampler.start().expect("start sampler");
        std::thread::sleep(Duration::from_millis(20));
        let _drain = sampler.stop_and_drain();
        // Pass if we got here at all: no deadlock, no panic.
    }

    /// Sampler against `task = 0` (an invalid port) — every pass
    /// short-circuits on `collect_task_threads` failure. Verifies
    /// the sampler stays responsive when its target dies (or never
    /// existed).
    #[test]
    fn sampler_on_invalid_task_drains_to_empty() {
        let mut sampler = PollSampler::new(0, Duration::from_millis(1));
        sampler.start().expect("start sampler");
        std::thread::sleep(Duration::from_millis(10));
        let drain = sampler.stop_and_drain();
        assert!(drain.samples.is_empty());
    }

    /// End-to-end pipeline test using our own task as the target.
    /// Spins up an in-process hot loop on a worker thread, runs
    /// the sampler against `mach_task_self()`, asserts at least
    /// one PC came back. The self-filter ensures the sampler
    /// doesn't suspend itself; the hot-loop thread is the prime
    /// target for samples.
    ///
    /// This is the closest unit-test approximation of "sampler
    /// captures real PCs from a real running thread" we can do
    /// without codesigning the test binary.
    #[test]
    fn sampler_against_in_process_hot_loop_captures_pcs() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let worker = std::thread::spawn(move || {
            // A simple side-effect-free hot loop. `black_box` keeps
            // the optimiser from collapsing this into a no-op.
            let mut sink: u64 = 0;
            while !stop_clone.load(Ordering::Relaxed) {
                for i in 0..10_000_u64 {
                    sink = sink.wrapping_add(i);
                }
                std::hint::black_box(&sink);
            }
        });

        // Give the worker a moment to be scheduled on a CPU.
        std::thread::sleep(Duration::from_millis(5));

        // SAFETY: mach_task_self always returns a valid task port.
        let task = unsafe { mach_task_self() };
        let mut sampler = PollSampler::new(task, Duration::from_millis(1));
        sampler.start().expect("start sampler");
        std::thread::sleep(Duration::from_millis(40));
        let drain = sampler.stop_and_drain();

        stop.store(true, Ordering::Relaxed);
        worker.join().expect("worker join");

        assert!(
            !drain.samples.is_empty(),
            "expected at least one PC from the hot-loop thread; drained {} samples, \
             {} failed snapshots",
            drain.samples.len(),
            drain.failed_snapshots
        );
    }

    /// Real cross-process target: spawn `sleep 1`, take a snapshot,
    /// expect at least one PC. Requires `task_for_pid` access —
    /// skipped (with an explanatory message) if the test binary
    /// isn't codesigned with the debugger entitlement.
    #[test]
    fn sample_external_task_when_entitled() {
        let mut child = match Command::new("/bin/sleep")
            .arg("2")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(err) => {
                eprintln!("spawn sleep failed: {err}; skipping test");
                return;
            }
        };
        let pid = child.id() as i32;
        // SAFETY: task_for_pid takes the current task, a pid, and
        // a *mut task_t out-param.
        let mut task: mach_port_t = 0;
        let kr = unsafe { task_for_pid(mach_task_self(), pid, &mut task) };
        if kr != KERN_SUCCESS {
            eprintln!(
                "task_for_pid(pid={pid}) returned kr={kr} — test binary likely lacks \
                 com.apple.security.cs.debugger entitlement; skipping"
            );
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        let sampler = PollSampler::new(task, Duration::from_millis(1));
        let pushed = sampler.sample_once();
        let drain = sampler.stop_and_drain();
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(pushed, drain.samples.len(), "pushed must match drained");
        // `sleep` has at least one thread that we should sample.
        assert!(
            !drain.samples.is_empty() || drain.failed_snapshots > 0,
            "expected at least one sample or one recorded failure from external sleep task"
        );
    }
}

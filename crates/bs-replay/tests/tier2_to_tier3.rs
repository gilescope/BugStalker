// SPDX-License-Identifier: MIT
//! End-to-end Tier 2 → Tier 3 bridge test (Linux only).
//!
//! Exercises every Phase 5 layer in one pass:
//!
//! 1. Tier 2 capture: fork+SIGSTOP+SEIZE+memory+regs into a
//!    `Tier2Capture`.
//! 2. Tier 2 → bytes: `tier2::to_payload`.
//! 3. Tier 3 storage: `TraceWriter::take_checkpoint` stashes the
//!    payload in a real on-disk trace directory.
//! 4. Trace round-trip: close, re-open via `TraceReader`.
//! 5. Tier 3 → bytes: read the checkpoint payload back.
//! 6. Tier 2 ← bytes: `tier2::from_payload` decodes into a
//!    fresh `Tier2State`.
//! 7. Fresh-fork restore: take + SEIZE a new fork, restore the
//!    decoded state, verify bytes + registers match the
//!    originally-captured A.

// x86_64-specific (uses libc::user_regs_struct fields directly).
// The aarch64 path validates via RegisterState::bytes equality
// in the inline tests; this end-to-end test stays x86-only
// pending a layout-agnostic field-flip helper.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use bs_replay::linux::fork_self::LinuxForkSelfMechanism;
use bs_replay::linux::proc_mem::{read_bytes_at, write_bytes_at};
use bs_replay::linux::proc_regs::capture_registers;
use bs_replay::linux::tier2::{self, Tier2Capture};
use bs_replay::ring::CheckpointMechanism;

use bs_replay_engine::format::version::FormatVersion;
use bs_replay_engine::format::{Manifest, TraceReader, TraceWriter};

fn manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "test".to_owned(),
        cpu_features: vec![],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec![],
        recorded_at: None,
        initial_fds: vec![],
    }
}

fn temp_trace_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bs-replay-bridge-{label}-{}", std::process::id(),));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn skip_if_yama<E: std::fmt::Debug>(e: &E) -> bool {
    let s = format!("{e:?}");
    s.contains("EPERM") || s.contains("EACCES")
}

#[test]
fn tier2_capture_through_tier3_storage_round_trip() {
    // Heap-allocate a buffer the parent owns. Both forks see it
    // at the same VA (post-fork COW invariant); both initial
    // copies have all-zero contents.
    let buf: Vec<u8> = vec![0u8; 128];
    let addr = buf.as_ptr() as u64;

    let mut mech = LinuxForkSelfMechanism::new();

    // === Tier 2: capture A ===
    let a = match Tier2Capture::capture(&mut mech, 1) {
        Ok(c) => c,
        Err(e) if skip_if_yama(&e) => {
            eprintln!("skipping bridge test: tier2 capture: {e:?}");
            return;
        }
        Err(e) => panic!("tier2 capture failed: {e:?}"),
    };
    let a_pid = a.handle.pid;
    let a_bytes_at_addr = read_bytes_at(a_pid, addr, buf.len()).expect("read A buf");
    assert!(a_bytes_at_addr.iter().all(|&b| b == 0));
    // Step 105 changed RegisterState to wrap a Vec<u8> blob
    // (cross-arch); compare the blobs directly rather than
    // reaching into the old user_regs_struct layout.
    let a_regs_bytes = a.state.regs.bytes.clone();

    // === Tier 3: write trace + stash A's payload as a checkpoint ===
    let dir = temp_trace_dir("e2e-bridge");
    let payload = tier2::to_payload(&a.state);
    {
        let mut w = TraceWriter::create(&dir, &manifest()).expect("create");
        w.take_checkpoint(payload.clone())
            .expect("checkpoint write");
        w.finish().expect("finish");
    }

    // === Tier 3: re-open trace, read checkpoint payload ===
    let read_payload = {
        let r = TraceReader::open(&dir).expect("reader open");
        let cp = r.open_checkpoint(1).expect("open checkpoint 1");
        cp.payload.clone()
    };
    assert_eq!(
        read_payload, payload,
        "Tier 3 storage corrupted the payload"
    );

    // === Tier 2 decode ===
    let decoded = tier2::from_payload(&read_payload).expect("decode failed");

    // === Fresh fork B, restore decoded state into it ===
    let b_handle = mech.take(2).expect("fork B failed");
    sleep(Duration::from_millis(50));
    if let Err(e) = mech.seize(&b_handle) {
        if skip_if_yama(&e) {
            eprintln!("skipping bridge test: seize B: {e:?}");
            a.kill(&mut mech).expect("kill A");
            mech.kill(b_handle).expect("kill B");
            return;
        }
        panic!("seize B failed: {e:?}");
    }

    // Perturb B at addr so we can prove the restore actually
    // brought A's bytes back.
    let sentinel = vec![0xee; 128];
    write_bytes_at(b_handle.pid, addr, &sentinel).expect("write sentinel");

    // The restore path is symmetric to Tier2Capture::restore_into,
    // but the decoded payload didn't reconstruct a Tier2Capture
    // (no fork handle to attach to) — so call the lower-level
    // restore primitives directly.
    use bs_replay::linux::checkpoint_capture::restore_writable_state;
    use bs_replay::linux::proc_regs::restore_registers;
    let report = restore_writable_state(b_handle.pid, &decoded.writable).expect("restore mem");
    assert!(
        report.written > 0,
        "restore wrote nothing — payload was empty?"
    );
    restore_registers(b_handle.pid, &decoded.regs).expect("restore regs");

    // === Verify B now matches A ===
    let b_post = read_bytes_at(b_handle.pid, addr, 128).expect("read B post");
    assert_eq!(
        b_post, a_bytes_at_addr,
        "B's bytes at addr should match A's after Tier 2 → 3 → 2 round-trip",
    );
    let b_regs = capture_registers(b_handle.pid).expect("capture B regs");
    let b_regs_bytes = b_regs.bytes.clone();
    assert_eq!(
        a_regs_bytes, b_regs_bytes,
        "B's regs should match A's after the bridge round-trip",
    );

    // Parent's view of buf still all-zero (COW held throughout).
    assert!(buf.iter().all(|&b| b == 0));

    a.kill(&mut mech).expect("kill A");
    mech.kill(b_handle).expect("kill B");
    fs::remove_dir_all(&dir).ok();
}

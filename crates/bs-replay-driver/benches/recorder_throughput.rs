// SPDX-License-Identifier: MIT
//! Recorder throughput benchmark.
//!
//! Measures `record_program` end-to-end against `/bin/true`:
//! how many full record sessions (fork + exec + ptrace loop +
//! trace finalise) per second can the supervisor drive.
//!
//! Linux-only — the recorder needs ptrace + seccomp NOTIF. On
//! Darwin the bench is a no-op stub so `cargo bench --workspace`
//! still works.
//!
//! Establishes a baseline so future regressions surface in CI.
//!
//! ## How to run
//!
//! ```text
//! cargo bench -p bs-replay-driver --bench recorder_throughput
//! ```
//!
//! ## What we expect
//!
//! `/bin/true` does ~30–40 syscalls (libc startup + exit_group).
//! Each syscall = recv_notif → respond_continue → wait_for_exit
//! → 2 PTRACE_GETREGS + 2 process_vm_readv (libc ABI capture).
//! Steady-state recorder throughput on modern x86 is in the
//! ~10–50 sessions/sec range; the headline number is per-syscall
//! overhead (1 / (sessions/sec × syscalls/session)).

use criterion::{Criterion, criterion_group, criterion_main};

#[cfg(target_os = "linux")]
fn bench_record_bin_true(c: &mut Criterion) {
    use std::ffi::CString;
    use std::path::PathBuf;
    use std::time::Duration;

    use bs_replay_driver::engine::format::manifest::Manifest;
    use bs_replay_driver::engine::format::version::FormatVersion;
    use bs_replay_driver::{RecordOptions, record_program};

    let prog = std::path::Path::new("/bin/true");
    if !prog.exists() {
        eprintln!("recorder_throughput: skipping — /bin/true not present");
        return;
    }

    fn manifest() -> Manifest {
        Manifest {
            format_version: FormatVersion::V1,
            build_id: "deadbeef".repeat(8),
            kernel_release: "bench".to_owned(),
            cpu_features: vec![],
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            initial_env: vec![],
            initial_cwd: "/tmp".to_owned(),
            initial_args: vec![],
            recorded_at: None,
            initial_fds: vec![],
        }
    }

    fn temp_dir(seq: usize) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bs-recorder-bench-{}-{seq}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // Probe once to confirm the recorder works on this host;
    // skip the bench (with a clear note) if it doesn't.
    let probe_dir = temp_dir(0);
    let probe = record_program(
        &probe_dir,
        &manifest(),
        vec![CString::new("/bin/true").unwrap()],
        vec![CString::new("PATH=/bin").unwrap()],
        RecordOptions::default(),
    );
    let _ = std::fs::remove_dir_all(&probe_dir);
    if let Err(e) = probe {
        eprintln!("recorder_throughput: skipping — probe failed (kernel/perms?): {e}");
        return;
    }

    let mut group = c.benchmark_group("recorder");
    // Recording is heavy (~tens of ms per session); a long
    // measurement time + small sample size keeps Criterion's
    // CI cost reasonable.
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(20);

    let mut session_seq = 1usize;
    group.bench_function("record_bin_true", |b| {
        b.iter_with_setup(
            || {
                let dir = temp_dir(session_seq);
                session_seq += 1;
                dir
            },
            |dir| {
                let _ = record_program(
                    &dir,
                    &manifest(),
                    vec![CString::new("/bin/true").unwrap()],
                    vec![CString::new("PATH=/bin").unwrap()],
                    RecordOptions::default(),
                );
                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    });
    group.finish();
}

#[cfg(not(target_os = "linux"))]
fn bench_record_bin_true(_c: &mut Criterion) {
    eprintln!(
        "recorder_throughput: bench is Linux-only (recorder needs ptrace+seccomp); \
         skipping on this platform."
    );
}

criterion_group!(benches, bench_record_bin_true);
criterion_main!(benches);

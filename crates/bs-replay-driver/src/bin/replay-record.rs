// SPDX-License-Identifier: MIT
//! `replay-record` — drive [`bs_replay_driver::record_program`]
//! against a user program.
//!
//! ```text
//! replay-record [OPTIONS] <TRACE_DIR> -- <PROGRAM> [ARG ...]
//! ```
//!
//! Linux only — the recorder needs ptrace. On Darwin the binary
//! still builds (the cfg gate compiles a stub `main` that
//! reports the platform limitation cleanly).
//!
//! Exit codes:
//! - 0  recording succeeded; tracee exited normally with code 0
//! - 1  recorder error / tracee died abnormally / disk full
//! - 2  argv parse error
//! - 3  permission / kernel doesn't support the recorder (skip)
//! - other  tracee exited with a non-zero code (echoed verbatim)

use std::process::ExitCode;

const USAGE: &str = "\
replay-record — record a program's syscalls into a Phase 5 trace.

Usage:
    replay-record [OPTIONS] <TRACE_DIR> -- <PROGRAM> [ARG ...]

Options:
    --max-iterations <N>     Hard cap on recorder loop iterations.
                             Defaults to 2_000_000 (~30 min @ 1 kHz).
    --build-id <hex>         Stamp this value into manifest.build_id
                             (otherwise a deadbeef placeholder).
    --label <text>           Human-readable label stamped into
                             manifest.kernel_release for diagnostics.
    --patch-vdso             At startup, patch the tracee's vDSO so
                             libc's gettimeofday/clock_gettime/time/
                             getcpu fast paths route through real
                             syscalls and trip the recorder. Off by
                             default — opt in if your program uses
                             time-related calls and you need them
                             captured.
    --trap-tsc               Set PR_SET_TSC=PR_TSC_SIGSEGV in the
                             tracee so RDTSC/RDTSCP raise SIGSEGV
                             and the recorder emits Event::InstructionTrap
                             for them. Off by default — some libc
                             versions probe RDTSC at startup, opt-in
                             keeps the default robust.
    --overwrite              If TRACE_DIR already exists, remove it
                             and start a fresh recording. Default
                             behaviour refuses (a stale segment from
                             a prior run could silently corrupt a
                             replay).
    -h, --help               Show this help.

The TRACE_DIR must not already exist; the recorder refuses to
overwrite an existing directory (a stale segment from a prior
run could silently corrupt a replay).

Exit codes:
    0    recording OK, tracee exited 0
    1    recorder error / tracee died abnormally
    2    argv parse error
    3    permission / kernel skip
    >0   tracee exited with non-zero code (echoed)
";

#[cfg(target_os = "linux")]
mod linux_main {
    use std::ffi::CString;
    use std::process::ExitCode;

    use bs_replay_driver::engine::format::manifest::Manifest;
    use bs_replay_driver::engine::format::version::FormatVersion;
    use bs_replay_driver::{
        record_program, RecordOptions, RecordProgramError, RecorderExitStatus,
    };

    use super::USAGE;

    #[derive(Default)]
    pub struct Cli {
        pub trace_dir: Option<String>,
        pub argv: Vec<String>,
        pub max_iterations: Option<u64>,
        pub build_id: Option<String>,
        pub label: Option<String>,
        pub patch_vdso: bool,
        pub trap_tsc: bool,
        pub overwrite: bool,
    }

    pub fn parse() -> Result<Cli, String> {
        let mut cli = Cli::default();
        let mut args = std::env::args().skip(1);
        let mut seen_separator = false;
        while let Some(a) = args.next() {
            if seen_separator {
                cli.argv.push(a);
                continue;
            }
            match a.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "--" => seen_separator = true,
                "--max-iterations" => {
                    let v = args.next().ok_or_else(|| {
                        "--max-iterations requires a u64 argument".to_owned()
                    })?;
                    let n: u64 = v.parse().map_err(|e| {
                        format!("--max-iterations `{v}`: not a u64 ({e})")
                    })?;
                    cli.max_iterations = Some(n);
                }
                "--build-id" => {
                    cli.build_id = Some(args.next().ok_or_else(|| {
                        "--build-id requires a hex argument".to_owned()
                    })?);
                }
                "--label" => {
                    cli.label = Some(args.next().ok_or_else(|| {
                        "--label requires a text argument".to_owned()
                    })?);
                }
                "--patch-vdso" => cli.patch_vdso = true,
                "--trap-tsc" => cli.trap_tsc = true,
                "--overwrite" => cli.overwrite = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag: {other}"));
                }
                _ => {
                    if cli.trace_dir.is_some() {
                        return Err(format!(
                            "unexpected positional `{a}` — use `--` to separate \
                             recorder flags from the tracee command"
                        ));
                    }
                    cli.trace_dir = Some(a);
                }
            }
        }
        if cli.trace_dir.is_none() {
            return Err("missing TRACE_DIR argument".to_owned());
        }
        if cli.argv.is_empty() {
            return Err(
                "missing program to record — pass it after `--` (e.g. \
                 `replay-record /tmp/trace.bs -- /bin/cat /etc/hostname`)"
                    .to_owned(),
            );
        }
        Ok(cli)
    }

    fn build_manifest(cli: &Cli) -> Manifest {
        Manifest {
            format_version: FormatVersion::V1,
            build_id: cli.build_id.clone().unwrap_or_else(|| "deadbeef".repeat(8)),
            kernel_release: cli
                .label
                .clone()
                .unwrap_or_else(|| "replay-record".to_owned()),
            cpu_features: vec![],
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            // POSIX disallows `=` in env keys; the format crate's
            // serialiser would refuse those rows. Filter belt-and-
            // braces.
            initial_env: std::env::vars()
                .filter(|(k, _)| !k.contains('='))
                .collect(),
            initial_cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_owned()),
            initial_args: cli.argv.clone(),
            // Auto-stamped — RFC 3339 / UTC. Same path as
            // bs_replay_driver::capture::capture_host_manifest.
            recorded_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }

    fn into_cstrings(args: &[String]) -> Result<Vec<CString>, String> {
        args.iter()
            .map(|s| {
                CString::new(s.as_str())
                    .map_err(|e| format!("argv contains a NUL byte: {e}"))
            })
            .collect()
    }

    fn current_envp() -> Result<Vec<CString>, String> {
        std::env::vars()
            .map(|(k, v)| {
                CString::new(format!("{k}={v}")).map_err(|e| {
                    format!("env var `{k}` contains a NUL byte: {e}")
                })
            })
            .collect()
    }

    fn is_skip(err: &RecordProgramError) -> bool {
        let s = format!("{err}");
        s.contains("EPERM")
            || s.contains("EACCES")
            || s.contains("ENOSYS")
            || s.contains("yama")
            || s.contains("64")
            || s.contains("65")
            || s.contains("66")
    }

    pub fn run(cli: Cli) -> ExitCode {
        let manifest = build_manifest(&cli);
        let argv = match into_cstrings(&cli.argv) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("replay-record: {e}");
                return ExitCode::from(2);
            }
        };
        let envp = match current_envp() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("replay-record: {e}");
                return ExitCode::from(2);
            }
        };
        let options = RecordOptions {
            max_iterations: cli.max_iterations.unwrap_or(2_000_000),
            patch_vdso: cli.patch_vdso,
            trap_tsc: cli.trap_tsc,
        };
        let trace_dir = cli.trace_dir.as_deref().expect("validated by parse()");

        if cli.overwrite {
            // Best-effort remove; non-existent dir is fine
            // because TraceWriter::create handles that case.
            // Anything else (permission denied, busy mount,
            // …) surfaces as the recorder's first hard error.
            let _ = std::fs::remove_dir_all(trace_dir);
        }

        match record_program(trace_dir, &manifest, argv, envp, options) {
            Ok(report) => {
                eprintln!(
                    "replay-record: {} syscalls / {} signals / {} instr-traps / {} steps",
                    report.syscall_events,
                    report.signal_events,
                    report.instruction_traps,
                    report.iterations,
                );
                match report.exit_status {
                    RecorderExitStatus::Exited(0) => ExitCode::SUCCESS,
                    RecorderExitStatus::Exited(code) => {
                        eprintln!(
                            "replay-record: tracee exited with code {code}"
                        );
                        ExitCode::from(code as u8)
                    }
                    RecorderExitStatus::Signalled(sig) => {
                        eprintln!(
                            "replay-record: tracee was killed by signal {sig}"
                        );
                        ExitCode::FAILURE
                    }
                    RecorderExitStatus::IterationCap(n) => {
                        eprintln!(
                            "replay-record: hit iteration cap at {n} steps; trace truncated"
                        );
                        ExitCode::FAILURE
                    }
                }
            }
            Err(e) if is_skip(&e) => {
                eprintln!("replay-record: skipping — {e}");
                eprintln!("                  the recorder needs Linux >= 5.5 with");
                eprintln!("                  ptrace + (no yama lockdown).");
                ExitCode::from(3)
            }
            Err(e) => {
                eprintln!("replay-record: recorder failed: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    match linux_main::parse() {
        Ok(cli) => linux_main::run(cli),
        Err(e) => {
            eprintln!("replay-record: {e}\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    // --help / -h still works cross-platform so the docs link
    // the user opens isn't a dead end.
    for a in std::env::args().skip(1) {
        if a == "--help" || a == "-h" {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    }
    eprintln!(
        "replay-record: this command is Linux-only. The Phase 5 recorder \
         depends on seccomp / ptrace; the macOS Mach-based path is Tier 2 \
         (checkpoint-only, see `replay-doctor` for trace inspection)."
    );
    ExitCode::from(2)
}

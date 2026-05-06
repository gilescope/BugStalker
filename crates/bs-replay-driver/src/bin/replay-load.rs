// SPDX-License-Identifier: MIT
//! `replay-load` — drive [`bs_replay_driver::replay_program`]
//! against a recorded trace.
//!
//! ```text
//! replay-load [OPTIONS] <TRACE_DIR> -- <PROGRAM> [ARG ...]
//! ```
//!
//! Symmetric to `replay-record`: re-launches `<PROGRAM>` and
//! supplies the recorded results from `<TRACE_DIR>` through
//! the seccomp-NOTIF intercept path.
//!
//! Linux only — replay needs seccomp NOTIF; Darwin gets a stub
//! main with a clear platform message.

use std::process::ExitCode;

const USAGE: &str = "\
replay-load — replay a recorded program from its Phase 5 trace.

Usage:
    replay-load [OPTIONS] <TRACE_DIR> -- <PROGRAM> [ARG ...]

Options:
    --max-iterations <N>     Hard cap on supervisor loop iterations.
                             Defaults to 2_000_000.
    -h, --help               Show this help.

The TRACE_DIR must already exist and have been produced by a
prior `replay-record`. The PROGRAM should be the same binary
that was recorded — a different binary's syscall sequence will
trip the shim's mismatch detector and the replay aborts loudly.

Exit codes:
    0    replay finished cleanly (tracee exited or trace ran out)
    1    shim refused / tracee died abnormally
    2    argv parse error
    3    permission / kernel skip
";

#[cfg(target_os = "linux")]
mod linux_main {
    use std::ffi::CString;
    use std::process::ExitCode;

    use bs_replay_driver::{
        replay_program, ReplayExit, ReplayOptions, ReplayProgramError,
    };

    use super::USAGE;

    #[derive(Default)]
    pub struct Cli {
        pub trace_dir: Option<String>,
        pub argv: Vec<String>,
        pub max_iterations: Option<u64>,
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
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag: {other}"));
                }
                _ => {
                    if cli.trace_dir.is_some() {
                        return Err(format!(
                            "unexpected positional `{a}` — use `--` to separate \
                             replay flags from the tracee command"
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
                "missing program to replay — pass it after `--` (e.g. \
                 `replay-load /tmp/trace.bs -- /bin/cat /etc/hostname`)"
                    .to_owned(),
            );
        }
        Ok(cli)
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

    fn is_skip(err: &ReplayProgramError) -> bool {
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
        let argv = match into_cstrings(&cli.argv) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("replay-load: {e}");
                return ExitCode::from(2);
            }
        };
        let envp = match current_envp() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("replay-load: {e}");
                return ExitCode::from(2);
            }
        };
        let options = ReplayOptions {
            max_iterations: cli.max_iterations.unwrap_or(2_000_000),
        };
        let trace_dir = cli.trace_dir.as_deref().expect("validated by parse()");

        match replay_program(trace_dir, argv, envp, options) {
            Ok(report) => {
                eprintln!(
                    "replay-load: {} syscalls applied / {} signals delivered / \
                     {} signals skipped / {} instr-traps skipped / \
                     {} steps / {} bytes written",
                    report.syscalls_applied,
                    report.signals_delivered,
                    report.signals_skipped,
                    report.instruction_traps_skipped,
                    report.iterations,
                    report.bytes_written,
                );
                match report.exit {
                    Some(ReplayExit::Exited(0)) => ExitCode::SUCCESS,
                    Some(ReplayExit::Exited(code)) => {
                        eprintln!("replay-load: tracee exited with code {code}");
                        ExitCode::from((code as u8).max(1))
                    }
                    Some(ReplayExit::Signalled(sig)) => {
                        eprintln!("replay-load: tracee was killed by signal {sig}");
                        ExitCode::FAILURE
                    }
                    Some(ReplayExit::TraceExhausted { applied }) => {
                        eprintln!(
                            "replay-load: trace ran out after {applied} syscalls; \
                             SIGKILL'd the tracee"
                        );
                        ExitCode::SUCCESS
                    }
                    Some(ReplayExit::ShimRefused(reason)) => {
                        eprintln!(
                            "replay-load: replay shim refused — {reason:?}; \
                             SIGKILL'd the tracee"
                        );
                        ExitCode::FAILURE
                    }
                    Some(ReplayExit::IterationCap(n)) => {
                        eprintln!(
                            "replay-load: iteration cap reached at {n} steps"
                        );
                        ExitCode::FAILURE
                    }
                    None => {
                        eprintln!("replay-load: replay finished without an exit reason");
                        ExitCode::FAILURE
                    }
                }
            }
            Err(e) if is_skip(&e) => {
                eprintln!("replay-load: skipping — {e}");
                ExitCode::from(3)
            }
            Err(e) => {
                eprintln!("replay-load: replay failed: {e}");
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
            eprintln!("replay-load: {e}\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    for a in std::env::args().skip(1) {
        if a == "--help" || a == "-h" {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    }
    eprintln!(
        "replay-load: this command is Linux-only. The Phase 5 replay shim \
         depends on seccomp NOTIF; the macOS path is Tier 2 (checkpoint-only, \
         see `replay-doctor` for trace inspection)."
    );
    ExitCode::from(2)
}

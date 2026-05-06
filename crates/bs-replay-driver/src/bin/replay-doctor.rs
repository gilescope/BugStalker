// SPDX-License-Identifier: MIT
//! `replay-doctor` — wrap `validate_with` in a tool a support
//! engineer can actually run.
//!
//! ```text
//! replay-doctor [--check-host] [--build-id <hex>] <DIR>
//! ```
//!
//! Exits 0 when the trace is replayable, 1 when there are errors,
//! 2 on argument-parse failure.

use std::process::ExitCode;

use bs_replay_driver::dap::{load, ReplayLoadRequest};
use bs_replay_driver::host::host_features;
use bs_replay_engine::format::{validate_with, ValidationOptions};

const USAGE: &str = "\
replay-doctor — validate or summarise a Phase 5 trace directory.

Usage:
    replay-doctor [OPTIONS] <DIR>

Options:
    --check-host             Detect this host's CPU features and
                             flag any the recording used that the
                             host lacks.
    --build-id <hex>         Cross-check the manifest's build_id
                             against this expected value.
    --load                   Skip the full validation report and
                             print just the bs/replayLoad summary
                             (events / segments / checkpoints).
    -h, --help               Show this help.

Exit codes:
    0  trace is replayable / summary printed
    1  trace has errors / load failed
    2  argument parse error
";

#[derive(Default)]
struct Cli {
    dir: Option<String>,
    check_host: bool,
    build_id: Option<String>,
    load: bool,
}

fn parse() -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--check-host" => cli.check_host = true,
            "--load" => cli.load = true,
            "--build-id" => {
                cli.build_id = Some(args.next().ok_or_else(|| {
                    "--build-id requires a hex argument".to_owned()
                })?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => {
                if cli.dir.is_some() {
                    return Err(format!("unexpected positional: {a}"));
                }
                cli.dir = Some(a);
            }
        }
    }
    if cli.dir.is_none() {
        return Err("missing trace directory argument".to_owned());
    }
    Ok(cli)
}

fn main() -> ExitCode {
    let cli = match parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("replay-doctor: {e}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let dir = cli.dir.as_deref().expect("validated by parse()");

    if cli.load {
        match load(&ReplayLoadRequest { trace_path: dir.to_owned() }) {
            Ok((_replayer, resp)) => {
                println!(
                    "{} events / {} segments / {} checkpoints",
                    resp.total_events, resp.total_segments, resp.total_checkpoints,
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("replay-doctor: load failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let host_owned: Option<Vec<String>> = if cli.check_host {
        match host_features() {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("replay-doctor: --check-host requested but host detection failed: {e}");
                eprintln!("                  the host check is being skipped.");
                None
            }
        }
    } else {
        None
    };
    let host_refs: Option<Vec<&str>> = host_owned
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());

    let opts = ValidationOptions {
        expected_build_id: cli.build_id.as_deref(),
        host_features: host_refs.as_deref(),
    };

    let report = validate_with(dir, &opts);
    print!("{report}");
    if report.is_replayable() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

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
use bs_replay_engine::format::event::Event;
use bs_replay_engine::format::{validate_with, TraceReader, ValidationOptions};

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
    --counts                 Print a per-event-kind histogram
                             (Syscall / Signal / InstructionTrap /
                             Marker / PcMarker counts).
    --dump-events [N]        Print the first N events in
                             human-readable form (default 20;
                             pass 0 to dump every event).
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
    counts: bool,
    dump_events: Option<usize>,
}

fn parse() -> Result<Cli, String> {
    let mut cli = Cli::default();
    // Collect to a Vec so we can peek (std::env::Args isn't
    // cloneable, and `--dump-events` takes an optional value).
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = raw.into_iter().peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--check-host" => cli.check_host = true,
            "--load" => cli.load = true,
            "--counts" => cli.counts = true,
            "--dump-events" => {
                // Optional value — peek at the next arg; if it
                // parses as a usize use it, otherwise default
                // to 20 (peekable doesn't consume).
                let n = match args.peek().and_then(|s| s.parse::<usize>().ok()) {
                    Some(n) => {
                        let _ = args.next();
                        n
                    }
                    None => 20,
                };
                cli.dump_events = Some(n);
            }
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
                println!("build-id: {}", resp.build_id);
                if let Some(ts) = &resp.recorded_at {
                    println!("recorded-at: {ts}");
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("replay-doctor: load failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if cli.counts {
        return match print_counts(dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("replay-doctor: --counts failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if let Some(n) = cli.dump_events {
        return match print_dump(dir, n) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("replay-doctor: --dump-events failed: {e}");
                ExitCode::FAILURE
            }
        };
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

// ---------------------------------------------------------------------------
// --counts
// ---------------------------------------------------------------------------

/// Walk every event in the trace, bucket by variant, print the
/// histogram. Heavier than `--load` because it deserialises
/// every event, but produces an actionable per-kind breakdown.
fn print_counts(dir: &str) -> Result<(), String> {
    let reader = TraceReader::open(dir).map_err(|e| format!("{e}"))?;
    let mut cursor = reader.cursor();
    let mut counts = EventCounts::default();
    while let Some(ev) = cursor.next().map_err(|e| format!("{e}"))? {
        counts.bump(&ev);
    }
    println!("event-kind histogram for {dir}:");
    println!("  Syscall          : {}", counts.syscall);
    println!("  Signal           : {}", counts.signal);
    println!("  InstructionTrap  : {}", counts.instruction_trap);
    println!("  Marker           : {}", counts.marker);
    println!("  PcMarker         : {}", counts.pc_marker);
    println!("  ----                  ----");
    println!("  total            : {}", counts.total());
    Ok(())
}

#[derive(Default)]
struct EventCounts {
    syscall: u64,
    signal: u64,
    instruction_trap: u64,
    marker: u64,
    pc_marker: u64,
}
impl EventCounts {
    fn bump(&mut self, ev: &Event) {
        match ev {
            Event::Syscall { .. } => self.syscall += 1,
            Event::Signal { .. } => self.signal += 1,
            Event::InstructionTrap { .. } => self.instruction_trap += 1,
            Event::Marker { .. } => self.marker += 1,
            Event::PcMarker { .. } => self.pc_marker += 1,
        }
    }
    fn total(&self) -> u64 {
        self.syscall + self.signal + self.instruction_trap + self.marker + self.pc_marker
    }
}

// ---------------------------------------------------------------------------
// --dump-events
// ---------------------------------------------------------------------------

/// Print the first `n` events in human-readable form. `n == 0`
/// dumps every event (handy for tiny traces).
fn print_dump(dir: &str, n: usize) -> Result<(), String> {
    let reader = TraceReader::open(dir).map_err(|e| format!("{e}"))?;
    let mut cursor = reader.cursor();
    let mut idx = 0u64;
    while let Some(ev) = cursor.next().map_err(|e| format!("{e}"))? {
        if n > 0 && idx as usize >= n {
            println!("(stopping after {n} events; pass --dump-events 0 for all)");
            break;
        }
        println!("[{idx:>6}] {}", render_event(&ev));
        idx += 1;
    }
    Ok(())
}

fn render_event(ev: &Event) -> String {
    match ev {
        Event::Syscall { nr, args, result, output } => {
            let name = bs_syscall_spec_name(*nr);
            format!(
                "Syscall  nr={nr:<3} {name:<20} args=[{:#x},{:#x},{:#x},{:#x},{:#x},{:#x}] result={result} output={}B",
                args[0], args[1], args[2], args[3], args[4], args[5], output.len(),
            )
        }
        Event::Signal { sig_no, pc, siginfo } => format!(
            "Signal   sig={sig_no:<3} ({}) pc={pc:#x} siginfo={}B",
            sig_name(*sig_no), siginfo.len(),
        ),
        Event::InstructionTrap { pc, kind, result } => format!(
            "Trap     {} pc={pc:#x} result={result:?}",
            instr_name(*kind),
        ),
        Event::Marker { tag, data } => format!("Marker   tag={tag} data={data}"),
        Event::PcMarker { pc } => format!("PcMarker pc={pc:#x}"),
    }
}

fn bs_syscall_spec_name(nr: u32) -> String {
    use bs_replay_driver::engine::record::syscall_capture::{classify, Tier};
    match classify(nr) {
        Tier::Curated(spec) => spec.name.to_owned(),
        Tier::LongTail(g) => g.name.to_owned(),
        Tier::Unknown => format!("syscall_{nr}"),
    }
}

fn sig_name(n: u32) -> &'static str {
    match n as i32 {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        _ => "SIG?",
    }
}

fn instr_name(kind: bs_replay_engine::format::event::InstructionTrapKind) -> &'static str {
    use bs_replay_engine::format::event::InstructionTrapKind as K;
    match kind {
        K::Rdtsc => "RDTSC",
        K::Rdtscp => "RDTSCP",
        K::Rdrand => "RDRAND",
        K::Rdseed => "RDSEED",
        K::Cpuid => "CPUID",
    }
}

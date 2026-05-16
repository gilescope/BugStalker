// SPDX-License-Identifier: MIT
//! Debugger application entry point.

use bugstalker::dap;
use bugstalker::dap::yadap;
use bugstalker::debugger::rust;
use bugstalker::log::LOGGER_SWITCHER;
use bugstalker::ui;
use bugstalker::ui::config::{Theme, UIConfig};
use bugstalker::ui::supervisor::{DebugeeSource, Interface};
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};
use std::fmt::Display;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

/// `--version` output. We carry the build stamp from `build.rs` so
/// `bs --version` is enough to tell a stale `~/.cargo/bin/bs` (or a
/// pre-fix tarball install) from a fresh local build — no mtime
/// archaeology required.
const BS_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BS_BUILD_STAMP"), ")");

#[derive(Parser, Debug, Clone)]
#[command(author, version = BS_VERSION, about, long_about = None)]
pub struct Args {
    /// Start with terminal ui
    #[clap(long)]
    #[arg(default_value_t = false)]
    tui: bool,

    /// Enable Debug Adapter Protocol in stdio mode (embedded in terminal/IDE)
    #[clap(long)]
    #[arg(default_value_t = false, aliases=["dap"])]
    dap_local: bool,

    /// Enable Debug Adapter Protocol in TCP server mode
    #[clap(long)]
    dap_remote: Option<String>,

    /// DAP: exit after first debug session
    #[clap(long)]
    dap_oneshot: bool,

    /// DAP: log file for adapter diagnostics
    #[clap(long)]
    dap_log_file: Option<PathBuf>,

    /// Phase 9 AI-bot scripting front-end: read JSON-RPC 2.0 requests
    /// (JSON5 with comments allowed) on stdin, write responses + events
    /// on stdout. See `doc/scripting/usage.md`.
    #[clap(long)]
    #[arg(default_value_t = false)]
    script: bool,

    /// Test-runner mode: read the given .json5 script, dispatch every
    /// request through the same engine `--script` uses, accumulate
    /// `assert.*` results into TAP 14 on stdout, and exit `0` if every
    /// assertion passed (else `1`). See `doc/scripting/usage.md`.
    #[clap(long, value_name = "SCRIPT")]
    test: Option<PathBuf>,

    /// Combined with `--test`: rewrite mismatched `expect:` blocks in
    /// the script in place using the current responses. Idempotent.
    /// `0x…` addresses are auto-masked into `$regex` placeholders.
    #[clap(long, requires = "test")]
    #[arg(default_value_t = false)]
    bless: bool,

    /// With `--bless` or `--record`: skip the address auto-mask
    /// step. Use when you want a recorded test to assert on a
    /// literal `0x…` value.
    #[clap(long)]
    #[arg(default_value_t = false)]
    no_masks: bool,

    /// Recorder mode: take a JSON-RPC stream on stdin (same wire
    /// format as `--script`), tee every request into the given file
    /// as a runnable test script. Read-only inspections (`var`,
    /// `arg`, `frame.info`) auto-promote to `assert.*` blocks pinned
    /// against the observed response.
    #[clap(long, value_name = "OUT")]
    record: Option<PathBuf>,

    /// Pure metadata mode: write the JSON Schema catalogue of every
    /// scripting method to stdout and exit. Pair with `bs --script` to
    /// drive the debugger from an agent.
    #[clap(long)]
    #[arg(default_value_t = false)]
    describe_commands: bool,

    /// Attach to running process PID
    #[clap(long, short)]
    pid: Option<i32>,

    #[clap(long)]
    cwd: Option<PathBuf>,

    /// Executable file (debugee)
    debugee: Option<String>,

    /// Path to rust stdlib
    #[clap(short, long)]
    std_lib_path: Option<String>,

    /// Discover a specific oracle (maybe more than one)
    #[clap(short, long)]
    oracle: Vec<String>,

    /// Arguments are passed to debugee
    #[arg(raw(true))]
    args: Vec<String>,

    /// Theme used for visualize code and variables.
    /// Available themes: none, inspired_github, solarized_dark, solarized_light, base16_eighties_dark
    /// base16_mocha_dark, base16_ocean_dark, base16_ocean_light
    #[clap(short, long)]
    #[arg(default_value = "solarized_dark")]
    theme: String,

    /// Path to TUI keymap file [default: ~/.config/bs/keymap.toml]
    #[clap(long, env)]
    keymap_file: Option<String>,

    // Retain command history between sessions.
    #[clap(long, env)]
    #[arg(default_value_t = false)]
    save_history: bool,
}

fn print_fatal_and_exit(kind: ErrorKind, message: impl Display) -> ! {
    let mut cmd = Args::command();
    _ = cmd.error(kind, message).print();
    exit(1);
}

trait FatalResult<T> {
    fn unwrap_or_exit(self, kind: ErrorKind, message: impl Display) -> T;
}

impl<T, E: Display> FatalResult<T> for Result<T, E> {
    fn unwrap_or_exit(self, kind: ErrorKind, message: impl Display) -> T {
        match self {
            Ok(ok) => ok,
            Err(err) => print_fatal_and_exit(kind, format!("{message}: {err:#}")),
        }
    }
}

impl From<&Args> for UIConfig {
    fn from(args: &Args) -> Self {
        Self {
            theme: Theme::from_str(&args.theme)
                .unwrap_or_exit(ErrorKind::InvalidValue, "Not an available theme"),
            tui_keymap: ui::tui::config::KeyMap::from_file(args.keymap_file.as_deref())
                .unwrap_or_default(),
            save_history: args.save_history,
        }
    }
}

/// Print a one-line warning to stderr when bs starts on a macOS
/// kernel known to force-reboot under bs's mach syscall traffic.
/// Three byte-identical kernel panics captured on
/// `xnu-12377.101.15 / 25E253` so far; see
/// `doc/macos-26.4.1-panic-risk.md`. Suppress with
/// `BS_DARWIN_PANIC_WARN=0` once you've internalised the risk.
fn warn_macos_panic_risk_once() {
    #[cfg(target_os = "macos")]
    {
        if std::env::var_os("BS_DARWIN_PANIC_WARN").as_deref() == Some(std::ffi::OsStr::new("0")) {
            return;
        }
        // Probe the running kernel build. `uname -v` looks like
        //   "Darwin Kernel Version 25.4.0: Thu Mar 19 19:26:07 PDT 2026; root:xnu-12377.101.15~1/RELEASE_ARM64_T6031"
        let kver = std::process::Command::new("uname")
            .arg("-v")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let is_known_bad = kver.contains("xnu-12377.101.15");
        if !is_known_bad {
            return;
        }
        eprintln!(
            "[bs] WARNING: macOS kernel xnu-12377.101.15 is known to \
             force-reboot the host under bs's mach syscall load."
        );
        eprintln!(
            "[bs]          Apple Feedback Assistant report filed \
             2026-05-15; see doc/macos-26.4.1-panic-risk.md."
        );
        eprintln!(
            "[bs]          Suppress this warning with \
             BS_DARWIN_PANIC_WARN=0 once you've internalised the risk."
        );
    }
}

fn main() {
    let logger = env_logger::Logger::from_default_env();
    let filter = logger.filter();
    LOGGER_SWITCHER.switch(logger, filter);

    warn_macos_panic_risk_once();

    let args = Args::parse();
    ui::config::set(UIConfig::from(&args));
    fn fun_name(p: &String) -> PathBuf {
        PathBuf::from(p)
    }

    rust::Environment::init(args.std_lib_path.as_ref().map(fun_name));

    // --describe-commands is a pure metadata path: no debuggee required.
    if args.describe_commands {
        let mut stdout = std::io::stdout().lock();
        bugstalker::ui::script::run_describe(&mut stdout)
            .unwrap_or_exit(ErrorKind::Io, "describe-commands");
        return;
    }

    let debugee_src = || {
        if let Some(ref debugee) = args.debugee {
            DebugeeSource::File {
                path: debugee,
                args: &args.args,
                cwd: args.cwd.as_deref(),
            }
        } else if let Some(pid) = args.pid {
            DebugeeSource::Process { pid }
        } else {
            print_fatal_and_exit(
                ErrorKind::ArgumentConflict,
                "Please provide a debugee name or use a \"-p\" option for attach to already running process",
            );
        }
    };

    // --script bypasses the supervisor entirely. JSON-RPC over stdio.
    if args.script {
        bugstalker::ui::script::run_script(debugee_src(), args.oracle.clone())
            .unwrap_or_exit(ErrorKind::Io, "script");
        return;
    }

    // --record drives a JSON-RPC session and writes a runnable test
    // script as we go. Same wire format as --script for the inputs
    // and outputs.
    if let Some(out_path) = args.record.clone() {
        bugstalker::ui::script::run_record(
            &out_path,
            debugee_src(),
            args.oracle.clone(),
            !args.no_masks,
        )
        .unwrap_or_exit(ErrorKind::Io, "record");
        return;
    }

    // --test reads a script file, accumulates assertions into TAP, and
    // exits with 0 on all-pass / 1 on any failure or bail-out. Same
    // dispatcher as --script underneath. `--bless` rewrites mismatched
    // expect blocks instead of failing.
    if let Some(script_path) = args.test.clone() {
        let opts = if args.bless {
            bugstalker::ui::script::RunOptions {
                bless: true,
                address_masking: !args.no_masks,
            }
        } else {
            bugstalker::ui::script::RunOptions::default()
        };
        let code = bugstalker::ui::script::run_test_with(
            &script_path,
            debugee_src(),
            args.oracle.clone(),
            opts,
        )
        .unwrap_or_exit(ErrorKind::Io, "test");
        exit(code);
    }

    // Determine interface mode
    let interface = if args.dap_local {
        // Stdio DAP mode
        let tracer = args.dap_log_file.as_ref().map(|path| {
            dap::tracer::FileTracer::new(path).unwrap_or_exit(ErrorKind::Io, "DAP server error")
        });

        Interface::DAP { tracer }
    } else if let Some(listen_addr) = &args.dap_remote {
        // TCP DAP server mode
        run_dap_tcp_server(&args, listen_addr)
            .unwrap_or_exit(ErrorKind::Io, "DAP TCP server error");
        return;
    } else if args.tui {
        Interface::TUI {
            source: debugee_src(),
        }
    } else {
        Interface::Default {
            source: debugee_src(),
        }
    };

    ui::supervisor::Supervisor::run(interface, &args.oracle)
        .unwrap_or_exit(ErrorKind::InvalidSubcommand, "Application error")
}

fn run_dap_tcp_server(args: &Args, listen_addr: &str) -> anyhow::Result<()> {
    use log::warn;
    use std::net::{SocketAddr, TcpListener};

    let addr: SocketAddr = listen_addr
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid listen address: {}", listen_addr))?;
    let listener =
        TcpListener::bind(addr).map_err(|e| anyhow::anyhow!("Failed to bind {}: {}", addr, e))?;

    log::info!(target: "dap", "DAP TCP server listening on {addr}");

    let tracer = match &args.dap_log_file {
        Some(path) => Some(dap::tracer::FileTracer::new(path)?),
        None => None,
    };

    // Server mode: accept multiple clients sequentially. One client == one debug session.
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(v) => v,
            Err(err) => {
                warn!(target: "dap", "accept failed: {err:#}");
                continue;
            }
        };
        log::info!(target: "dap", "DAP client connected: {peer}");
        if let Some(t) = &tracer {
            t.line(&format!("client connected: {peer}"));
        }

        let io = match dap::transport::new_tcp_transport(stream, tracer.clone()) {
            Ok(v) => v,
            Err(err) => {
                warn!(target: "dap", "failed to init DAP I/O: {err:#}");
                continue;
            }
        };

        let transport = Arc::new(Mutex::new(io));
        let res = yadap::session::DebugSession::new(transport).run(args.oracle.clone());
        if let Err(err) = res {
            warn!(target: "dap", "session ended with error: {err:#}");
            if let Some(t) = &tracer {
                t.line(&format!("session error: {err:#}"));
            }
        } else if let Some(t) = &tracer {
            t.line("session finished OK");
        }

        if args.dap_oneshot {
            break;
        }
    }
    Ok(())
}

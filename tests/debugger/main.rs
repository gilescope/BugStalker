// SPDX-License-Identifier: MIT
mod common;

mod breakpoints;
mod io;
mod multithreaded;
mod signal;
mod steps;
mod symbol;
mod tokio;
mod unwind;
mod variables;
// Hardware-watchpoint tests only run on x86_64. The aarch64 code path
// exists (see src/debugger/register/aarch64.rs::debug_impl) and is
// correct for real hardware, but the CI environments we currently run
// under (Docker Desktop + Apple Virtualization.framework on ARM Macs,
// QEMU user-mode on x86 hosts) do not virtualise the debug-exception
// delivery path — `PTRACE_SETREGSET(NT_ARM_HW_WATCH)` is accepted but
// no `TRAP_HWBKPT` is ever raised when the debuggee writes to the
// watched address. Running these on an aarch64 Linux VM with proper
// debug-register virtualisation (e.g. a KVM host, bare-metal) should
// pass.
#[cfg(target_arch = "x86_64")]
mod watchpoint;

use crate::common::{TestHooks, TestInfo};
use bugstalker::debugger::process::{Child, Installed};
use bugstalker::debugger::register::{Register, RegisterMap};
use bugstalker::debugger::{DebuggerBuilder, rust};
use serial_test::serial;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::thread;

/// Darwin: ensure the running test binary carries the
/// `com.apple.security.cs.debugger` entitlement; without it,
/// `task_for_pid` on the spawned inferior returns KERN_FAILURE
/// even though we own the child. Cargo rebuilds wipe ad-hoc
/// signatures, so we self-sign + re-exec on first run if the
/// entitlement is missing. Idempotent — once the running image
/// already has it, this is a no-op.
#[cfg(target_os = "macos")]
fn ensure_entitled_self_or_reexec() {
    use std::process::Command;
    use std::sync::OnceLock;
    static GUARD: OnceLock<()> = OnceLock::new();
    if GUARD.get().is_some() {
        return;
    }
    if std::env::var_os("BS_DARWIN_RESIGNED").is_some() {
        // Already re-exec'd once; if it still isn't entitled, give up
        // rather than loop. The earlier `codesign` clearly succeeded.
        let _ = GUARD.set(());
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => {
            let _ = GUARD.set(());
            return;
        }
    };
    // Probe entitlements; if cs.debugger is already present, nothing to do.
    let out = Command::new("codesign")
        .args(["-d", "--entitlements", "-"])
        .arg(&exe)
        .output();
    if let Ok(out) = &out {
        let blob = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
        if blob.contains("com.apple.security.cs.debugger") {
            let _ = GUARD.set(());
            return;
        }
    }
    // Locate the entitlements plist relative to the workspace.
    let plist = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/darwin.entitlements");
    if !plist.exists() {
        eprintln!("[bs/test] entitlements plist not found at {plist:?}; skipping resign");
        let _ = GUARD.set(());
        return;
    }
    let status = Command::new("codesign")
        .args(["--entitlements"])
        .arg(&plist)
        .args(["--force", "--sign", "-"])
        .arg(&exe)
        .status();
    match status {
        Ok(s) if s.success() => {
            // Re-exec ourselves so the kernel picks up the new
            // signature. The original `cargo test` invocation will
            // see this child's exit code as the test result.
            let mut cmd = Command::new(&exe);
            cmd.args(std::env::args_os().skip(1));
            cmd.env("BS_DARWIN_RESIGNED", "1");
            // Use exec to avoid leaving a stub parent behind.
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            eprintln!("[bs/test] failed to re-exec after resign: {err}");
            std::process::exit(70);
        }
        Ok(s) => {
            eprintln!("[bs/test] codesign exited with status {s}; tests will likely fail");
        }
        Err(e) => {
            eprintln!("[bs/test] codesign failed to spawn: {e}");
        }
    }
    let _ = GUARD.set(());
}

#[cfg(not(target_os = "macos"))]
fn ensure_entitled_self_or_reexec() {}

/// Darwin: ensure each example binary has a fresh `.dSYM` bundle.
/// Mach-O carries no DWARF in the executable itself; `dsymutil` has
/// to extract it from the per-CU `.o` files into a sidecar bundle.
/// Cargo doesn't run this step on its own, and a stale bundle —
/// older than the binary it describes — points at addresses that no
/// longer exist. We re-run `dsymutil` lazily, only when the bundle
/// is missing or older than the binary, so test re-runs don't pay
/// the cost twice.
#[cfg(target_os = "macos")]
fn ensure_dsym_fresh(prog: &str) {
    use std::process::Command;
    let bin = Path::new(prog);
    let bin_meta = match std::fs::metadata(bin) {
        Ok(m) => m,
        Err(_) => return,
    };
    let bin_mtime = bin_meta
        .modified()
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let dsym_inner = bin
        .with_extension(format!(
            "{}.dSYM",
            bin.extension().and_then(|e| e.to_str()).unwrap_or("")
        ))
        .join("Contents")
        .join("Resources")
        .join("DWARF")
        .join(bin.file_name().unwrap_or_default());
    let dsym_path = match bin.file_name() {
        Some(name) => {
            let mut p = bin.to_path_buf().into_os_string();
            p.push(".dSYM");
            std::path::PathBuf::from(p)
                .join("Contents")
                .join("Resources")
                .join("DWARF")
                .join(name)
        }
        None => return,
    };
    let _ = dsym_inner; // keep the variable for future symlink-aware checks
    let needs_refresh = match std::fs::metadata(&dsym_path) {
        Ok(m) => m
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            < bin_mtime,
        Err(_) => true,
    };
    if needs_refresh {
        let _ = Command::new("dsymutil").arg(bin).status();
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_dsym_fresh(_prog: &str) {}

pub fn prepare_debugee_process(prog: &str, args: &[&'static str]) -> Child<Installed> {
    ensure_entitled_self_or_reexec();
    ensure_dsym_fresh(prog);
    let (reader, writer) = os_pipe::pipe().unwrap();

    thread::spawn(move || {
        let mut stream = BufReader::new(reader);
        loop {
            let mut line = String::new();
            let size = stream.read_line(&mut line).unwrap_or(0);
            if size == 0 {
                return;
            }
        }
    });

    rust::Environment::init(None);

    let runner = Child::new(
        prog,
        args.to_vec(),
        None::<&Path>,
        writer.try_clone().unwrap(),
        writer,
    );
    runner.install().unwrap()
}

const HW_APP: &str = "./examples/target/debug/hello_world";
const CALC_APP: &str = "./examples/target/debug/calc";
const MT_APP: &str = "./examples/target/debug/mt";
const VARS_APP: &str = "./examples/target/debug/vars";
const RECURSION_APP: &str = "./examples/target/debug/recursion";
const SIGNALS_APP: &str = "./examples/target/debug/signals";
const SHARED_LIB_APP: &str = "./examples/target/debug/calc_bin";
const SLEEPER_APP: &str = "./examples/target/debug/sleeper";
const FIZZBUZZ_APP: &str = "./examples/target/debug/fizzbuzz";
#[cfg(target_arch = "x86_64")]
const CALCULATIONS_APP: &str = "./examples/target/debug/calculations";
const TOKIO_TICKER_APP: &str = "./examples/target/debug/tokioticker";
const CALLS_APP: &str = "./examples/target/debug/calls";

#[test]
#[serial]
fn test_debugger_graceful_shutdown() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let pid = process.pid();

    let builder = DebuggerBuilder::new().with_hooks(TestHooks::default());
    let mut debugger = builder.build(process).unwrap();
    debugger
        .set_breakpoint_at_line("hello_world.rs", 5)
        .unwrap();
    debugger.start_debugee().unwrap();
    drop(debugger);

    assert_no_proc!(pid);
}

#[test]
#[serial]
fn test_debugger_graceful_shutdown_multithread() {
    let process = prepare_debugee_process(MT_APP, &[]);
    let pid = process.pid();

    let builder = DebuggerBuilder::new().with_hooks(TestHooks::default());
    let mut debugger = builder.build(process).unwrap();
    debugger.set_breakpoint_at_line("mt.rs", 31).unwrap();
    debugger.start_debugee().unwrap();
    drop(debugger);

    assert_no_proc!(pid);
}

#[test]
#[serial]
fn test_frame_cfa() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let debugee_pid = process.pid();

    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();
    debugger
        .set_breakpoint_at_line("hello_world.rs", 5)
        .unwrap();
    debugger
        .set_breakpoint_at_line("hello_world.rs", 15)
        .unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(5));

    let sp = RegisterMap::current(debugee_pid)
        .unwrap()
        .value(Register::SP);

    debugger.continue_debugee().unwrap();
    let frame_info = debugger.frame_info().unwrap();

    // expect that cfa equals stack pointer from callee function.
    assert_eq!(sp, u64::from(frame_info.cfa));

    drop(debugger);
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_registers() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let debugee_pid = process.pid();

    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();
    debugger
        .set_breakpoint_at_line("hello_world.rs", 5)
        .unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(5));

    // The unwinder fills `frame.return_addr` from whichever DWARF
    // column the CIE designates as its return-address register —
    // column 16 (rip) on x86_64, column 30 (x30/LR) on aarch64.
    // `Register::RA` resolves to that arch-appropriate column.
    let pc = debugger.ecx().location().pc;
    let frame = debugger.frame_info().unwrap();
    let registers = debugger.current_thread_registers_at_pc(pc).unwrap();
    let ra_register = Register::RA
        .dwarf_register()
        .expect("return-address register must map to a DWARF register");
    assert_eq!(
        u64::from(frame.return_addr.unwrap()),
        registers.value(ra_register).unwrap()
    );

    drop(debugger);
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_debugger_disassembler() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let pid = process.pid();

    let builder = DebuggerBuilder::new().with_hooks(TestHooks::default());
    let mut debugger = builder.build(process).unwrap();
    debugger.set_breakpoint_at_fn("main").unwrap();
    debugger.start_debugee().unwrap();

    let fn_assembly = debugger.disasm().unwrap();
    assert_eq!(fn_assembly.name, Some("hello_world::main".to_string()));
    assert!(!fn_assembly.instructions.is_empty());

    debugger.set_breakpoint_at_fn("myprint").unwrap();
    debugger.continue_debugee().unwrap();

    let fn_assembly = debugger.disasm().unwrap();
    assert_eq!(fn_assembly.name, Some("hello_world::myprint".to_string()));
    assert!(!fn_assembly.instructions.is_empty());

    drop(debugger);
    assert_no_proc!(pid);
}

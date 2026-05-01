// SPDX-License-Identifier: MIT
mod common;

// Phase 3 Feature D batch D3b — async await-trace tests rely on
// tokio runtime introspection, which is currently Linux-only. The
// existing `tokio.rs` module is in the same boat and is already a
// known-fail on Darwin; we gate the new dedicated suite explicitly
// so local Darwin test runs stay clean while Linux CI exercises it.
#[cfg(target_os = "linux")]
mod async_await;
mod breakpoints;
mod io;
mod multithreaded;
mod signal;
mod steps;
mod symbol;
// Tokio runtime introspection on Darwin needs more than the TLS
// resolver — `darwin_mach::resolve_tlv` lands the per-thread CONTEXT
// slot correctly, but walking tokio's worker registry from there
// still hits a wall (probably an async-fn name-mangling or
// vtable-PAC quirk). Keep `mod tokio` Linux-only until a dedicated
// tokio-on-darwin batch.
#[cfg(target_os = "linux")]
mod tokio;
mod fuzz;
mod unwind;
mod variables;
mod viz;
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
///
/// **Cross-process serialisation under nextest parallel.**
/// The `OnceLock` guard only covers a single process. When
/// nextest fans out into many test processes against the same
/// on-disk binary, naive concurrent `codesign --force --sign -`
/// calls race on the signature blob — a late writer can
/// overwrite a partial earlier write, surfacing as
/// `task_for_pid` → `KERN_FAILURE` → `Ptrace(EFAULT)` from a
/// random subset of tests. Mitigation: take an exclusive
/// `flock(2)` on a sibling `.codesign.lock` file before
/// touching the signature, then re-probe entitlements once we
/// hold the lock so a process that lost the race short-circuits
/// instead of double-signing. Build-time signing in `build.rs`
/// would be a stronger fix (no runtime race at all); the lock
/// is the cheap step.
#[cfg(target_os = "macos")]
fn ensure_entitled_self_or_reexec() {
    use std::os::fd::AsRawFd;
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

    let probe_entitled = |exe: &Path| -> bool {
        let out = Command::new("codesign")
            .args(["-d", "--entitlements", "-"])
            .arg(exe)
            .output();
        match out {
            Ok(out) => {
                let blob = String::from_utf8_lossy(&out.stdout)
                    + String::from_utf8_lossy(&out.stderr);
                blob.contains("com.apple.security.cs.debugger")
            }
            Err(_) => false,
        }
    };

    // Fast path: already entitled, no lock needed.
    if probe_entitled(&exe) {
        let _ = GUARD.set(());
        return;
    }
    // Locate the entitlements plist relative to the workspace.
    let plist = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/darwin.entitlements");
    if !plist.exists() {
        eprintln!("[bs/test] entitlements plist not found at {plist:?}; skipping resign");
        let _ = GUARD.set(());
        return;
    }

    // Cross-process serialisation. Open (or create) a sibling
    // lock file next to the binary, then `flock(LOCK_EX)` to
    // serialise resign with any other test process pointing at
    // the same binary. The lock auto-releases when `_lock_file`
    // is dropped at end of scope.
    let lock_path = {
        let mut p = exe.clone();
        let new_name = match p.file_name().and_then(|s| s.to_str()) {
            Some(name) => format!("{name}.codesign.lock"),
            None => "codesign.lock".to_string(),
        };
        p.set_file_name(new_name);
        p
    };
    let _lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => {
            // SAFETY: fd is valid for the duration of `f`; flock
            // is a thread-safe POSIX advisory lock. LOCK_EX
            // blocks until acquired (no timeout — we want to
            // wait for the prior signer).
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                eprintln!(
                    "[bs/test] flock({lock_path:?}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            f
        }
        Err(e) => {
            eprintln!("[bs/test] could not open codesign lock {lock_path:?}: {e}");
            // Fall through unlocked — best effort.
            std::fs::File::open("/dev/null").expect("/dev/null open")
        }
    };

    // Re-probe under the lock: if a peer just finished signing
    // we skip the redundant (and racy) overwrite. We still need
    // to re-exec ourselves below — our running image was loaded
    // from the file *before* the peer signed, so the kernel
    // stamped us with the old (un-entitled) csflags.
    let already_signed_by_peer = probe_entitled(&exe);
    if !already_signed_by_peer {
        let status = Command::new("codesign")
            .args(["--entitlements"])
            .arg(&plist)
            .args(["--force", "--sign", "-"])
            .arg(&exe)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("[bs/test] codesign exited with status {s}; tests will likely fail");
                let _ = GUARD.set(());
                return;
            }
            Err(e) => {
                eprintln!("[bs/test] codesign failed to spawn: {e}");
                let _ = GUARD.set(());
                return;
            }
        }
    }
    // Re-exec so the kernel re-reads the signature. Drop the
    // lock first so any waiting peer wakes up immediately,
    // re-probes, finds the file already entitled, and re-execs
    // without redundantly resigning.
    let mut cmd = Command::new(&exe);
    cmd.args(std::env::args_os().skip(1));
    cmd.env("BS_DARWIN_RESIGNED", "1");
    drop(_lock_file);
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("[bs/test] failed to re-exec after resign: {err}");
    std::process::exit(70);
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
    // Opt-out: the loader's own `ensure_dsym_fresh` (in
    // `src/debugger/debugee/dwarf/mod.rs`) handles the
    // `split-debuginfo = "unpacked"` recovery for end users at
    // attach time. Tests that want to exercise that loader path
    // explicitly set this env var so the test harness *doesn't*
    // pre-emptively run dsymutil and mask the loader's work.
    if std::env::var_os("BS_TEST_NO_AUTODSYM").is_some() {
        return;
    }
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
// `mod tokio` is Linux-only on Darwin (worker-registry walk needs
// more than TLS); gate the path constant likewise so darwin builds
// stay warning-clean.
#[cfg(target_os = "linux")]
const TOKIO_TICKER_APP: &str = "./examples/target/debug/tokioticker";
// Phase 3 D3b debuggees — only consumed by `mod async_await`,
// which is itself Linux-gated.
#[cfg(target_os = "linux")]
const TOKIO_SIMPLE_AWAIT_APP: &str = "./examples/target/debug/tokio_simple_await";
#[cfg(target_os = "linux")]
const TOKIO_CHAINED_AWAIT_APP: &str = "./examples/target/debug/tokio_chained_await";
#[cfg(target_os = "linux")]
const TOKIO_SELECT_APP: &str = "./examples/target/debug/tokio_select";
#[cfg(target_os = "linux")]
const TOKIO_JOIN_APP: &str = "./examples/target/debug/tokio_join";
#[cfg(target_os = "linux")]
const TOKIO_DYN_FUTURE_APP: &str = "./examples/target/debug/tokio_dyn_future";
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

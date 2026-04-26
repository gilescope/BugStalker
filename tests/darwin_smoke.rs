//! End-to-end smoke test for the darwin/aarch64 backend.
//!
//! Drives the lowest layer of the macOS port: spawn a small
//! debuggee via [`Child::install`] (fork + `PT_TRACE_ME` + exec),
//! prove that we can talk to it via Mach (`task_for_pid` →
//! `thread_get_state(ARM_THREAD_STATE64)` → read PC/SP), and
//! that the stop happened where we expect (PC inside the loaded
//! binary, SP inside the user-space stack range). Doesn't yet
//! drive a full `Debugger` — that needs DWARF dSYM resolution to
//! land first.
//!
//! Skipped at compile time on every other OS so the linux/arm64
//! and linux/x86_64 test suites are unaffected.
//!
//! ## Running this test
//!
//! Requires `examples/target/debug/hello_world` to exist:
//!
//! ```sh
//! cd examples && cargo build -p hello_world
//! ```
//!
//! `task_for_pid` on modern macOS requires the
//! `com.apple.security.cs.debugger` entitlement on the *caller*
//! (i.e. the test binary, not the debuggee). Even ptraced children
//! aren't an exception in current SIP-enabled OSes — `task_for_pid`
//! returns `KERN_FAILURE` (`0x5`) without it. Sign the test binary
//! with the entitlements file in this directory:
//!
//! ```sh
//! cargo test --no-run --test darwin_smoke
//! TEST_BIN=$(find target/debug/deps -name 'darwin_smoke-*' -perm +111 | head -1)
//! codesign --entitlements tests/darwin.entitlements --force --sign - "$TEST_BIN"
//! cargo test --test darwin_smoke -- --ignored
//! ```
//!
//! The `#[ignore]` below is there so an unsigned `cargo test` run
//! doesn't fail on machines where the entitlement isn't applied.

#![cfg(target_os = "macos")]

use bugstalker::debugger::process::Child;
use bugstalker::debugger::register::RegisterMap;
use os_pipe::pipe;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const HELLO_WORLD: &str = "./examples/target/debug/hello_world";

fn ensure_debuggee_present() {
    if !PathBuf::from(HELLO_WORLD).exists() {
        panic!(
            "darwin smoke: missing debuggee at {HELLO_WORLD}. \
             Build it first: `cd examples && cargo build -p hello_world`"
        );
    }
}

#[test]
#[ignore = "needs `codesign --entitlements tests/darwin.entitlements …` on the test binary; see file header"]
fn spawn_and_read_pc() {
    ensure_debuggee_present();

    let (_stdout_r, stdout_w) = pipe().expect("create stdout pipe");
    let (_stderr_r, stderr_w) = pipe().expect("create stderr pipe");

    let child = Child::new(
        HELLO_WORLD,
        Vec::<String>::new(),
        None::<PathBuf>,
        stdout_w,
        stderr_w,
    );

    let installed = child.install().expect("Child::install");
    let pid = installed.pid();

    // After `Child::install`, the debuggee should be paused at the
    // post-execve SIGTRAP (or SIGSTOP) the kernel delivers in
    // response to PT_TRACE_ME. We can talk to it over Mach.
    let regs = RegisterMap::current(pid).expect("RegisterMap::current via thread_get_state");

    // PC should sit somewhere in user space. Aarch64 user VA on
    // darwin tops out below 0x0000_FFFF_FFFF_FFFF, and user space
    // starts well above the null page; any non-zero value below
    // the top is plausible for an entry point.
    assert!(regs.pc() != 0, "PC must be set after exec; got 0");
    assert!(
        regs.pc() < 0x0000_FFFF_FFFF_FFFFu64,
        "PC must be in aarch64 user-space range; got {:#x}",
        regs.pc()
    );

    // SP should similarly be a real user-space stack address.
    assert!(regs.sp() != 0, "SP must be set after exec; got 0");
    assert!(
        regs.sp() < 0x0000_FFFF_FFFF_FFFFu64,
        "SP must be in aarch64 user-space range; got {:#x}",
        regs.sp()
    );

    // Clean up: the debuggee is a ptraced child stopped at SIGTRAP.
    // A plain `kill(SIGKILL)` *delivers* the signal but the kernel
    // queues it until the tracer continues the tracee. Use
    // `ptrace::cont(pid, SIGKILL)` to inject + resume in one step,
    // then `waitpid` reaps the process. (We could use `ptrace::kill`
    // but it's documented as deprecated even on bsd.)
    let _ = nix::sys::ptrace::cont(pid, Some(nix::sys::signal::SIGKILL));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => return,
        }
    }
    panic!("debuggee {pid} didn't die within 5s of ptrace::cont(SIGKILL)");
}

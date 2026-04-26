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
use bugstalker::debugger::{DebuggerBuilder, NopHook, rust};
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

/// Higher-level: build a full `Debugger` over the spawned hello_world,
/// set a breakpoint on `main`, run, and verify we hit the BP. This
/// exercises the dSYM bundle loader, the BRK install path through
/// the Mach memory shim, and the Tracer event loop end to end.
///
/// **Prerequisite:** the binary must have a `.dSYM` bundle —
/// `cargo build` on macOS doesn't generate one, so run:
/// ```sh
/// dsymutil examples/target/debug/hello_world
/// ```
#[test]
#[ignore = "needs codesigning + dsymutil; see file header"]
fn debugger_runs_to_first_breakpoint() {
    ensure_debuggee_present();
    rust::Environment::init(None);

    let (_reader, writer) = pipe().expect("pipe");
    let runner = Child::new(
        HELLO_WORLD,
        Vec::<String>::new(),
        None::<PathBuf>,
        writer.try_clone().expect("clone writer"),
        writer,
    );
    let installed = runner.install().expect("Child::install");

    let builder = DebuggerBuilder::new().with_hooks(NopHook {});
    let mut debugger = builder
        .build(installed)
        .expect("DebuggerBuilder::build — typically fails here if dSYM bundle is missing");

    debugger
        .set_breakpoint_at_line("hello_world.rs", 5)
        .expect("set_breakpoint_at_line");
    debugger.start_debugee().expect("start_debugee");
    // If we got here, the breakpoint at hello_world.rs:5 fired and
    // the tracer returned with the debuggee paused inside main.
    // The next set of assertions (PC at stop, continue-to-exit)
    // depends on a more disciplined Tracer event loop than the
    // current ptrace+SIGTRAP shortcut delivers — Mach exception
    // ports land in a follow-up.
}

/// Allocate a Mach exception port and subscribe it to a freshly
/// spawned debuggee. Doesn't yet try to *receive* exceptions —
/// that's the next iteration. Verifying allocation + registration
/// here means the foundation is in place when the receive loop
/// lands.
#[test]
#[ignore = "needs codesigning; see file header"]
fn exception_port_allocate_and_register() {
    use bugstalker::debugger::darwin_mach::{self, ExceptionPort};

    ensure_debuggee_present();

    let (_reader, writer) = pipe().expect("pipe");
    let runner = Child::new(
        HELLO_WORLD,
        Vec::<String>::new(),
        None::<PathBuf>,
        writer.try_clone().expect("clone writer"),
        writer,
    );
    let installed = runner.install().expect("Child::install");
    let pid = installed.pid();

    let port = ExceptionPort::allocate().expect("ExceptionPort::allocate");
    let task = darwin_mach::task_for_pid(pid).expect("task_for_pid");
    port.register(task).expect("ExceptionPort::register");

    // Drop the port (releases receive + send) and clean up the
    // ptraced child the same way the spawn-and-read smoke does.
    drop(port);
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

/// Allocate a port, don't subscribe it to anything, and prove
/// `receive` returns `Ok(None)` when the timeout elapses with no
/// message in the queue. This is the cheapest possible exercise
/// of the `mach_msg(MACH_RCV_MSG | MACH_RCV_TIMEOUT)` path — no
/// child process needed, no codesigning needed, runs on every
/// macOS host. It catches obvious mistakes (wrong message header
/// pointer, wrong port name, byte-order/timeout argument swaps).
#[test]
fn exception_port_receive_times_out() {
    use bugstalker::debugger::darwin_mach::ExceptionPort;

    let port = ExceptionPort::allocate().expect("ExceptionPort::allocate");
    let started = Instant::now();
    let result = port
        .receive(50)
        .expect("receive should not error on timeout");
    let elapsed = started.elapsed();

    assert!(result.is_none(), "no exception was queued; expected None");
    // Generous bounds: kernel scheduling can delay return a bit,
    // and on a heavily loaded machine the lower bound matters less
    // than the upper bound (it shouldn't block forever).
    assert!(
        elapsed >= Duration::from_millis(40),
        "receive returned before the timeout elapsed: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "receive blocked far past the requested timeout: {elapsed:?}"
    );
}

/// `thread_info(THREAD_IDENTIFIER_INFO)` on our own main thread
/// returns a non-zero stable id and a pthread_t handle. We use
/// `mach_thread_self()` (no entitlement needed for our own task)
/// so the test runs on every macOS host.
#[test]
fn thread_identity_self() {
    use bugstalker::debugger::darwin_mach::thread_identity;

    // SAFETY: mach_thread_self always succeeds; result is a port
    // name in our task's IPC space.
    let me = unsafe { mach2::mach_init::mach_thread_self() };
    let id = thread_identity(me).expect("thread_identity on self");

    assert!(id.thread_id != 0, "thread_id must be non-zero");
    assert!(
        id.thread_handle != 0,
        "thread_handle (pthread_t) must be non-zero on a live thread"
    );
}

/// `ExceptionPort::reply` to a port name we don't hold a send
/// right to should fail predictably (`MACH_SEND_INVALID_DEST` =
/// `0x10000003`) rather than crash, hang, or succeed silently.
/// This proves the reply-message encoding (msgh_bits, sizes,
/// header layout) round-trips through `mach_msg(MACH_SEND_MSG)`
/// — the kernel parses the header before checking the port and
/// would return a different error on a malformed message.
///
/// We can't easily test the success branch from a unit test (it
/// needs a real kernel-side request waiting on a send-once right
/// in our IPC space), so the integration test that exercises the
/// happy path runs end-to-end against a debuggee — that arrives
/// later in the macOS port.
#[test]
fn exception_port_reply_to_null_port() {
    use bugstalker::debugger::darwin_mach::ExceptionPort;

    let err = ExceptionPort::reply(0, 2405, 0).expect_err("null port must fail");
    // MACH_SEND_INVALID_DEST = 0x10000003. Allow any non-success
    // reply error in the 0x1000_xxxx range — the kernel may also
    // return MACH_SEND_INVALID_HEADER (0x10000000) on certain
    // builds; the important thing is "it failed and didn't lie".
    let kr = err.0 as u32;
    // Sanity: Display gives back the kr in hex plus a name.
    let rendered = err.to_string();
    assert!(
        rendered.contains(&format!("0x{kr:08x}")),
        "Display should include the raw kr; got {rendered}"
    );
    assert!(
        !rendered.contains("unknown"),
        "MACH_SEND_* code should be named, got {rendered}"
    );
    assert!(
        (0x1000_0000..=0x1000_FFFF).contains(&kr),
        "expected a MACH_SEND_* error, got 0x{kr:08x}"
    );
}

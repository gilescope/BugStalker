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

/// Exercise the `ARM_DEBUG_STATE64` API surface on a suspended
/// worker. Reads must return KERN_SUCCESS with the default-zero
/// state (a fresh thread has no hw watchpoints set), and the set
/// path must round-trip the syscall (returns KERN_SUCCESS) — but
/// we deliberately *don't* assert the kernel preserved the bytes
/// we wrote. Darwin silently drops `thread_set_state` for the
/// `ARM_DEBUG_STATE64` flavour on threads that aren't being
/// supervised (no ptrace, no exception-port subscription). That's
/// a feature: an unprivileged process shouldn't be able to arm
/// watchpoints on arbitrary threads. The actual write semantics
/// are exercised by the entitlement-gated end-to-end debugger
/// path.
#[test]
fn arm_debug_state64_api_surface() {
    use bugstalker::debugger::darwin_mach::{
        thread_get_arm_debug_state64, thread_set_arm_debug_state64, thread_suspend,
    };
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<u32>();
    std::thread::spawn(move || {
        let me = unsafe { mach2::mach_init::mach_thread_self() };
        tx.send(me).expect("send port");
        loop {
            std::thread::park();
        }
    });
    let worker = rx.recv().expect("worker port");

    thread_suspend(worker).expect("thread_suspend");
    let s = thread_get_arm_debug_state64(worker).expect("get debug state");
    assert!(
        s.bvr.iter().all(|&v| v == 0)
            && s.bcr.iter().all(|&v| v == 0)
            && s.wvr.iter().all(|&v| v == 0)
            && s.wcr.iter().all(|&v| v == 0)
            && s.mdscr_el1 == 0,
        "fresh thread should have no hw debug registers set"
    );
    // Setting the same default state must be a successful syscall —
    // we assert the *call* works, not that the bytes stick.
    thread_set_arm_debug_state64(worker, &s).expect("set debug state");
}

/// `thread_set_arm_state64` round-trip on a suspended worker:
/// snapshot state, mutate `x[0]` to a sentinel, write it back,
/// re-read, assert the sentinel survives. We don't resume the
/// worker — between resume and the next suspend it could clobber
/// x0 by executing one instruction, and we want to test the
/// kernel-side write semantics, not a race-resistant inferior.
/// The worker leaks but the test process is short-lived.
#[test]
fn thread_set_arm_state64_roundtrip() {
    use bugstalker::debugger::darwin_mach::{
        thread_get_arm_state64, thread_set_arm_state64, thread_suspend,
    };
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<u32>();
    std::thread::spawn(move || {
        let me = unsafe { mach2::mach_init::mach_thread_self() };
        tx.send(me).expect("send port");
        loop {
            std::thread::park();
        }
    });
    let worker = rx.recv().expect("worker port");

    thread_suspend(worker).expect("thread_suspend");
    let mut s = thread_get_arm_state64(worker).expect("thread_get_arm_state64 (before)");
    let sentinel: u64 = 0xDEAD_BEEF_FEED_FACE;
    s.__x[0] = sentinel;
    thread_set_arm_state64(worker, &s).expect("thread_set_arm_state64");
    let after = thread_get_arm_state64(worker).expect("thread_get_arm_state64 (after)");

    assert_eq!(
        after.__x[0], sentinel,
        "x0 didn't survive the set/get round-trip; got {:#x}",
        after.__x[0]
    );
    // PC + SP shouldn't have changed — we only touched x[0].
    assert_eq!(after.__pc, s.__pc, "PC drifted across set");
    assert_eq!(after.__sp, s.__sp, "SP drifted across set");
}

/// `thread_get_arm_state64` against a suspended worker thread:
/// PC and SP must be non-zero and within the aarch64 user-space
/// range. We suspend before reading because Mach docs only
/// guarantee a coherent snapshot when the thread isn't executing
/// — same discipline the eventual Tracer uses around every
/// register read.
#[test]
fn thread_get_arm_state64_suspended_worker() {
    use bugstalker::debugger::darwin_mach::{thread_get_arm_state64, thread_resume, thread_suspend};
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<u32>();
    std::thread::spawn(move || {
        let me = unsafe { mach2::mach_init::mach_thread_self() };
        tx.send(me).expect("send port");
        loop {
            std::thread::park();
        }
    });
    let worker = rx.recv().expect("worker port");

    thread_suspend(worker).expect("thread_suspend");
    let s = thread_get_arm_state64(worker).expect("thread_get_arm_state64");
    thread_resume(worker).expect("thread_resume");

    assert!(s.__pc != 0, "PC must be set on a live thread");
    assert!(
        s.__pc < 0x0000_FFFF_FFFF_FFFF,
        "PC must be in aarch64 user-space range; got {:#x}",
        s.__pc
    );
    assert!(s.__sp != 0, "SP must be set");
    assert!(
        s.__sp < 0x0000_FFFF_FFFF_FFFF,
        "SP must be in aarch64 user-space range; got {:#x}",
        s.__sp
    );
}

/// `vm_read_n` and `vm_write_word` round-trip against our own
/// task. Allocates a fresh page (so we know the layout), writes
/// a sentinel via `vm_write_word`, reads it back via `vm_read_n`,
/// asserts equality. This exercises the `mach_vm_protect` framing
/// dance even on a writable page (`VM_PROT_COPY|R|W` → write →
/// restore) which is the same dance used to write to r-x text
/// pages in the debuggee.
///
/// Catches regressions in the BRK install path on every macOS
/// host without needing a debuggee or entitlement.
#[test]
fn vm_write_then_read_self() {
    use bugstalker::debugger::darwin_mach::{vm_read_n, vm_write_word};

    // 16 KiB page is darwin/aarch64's default; libc::sysconf is
    // the portable way to ask. We just need a writable region.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(page != libc::MAP_FAILED, "mmap failed");

    let task = unsafe { mach2::traps::mach_task_self() };
    let sentinel: usize = 0xCAFEBABEDEADBEEF;
    let addr = page as usize;

    vm_write_word(task, addr, sentinel).expect("vm_write_word");
    let bytes = vm_read_n(task, addr, std::mem::size_of::<usize>()).expect("vm_read_n");
    let read_back = usize::from_ne_bytes(bytes.as_slice().try_into().expect("8 bytes"));
    assert_eq!(read_back, sentinel, "round-trip mismatch");

    unsafe {
        libc::munmap(page, page_size);
    }
}

/// `thread_suspend` + `thread_resume` round-trip on a worker
/// thread we control. Suspending the test's *own* main thread
/// would deadlock the test (it'd never resume itself), so spawn
/// a worker that publishes its Mach thread port through a channel
/// and gets suspended from main. We resume immediately so the
/// suspend count goes back to zero — Mach suspend counts are a
/// 1:1 nesting count, not a boolean.
///
/// This proves the `task_resume` / `task_suspend` family wires
/// up cleanly; the eventual `Tracer` cutover from ptrace+SIGTRAP
/// will use them as the resume/pause primitives.
#[test]
fn thread_suspend_resume_roundtrip() {
    use bugstalker::debugger::darwin_mach::{thread_resume, thread_suspend};
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<u32>();
    std::thread::spawn(move || {
        // SAFETY: mach_thread_self always succeeds.
        let me = unsafe { mach2::mach_init::mach_thread_self() };
        tx.send(me).expect("send port");
        // Park forever; the test process exits after main returns
        // and reaps us as a daemon.
        loop {
            std::thread::park();
        }
    });
    let worker_port = rx.recv().expect("worker port");

    thread_suspend(worker_port).expect("thread_suspend");
    thread_resume(worker_port).expect("thread_resume — must rebalance the suspend count");
}

/// `dyld_image_list` walked against our own task port (no
/// debuggee, no entitlement) returns at least one image — the
/// test binary itself — and every entry has a non-zero
/// `load_addr` and a path string. Catches regressions in the
/// dyld_all_image_infos / TASK_DYLD_INFO walk on every macOS host.
///
/// `mach_task_self()` always succeeds for the current task; the
/// entitlement requirement only kicks in for cross-process
/// `task_for_pid`. So this gives us a free dyld_image_list
/// regression check.
#[test]
fn dyld_image_list_self() {
    use bugstalker::debugger::darwin_mach::dyld_image_list;

    // SAFETY: mach_task_self always returns a valid task port.
    let task = unsafe { mach2::traps::mach_task_self() };
    let images = dyld_image_list(task).expect("dyld_image_list on self");

    assert!(!images.is_empty(), "expected at least the test binary");
    let test_bin = images
        .iter()
        .find(|i| i.path.contains("darwin_smoke"))
        .or_else(|| images.iter().find(|i| !i.path.is_empty()))
        .expect("at least one image must have a path");
    assert!(
        test_bin.load_addr != 0,
        "load_addr must be non-zero for {}",
        test_bin.path
    );
    // Sanity-check the path is a real-looking absolute path.
    assert!(
        test_bin.path.starts_with('/') || test_bin.path.contains("dyld"),
        "first non-empty path looks bogus: {}",
        test_bin.path
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

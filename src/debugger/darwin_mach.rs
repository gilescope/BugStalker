// SPDX-License-Identifier: MIT
//! Mach kernel shims for the macOS debuggee backend.
//!
//! `task_for_pid` returns the Mach task port of the (ptraced) child
//! process; once we have that we can read/write memory with
//! `mach_vm_read_overwrite` / `mach_vm_write` and inspect/mutate
//! registers with `thread_get_state` / `thread_set_state`. The
//! linux equivalents are scattered across `PTRACE_PEEKDATA`,
//! `PTRACE_POKEDATA`, `PTRACE_GETREGSET`, `PTRACE_SETREGSET`.
//!
//! All Mach calls return a `kern_return_t`. We map non-`KERN_SUCCESS`
//! values to `MachError` and leave it to the caller to map further
//! into `crate::debugger::Error::Ptrace` (which we keep as the
//! one-error-variant for "could not poke the inferior" because it
//! preserves the meaning at the API surface even though Mach isn't
//! ptrace).

#![cfg(target_os = "macos")]

use crate::debugger::Error;
use crate::debugger::Error::Ptrace;
use mach2::exception_types::{
    EXC_MASK_BAD_ACCESS, EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE, EXCEPTION_DEFAULT,
    MACH_EXCEPTION_CODES, exception_mask_t,
};
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::mach_port::{mach_port_allocate, mach_port_insert_right};
use mach2::mach_types::{task_t, thread_act_array_t, thread_act_t, vm_task_entry_t};
use mach2::message::mach_msg_type_number_t;
use mach2::message::{
    MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_RCV_MSG,
    MACH_RCV_TIMED_OUT, MACH_RCV_TIMEOUT, MACH_SEND_MSG, MACH_SEND_TIMEOUT, mach_msg,
    mach_msg_header_t,
};
use mach2::port::mach_port_t;
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_name_t};
use mach2::structs::arm_thread_state64_t;
use mach2::task::task_set_exception_ports;
use mach2::task::task_threads;
use mach2::thread_act::{thread_get_state, thread_set_state};
use mach2::thread_status::ARM_THREAD_STATE64;
use mach2::thread_status::THREAD_STATE_NONE;
use mach2::traps::{mach_task_self, task_for_pid as raw_task_for_pid};
use mach2::vm::{mach_vm_protect, mach_vm_read_overwrite, mach_vm_region, mach_vm_write};
use mach2::vm_prot::{VM_PROT_COPY, VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, vm_prot_t};
use mach2::vm_region::{VM_REGION_BASIC_INFO_64, vm_region_basic_info_64, vm_region_info_t};
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::mem;

/// Coarse error envelope for Mach-side failures. We round-trip
/// through `Errno::EFAULT` at the boundary so callers using the
/// `nix::Error` type don't grow a Mach awareness; for callers
/// using `crate::debugger::Error` we also offer a direct `Ptrace`
/// mapping that carries `EFAULT`.
#[derive(Debug)]
pub struct MachError(pub kern_return_t);

impl MachError {
    /// Human-readable name for the wrapped `kern_return_t`. Covers
    /// the codes we actually hit in this codebase plus the most
    /// common others; unknown codes fall through to `"unknown"`.
    /// The point is rustc-level diagnostics — when something
    /// upstream prints `MachError(5)` the developer should see
    /// "task_for_pid denied: needs cs.debugger entitlement", not
    /// "0x5".
    pub fn describe(&self) -> &'static str {
        match self.0 {
            // <mach/kern_return.h>
            0 => "KERN_SUCCESS",
            1 => "KERN_INVALID_ADDRESS — read/write of unmapped vm",
            2 => {
                "KERN_PROTECTION_FAILURE — page perms reject the op (e.g. write to r-x without VM_PROT_COPY)"
            }
            3 => "KERN_NO_SPACE",
            4 => {
                "KERN_INVALID_ARGUMENT — bad task/thread port, wrong state flavour, or out-of-range count"
            }
            5 => {
                "KERN_FAILURE — generic Mach catch-all; the meaning depends on the calling op (task_for_pid: missing cs.debugger entitlement; thread_set_state on arm64: thread not suspended, hardened-runtime restriction, or stale port; task_resume: already running)"
            }
            6 => "KERN_RESOURCE_SHORTAGE",
            7 => "KERN_NOT_RECEIVER",
            8 => "KERN_NO_ACCESS",
            10 => "KERN_MEMORY_ERROR",
            14 => "KERN_ABORTED",
            15 => "KERN_INVALID_NAME — port name doesn't refer to a port we own",
            16 => "KERN_INVALID_TASK",
            17 => "KERN_INVALID_RIGHT — port name lacks the requested right (send/recv/send-once)",
            18 => "KERN_INVALID_VALUE",
            22 => "KERN_INVALID_HOST",
            37 => "KERN_TERMINATED — the target task/thread has exited",
            46 => "KERN_NOT_SUPPORTED",
            49 => "KERN_OPERATION_TIMED_OUT",
            // <mach/message.h> — only the ones we actually trigger.
            0x10000001 => "MACH_SEND_INVALID_DATA",
            0x10000002 => {
                "MACH_SEND_INVALID_DEST — destination port name is not a valid send right"
            }
            0x10000003 => "MACH_SEND_TIMED_OUT",
            0x10000004 => "MACH_SEND_INTERRUPTED",
            0x10000007 => {
                "MACH_SEND_INVALID_HEADER — malformed mach_msg_header_t (bits, size, or port refs)"
            }
            0x10004003 => "MACH_RCV_TIMED_OUT",
            0x10004002 => "MACH_RCV_INVALID_NAME",
            _ => "unknown kern_return_t",
        }
    }
}

impl std::fmt::Display for MachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mach kr=0x{:08x}: {}", self.0, self.describe())
    }
}

impl std::error::Error for MachError {}

impl From<MachError> for Error {
    fn from(e: MachError) -> Self {
        // We log the detailed kr+description at error level
        // unconditionally so anyone grep'ing the adapter log sees
        // the Mach detail. Set `BS_DARWIN_DEBUG=1` to also print a
        // stderr backtrace at the conversion site — useful when the
        // caller surfaces a bare `Ptrace(EFAULT)` and you need to
        // see which Mach call started the chain.
        log::error!(target: "darwin_mach", "{}", e);
        if std::env::var_os("BS_DARWIN_DEBUG").is_some() {
            eprintln!("[darwin_mach->Error] {}", e);
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        // KERN_FAILURE on darwin is the *generic* Mach error code —
        // many syscalls return it for unrelated reasons. We only
        // promote it to `DarwinDebuggerEntitlementMissing` if
        // `task_for_pid` has not yet succeeded in this process: if
        // it has, the cs.debugger entitlement is already proven and
        // a later KERN_FAILURE comes from a different Mach call
        // (e.g. `thread_set_arm_debug_state64` during a step) and
        // would mislead the user if reported as a codesign issue.
        if e.0 == 5 && !TASK_FOR_PID_EVER_SUCCEEDED.load(std::sync::atomic::Ordering::Relaxed) {
            // The binary that needs the entitlement is *us* — the
            // running BugStalker binary, not the inferior — so resolve
            // current_exe() and inline its absolute path into the
            // codesign command. Fall back to a placeholder if the
            // syscall fails (rare; e.g. exe deleted from under us).
            let binary = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<path-to-binary>".to_string());
            return Error::DarwinDebuggerEntitlementMissing {
                mach: e.to_string(),
                binary,
            };
        }
        // Preserve the kr verbatim and capture the stack so the
        // user-facing error report itself names the failing Mach
        // call (`thread_set_arm_debug_state64`, `task_resume`,
        // `vm_write_word`, …). Beats blanket-mapping to
        // `Ptrace(EFAULT)` and beats per-callsite instrumentation
        // — one capture covers every Mach call in the codebase.
        Error::DarwinMach {
            mach: e.to_string(),
            backtrace: format!("{}", std::backtrace::Backtrace::force_capture()),
        }
    }
}

#[inline]
fn check(kr: kern_return_t) -> Result<(), MachError> {
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(MachError(kr))
    }
}

/// Resolve a pid to its Mach task port.
///
/// The kernel only allows this across an unrelated `(caller, pid)`
/// pair when the caller has the `com.apple.security.cs.debugger`
/// entitlement — but for a process that called `PT_TRACE_ME`
/// (e.g. a child we forked + execve'd via `Child::install`) it's
/// allowed unconditionally. On failure, `MachError::Display`
/// surfaces the kr name and the most likely cause (typically
/// "missing cs.debugger entitlement on the caller").
///
/// Result is cached per pid for the process lifetime. The kernel
/// `task_for_pid` syscall is heavyweight on darwin (a single
/// global lock) and `read_memory_by_pid` calls it on every memory
/// read — without caching, parallel test runs spend most of their
/// time bouncing on that lock. The first hit per pid does the
/// real syscall; subsequent hits return the cached `task_t`.
/// Entries live until the parent exits; that's fine because the
/// only callers in this codebase are tests with a 1:1 parent-to-
/// inferior relationship and the inferior outlives the cache.
// Module-private pid → task cache. Lifted out of `task_for_pid` so
// the reverse lookup (`task_to_pid`) used by the proc_maps fallback
// in `image_list_from_proc_maps` can scan it.
static TASK_FOR_PID_CACHE: std::sync::Mutex<Option<HashMap<i32, task_t>>> =
    std::sync::Mutex::new(None);

// Set to true the first time `task_for_pid` returns KERN_SUCCESS in
// this process. Used by the `From<MachError> for Error` impl to
// disambiguate KERN_FAILURE: before the flag flips it's almost
// certainly missing-cs.debugger; after it flips, the entitlement is
// already proven and any later KERN_FAILURE is from a different
// Mach call (e.g. `thread_set_state`, `task_resume`) and shouldn't
// be misreported as a codesign issue.
pub(super) static TASK_FOR_PID_EVER_SUCCEEDED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn task_for_pid(pid: Pid) -> Result<task_t, MachError> {
    {
        let guard = TASK_FOR_PID_CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref()
            && let Some(&t) = map.get(&pid.as_raw())
        {
            return Ok(t);
        }
    }
    let mut task: mach_port_t = 0;
    // SAFETY: mach_task_self() is always valid; raw_task_for_pid
    // takes an out-port and writes to it iff KERN_SUCCESS.
    let kr = unsafe { raw_task_for_pid(mach_task_self(), pid.as_raw(), &mut task) };
    if kr != mach2::kern_return::KERN_SUCCESS {
        // `task_for_pid_or_proc` *expects* this to fail for
        // synthetic per-thread pids and retries with the proc pid,
        // so log at debug rather than error to avoid spamming the
        // adapter log on every step.
        log::debug!(target: "darwin_mach",
            "task_for_pid: raw_task_for_pid pid={} failed: {}",
            pid.as_raw(), MachError(kr));
    }
    check(kr)?;
    debug_assert!(
        task != 0,
        "task_for_pid returned KERN_SUCCESS but null port"
    );
    TASK_FOR_PID_EVER_SUCCEEDED.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut guard = TASK_FOR_PID_CACHE.lock().unwrap();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(pid.as_raw(), task);
    Ok(task)
}

/// Resolve a pid (real or synthetic-per-thread) to the owning task.
///
/// Synthetic pids (allocated by `Tracer::reconcile_threads` from
/// `proc_pid + 1_000_000`) aren't real kernel pids — `task_for_pid`
/// rejects them. Memory- and task-level operations on a worker
/// thread should target the *process's* task (it's the same task
/// for every thread in the process). We resolve via the per-pid
/// thread-port registry: the registered thread port came from
/// `task_threads()` of the proc's task, so the proc_pid → task
/// mapping must already be cached. If `pid` is real, falls back to
/// `task_for_pid` directly.
pub fn task_for_pid_or_proc(pid: Pid) -> Result<task_t, MachError> {
    if let Ok(t) = task_for_pid(pid) {
        return Ok(t);
    }
    let proc =
        synthetic_pid_proc(pid).ok_or(MachError(mach2::kern_return::KERN_INVALID_ARGUMENT))?;
    task_for_pid(proc)
}

/// Read `n` bytes from the inferior's address space starting at
/// `addr`. One Mach round-trip regardless of `n`.
pub fn vm_read_n(task: task_t, addr: usize, n: usize) -> Result<Vec<u8>, MachError> {
    let mut buf = vec![0u8; n];
    let mut out_size: mach_vm_size_t = 0;
    // SAFETY: buf is uniquely owned and large enough; the kernel
    // will only write up to its `len` (= `n`).
    let kr = unsafe {
        mach_vm_read_overwrite(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            n as mach_vm_size_t,
            buf.as_mut_ptr() as mach_vm_address_t,
            &mut out_size,
        )
    };
    if kr != mach2::kern_return::KERN_SUCCESS {
        // Log the (addr, len, region prot) before propagating so a
        // KERN_FAILURE here tells us *what we tried to read* and
        // whether the region was even mapped/readable. Cheap when
        // it matters; never runs on the success path.
        let prot = vm_region_protections(task, addr);
        log::error!(target: "darwin_mach",
            "vm_read_n failed: addr=0x{addr:x} len={n} region={prot:?} kr={}",
            MachError(kr));
    }
    check(kr)?;
    buf.truncate(out_size as usize);
    Ok(buf)
}

/// Write a single `usize`-wide word at `addr`. Mirrors the linux
/// `PTRACE_POKEDATA` shape used by `Debugger::write_memory`.
///
/// Code pages are normally `r-x`; we widen them to `rw-` for the
/// duration of the write with `mach_vm_protect(VM_PROT_COPY |
/// VM_PROT_READ | VM_PROT_WRITE)`. The `VM_PROT_COPY` flag turns
/// shared text pages into private CoW copies, which is exactly
/// what we want for software breakpoints — without it, a
/// breakpoint would be visible to other processes mapping the
/// same binary.
///
/// After the write we restore the *original* protection. Earlier
/// versions hardcoded the post-write perms to `R+X` on the
/// assumption that the only caller writes BPs into text; that
/// assumption broke once `Debugger::write_memory` started writing
/// scratch data into `mmap`-allocated `R+W` pages during inferior
/// calls (`call::fmt::call_debug_fmt`). Snapping a data page to
/// `R+X` made every subsequent inferior store to that page raise
/// `KERN_PROTECTION_FAILURE`. Querying `mach_vm_region` once and
/// restoring the page's original protection covers both shapes.
pub fn vm_write_word(task: task_t, addr: usize, value: usize) -> Result<(), Error> {
    let bytes = value.to_ne_bytes();
    let len = mem::size_of::<usize>() as mach_vm_size_t;

    // Pick the post-write protection by what the page is *for*:
    //
    // * If the page was already writable (`cur_prot & W`), it
    //   belongs to a data scratchpad — keep the writable state so
    //   the inferior can keep writing through it.
    // * Otherwise the page is text or shared-cache code — restore
    //   to `R+X` (we CoW'd it through the protect-copy-write cycle
    //   and need it executable for the inferior's next fetch).
    //
    // `max_protection` is *not* a reliable signal here: the dyld
    // shared cache reports `max=R` for pages that are genuinely
    // executable in practice. `cur_protection` distinguishes our
    // two real cases — anonymous `mmap(R|W)` data pages vs.
    // text/shared-cache code pages — so we key off of that.
    let (cur_prot, max_prot) = vm_region_protections(task, addr)
        .unwrap_or((VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE));
    debug_assert!(
        cur_prot & !(VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE) == 0,
        "vm_region_protections returned unexpected cur bits 0x{cur_prot:x}"
    );
    debug_assert!(
        max_prot & !(VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE) == 0,
        "vm_region_protections returned unexpected max bits 0x{max_prot:x}"
    );
    let restore_prot = if cur_prot & VM_PROT_WRITE != 0 {
        // Data scratchpad — keep what we found.
        cur_prot
    } else {
        VM_PROT_READ | VM_PROT_EXECUTE
    };
    let _ = max_prot; // currently informational; only cur drives restore

    // Widen protection to W (CoW). The VM_PROT_COPY bit makes the
    // kernel turn the shared text page into a private CoW copy
    // before applying the new permissions, so the BP we're about
    // to write isn't visible to other processes mapping the same
    // binary. We don't *bail* on a failed protect (many pages are
    // already writable, or the kernel rejects W|COPY for hardened
    // pages but still lets `mach_vm_write` through some other
    // path) — but we *do* capture the result so a later
    // `mach_vm_write` failure knows whether the page was widened.
    let protect_kr = unsafe {
        mach_vm_protect(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            len,
            0,
            VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
        )
    };
    if protect_kr != mach2::kern_return::KERN_SUCCESS {
        log::error!(target: "darwin_mach",
            "vm_write_word: mach_vm_protect(R|W|COPY) at 0x{addr:x} failed: {} (page cur_prot=0x{cur_prot:x} max_prot=0x{max_prot:x})",
            MachError(protect_kr));
    }

    // SAFETY: bytes lives across the call; mach_vm_write copies
    // immediately and doesn't retain the pointer.
    let kr = unsafe {
        mach_vm_write(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            bytes.as_ptr() as mach2::vm_types::vm_offset_t,
            len as u32,
        )
    };
    if kr != mach2::kern_return::KERN_SUCCESS {
        // Surface every diagnostic we have. If the user ever
        // re-hits this path the error message itself names the
        // address, the page protection at entry, and whether the
        // pre-write protect succeeded — enough to triage without
        // attaching another debugger to bs.
        return Err(Error::DarwinMach {
            mach: format!(
                "vm_write_word @ 0x{addr:x} (cur_prot=0x{cur_prot:x} max_prot=0x{max_prot:x} protect_kr=0x{protect_kr:08x}): {}",
                MachError(kr)
            ),
            backtrace: format!("{}", std::backtrace::Backtrace::force_capture()),
        });
    }

    // Apple Silicon has split D/I caches with weak coherency for
    // self-modifying code. `mach_vm_write` updates the D-cache but
    // the inferior's I-cache may still hold the pre-write
    // instruction, so the BRK we just placed never executes —
    // subsequent loop iterations sail past it. Force the I-cache
    // line back in sync by invalidating the range we just wrote
    // *in the inferior's address space*. `sys_icache_invalidate`
    // operates on the calling task's address space, so we map a
    // small read-only window of the inferior's text into our own
    // address space, invalidate from there, then unmap.
    //
    // Cheaper, equivalent path: ARMv8's `dc cvau` + `ic ivau` +
    // `dsb ish` + `isb` sequence executed in the *inferior*. We
    // can't do that without injecting code, so we use the host
    // `sys_icache_invalidate` against a temporary mapping. On
    // x86_64 macOS the call is a no-op (caches are coherent), so
    // the same path is safe to compile unconditionally.
    let _ = invalidate_inferior_icache(task, addr, len as usize);

    // Restore protection. See `restore_prot` selection above.
    let _ = unsafe {
        mach_vm_protect(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            len,
            0,
            restore_prot,
        )
    };
    Ok(())
}

/// Query a VM region for its `(protection, max_protection)` mask
/// pair. The Mach kernel reports both: `protection` is the *current*
/// permission, `max_protection` is the upper bound the page can ever
/// be raised to without re-mapping.
///
/// The caller wants `max_protection` for restoration decisions —
/// `protection` lies for some shared pages (notably the dyld shared
/// cache reports `R` only even though the page is genuinely
/// executable). `max_protection` reflects what the page is *for*:
/// `R+X` for text loaded from disk, `R+W` for an anonymous
/// `mmap(PROT_READ | PROT_WRITE)`, etc.

/// Invalidate the inferior's instruction-cache lines covering
/// `[addr, addr+len)` so the BRK we just wrote via `mach_vm_write`
/// is actually fetched on the inferior's next execute.
///
/// Apple Silicon's I-cache is incoherent with the D-cache; without
/// this, the inferior keeps executing the cached pre-write
/// instructions and our breakpoints are silently no-ops on every
/// path that's already been into I-cache. Symptom: a step_over
/// inside a tight loop body (where the BP at the body's stmt-PC
/// has been disabled-and-restored) advances past the loop instead
/// of stopping on the next iteration's body — a stale I-cache
/// line, modified D-side and never invalidated, holds the
/// disabled (no-BRK) opcode.
///
/// `mach_vm_machine_attribute` with `MATTR_CACHE` +
/// `MATTR_VAL_ICACHE_FLUSH` is the Mach-level primitive lldb uses
/// for the same job. It operates on the *target task's* address
/// space, so we don't need to map the inferior's pages into our
/// own to use `sys_icache_invalidate`.
///
/// On x86_64 this is a no-op at the kernel level (caches are
/// hardware-coherent) so the call is safe to compile for any
/// macOS arch.
fn invalidate_inferior_icache(task: task_t, addr: usize, len: usize) -> Result<(), MachError> {
    use mach2::vm::mach_vm_machine_attribute;
    use mach2::vm_attributes::{MATTR_CACHE, MATTR_VAL_ICACHE_FLUSH, vm_machine_attribute_val_t};
    let mut value: vm_machine_attribute_val_t = MATTR_VAL_ICACHE_FLUSH;
    let kr = unsafe {
        mach_vm_machine_attribute(
            task as mach2::mach_types::vm_task_entry_t,
            addr as mach2::vm_types::mach_vm_address_t,
            len as mach2::vm_types::mach_vm_size_t,
            MATTR_CACHE,
            &mut value as *mut _,
        )
    };
    check(kr)
}

fn vm_region_protections(task: task_t, addr: usize) -> Option<(vm_prot_t, vm_prot_t)> {
    let mut region_addr = addr as mach_vm_address_t;
    let mut region_size: mach_vm_size_t = 0;
    let mut info = vm_region_basic_info_64::default();
    let mut info_count = vm_region_basic_info_64::count();
    let mut object_name: mach_port_t = MACH_PORT_NULL;
    // SAFETY: all out-pointers point at locals that outlive the
    // call; `flavor` is paired with the matching info struct.
    let kr = unsafe {
        mach_vm_region(
            task as vm_task_entry_t,
            &mut region_addr,
            &mut region_size,
            VM_REGION_BASIC_INFO_64,
            &mut info as *mut _ as vm_region_info_t,
            &mut info_count,
            &mut object_name,
        )
    };
    if kr != KERN_SUCCESS {
        return None;
    }
    // `protection` / `max_protection` are `i32` fields in a
    // `repr(C, packed(4))` struct — copy through locals before
    // returning to dodge the unaligned-reference lint.
    let prot = info.protection;
    let max_prot = info.max_protection;
    Some((prot, max_prot))
}

/// Mark a region of the inferior's address space as `R+X`. Used
/// by the inferior-call path to make the freshly-`mmap`-ed
/// trampoline page executable: darwin's W^X policy bars the
/// inferior's `mmap` from requesting `PROT_EXEC` alongside
/// `PROT_WRITE` without `MAP_JIT` (which itself needs an
/// entitlement we don't ship), but a `mach_vm_protect` from the
/// parent task port can flip a freshly-allocated `R+W` anonymous
/// page to `R+X` after we've written the `BLR x8 ; BRK #0`
/// trampoline. Linux skips this entirely — its `mmap` accepts
/// `PROT_EXEC | PROT_WRITE` directly.
pub fn vm_protect_rx(task: task_t, addr: usize, len: usize) -> Result<(), MachError> {
    // mach_vm_protect requires page-aligned addr/len. Both come
    // from the inferior's `mmap` reply in our only caller, so the
    // alignment is structurally guaranteed; assert in debug
    // builds to surface any future caller that violates it.
    debug_assert!(len > 0, "vm_protect_rx with zero length");
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    debug_assert_eq!(
        addr % page_size,
        0,
        "vm_protect_rx addr 0x{addr:x} not page-aligned (page={page_size})"
    );
    debug_assert_eq!(
        len % page_size,
        0,
        "vm_protect_rx len {len} not page-multiple (page={page_size})"
    );
    // SAFETY: addr/len describe a valid mapping in the task; the
    // kernel rejects with KERN_INVALID_ARGUMENT otherwise.
    let kr = unsafe {
        mach_vm_protect(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            len as mach_vm_size_t,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        )
    };
    check(kr)
}

/// Enumerate all Mach thread ports for a task. Used by the
/// equivalent of `/proc/<pid>/task/` enumeration on linux —
/// `Tracer` needs the per-thread ports to read registers / single
/// step.
///
/// The returned array is owned by the task port; today we leak the
/// out-of-line array on success because there's no good
/// `vm_deallocate` wrapper in this module yet. TODO when the
/// thread-list path actually fires more than once per session.
pub fn task_threads_vec(task: task_t) -> Result<Vec<thread_act_t>, MachError> {
    let mut threads: thread_act_array_t = std::ptr::null_mut();
    let mut count: mach_msg_type_number_t = 0;
    // SAFETY: kernel writes both out-pointers iff KERN_SUCCESS.
    let kr = unsafe { task_threads(task, &mut threads, &mut count) };
    check(kr)?;
    // SAFETY: kernel guarantees the array is `count` `thread_act_t`-wide
    // contiguous when it returns success.
    let slice = unsafe { std::slice::from_raw_parts(threads, count as usize) };
    Ok(slice.to_vec())
}

/// Suspend every thread of the task. Each call increments the
/// task's suspend count; pair with `task_resume` 1:1. The Mach
/// equivalent of sending `SIGSTOP`, except suspension is reflected
/// at the kernel level rather than as a delivered signal —
/// useful for the `Tracer::pause` path once we drop ptrace.
pub fn task_suspend(task: task_t) -> Result<(), MachError> {
    // SAFETY: task is a valid task port; task_suspend takes the
    // port and returns a kr.
    let kr = unsafe { mach2::task::task_suspend(task) };
    check(kr)
}

/// Resume the task — decrement its suspend count by one. Mirror
/// of `task_suspend`; if the count was 1, the task starts running
/// again.
pub fn task_resume(task: task_t) -> Result<(), MachError> {
    // SAFETY: task is a valid task port; task_resume returns a kr.
    let kr = unsafe { mach2::task::task_resume(task) };
    check(kr)
}

/// Suspend a single thread. Thread suspend counts are independent
/// of the task suspend count — a thread runs only when *both*
/// counts are zero. Used by the future single-step path: suspend
/// every thread except the one we're stepping, set MDSCR_EL1.SS,
/// resume the chosen thread, wait for an EXC_BREAKPOINT.
pub fn thread_suspend(thread: thread_act_t) -> Result<(), MachError> {
    // SAFETY: thread is a valid thread port.
    let kr = unsafe { mach2::thread_act::thread_suspend(thread) };
    check(kr)
}

/// Resume a single thread — mirror of `thread_suspend`.
pub fn thread_resume(thread: thread_act_t) -> Result<(), MachError> {
    // SAFETY: thread is a valid thread port.
    let kr = unsafe { mach2::thread_act::thread_resume(thread) };
    check(kr)
}

/// Stable 64-bit identifier for a Mach thread.
///
/// Mach thread *ports* (`thread_act_t`) are u32 IPC names — they're
/// unique within our task's port space at one moment, but the
/// kernel may recycle them when threads come and go. The
/// `THREAD_IDENTIFIER_INFO` flavour gives us:
///
/// * `thread_id` — a 64-bit globally-unique-and-stable id (the
///   value `pthread_threadid_np(pth, &id)` returns from inside
///   the inferior),
/// * `thread_handle` — the pthread_t pointer for that thread,
///   which is what we'll deref to walk TLS (`__pthread_t->tsd[]`)
///   and pull thread-local Rust variables out.
///
/// Both fields will be loadbearing for the multi-thread Tracee
/// enumeration; we land the shim now so the call sites are
/// already there when the rest of the wiring catches up.
pub struct ThreadIdentity {
    pub thread_id: u64,
    pub thread_handle: u64,
}

/// `<mach/thread_info.h>` :: `thread_identifier_info_t`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
#[allow(non_camel_case_types)]
struct thread_identifier_info {
    thread_id: u64,
    thread_handle: u64,
    dispatch_qaddr: u64,
}

/// `THREAD_IDENTIFIER_INFO` flavour and the count it expects.
const THREAD_IDENTIFIER_INFO_FLAVOR: u32 = 4;
const THREAD_IDENTIFIER_INFO_COUNT: u32 =
    (mem::size_of::<thread_identifier_info>() / mem::size_of::<u32>()) as u32;

unsafe extern "C" {
    fn thread_info(
        target_act: thread_act_t,
        flavor: u32,
        thread_info_out: *mut u32,
        thread_info_outCnt: *mut u32,
    ) -> kern_return_t;
}

pub fn thread_identity(thread: thread_act_t) -> Result<ThreadIdentity, MachError> {
    let mut info = thread_identifier_info::default();
    let mut count = THREAD_IDENTIFIER_INFO_COUNT;
    // SAFETY: info is sized to match `count`; the kernel writes
    // `count` u32-words into it iff success.
    let kr = unsafe {
        thread_info(
            thread,
            THREAD_IDENTIFIER_INFO_FLAVOR,
            &mut info as *mut _ as *mut u32,
            &mut count,
        )
    };
    check(kr)?;
    Ok(ThreadIdentity {
        thread_id: info.thread_id,
        thread_handle: info.thread_handle,
    })
}

/// Resolve a Mach-O TLV access for a stopped thread.
///
/// Mach-O thread-locals are accessed via a 16-byte `tlv_descriptor`
/// in `__DATA,__thread_vars`. The on-disk layout written by recent
/// linkers + processed by libdyld's `_tlv_get_addr` (verified by
/// disassembling that helper on macOS 14+):
///
/// ```text
///   off  size  field
///   0    8     thunk    (void* (*)(TLVDescriptor*))
///   8    4     key      (uint32_t pthread key)
///   12   4     offset   (uint32_t offset inside tsd[key] block)
/// ```
///
/// (Older docs on the internet describe key/offset as 8-byte fields;
/// this is wrong for current dyld. The hot path of `_tlv_get_addr`
/// is `ldr w16, [x0, #8]` for the key and `ldr w16, [x0, #0xc]` for
/// the offset — both 32-bit loads from byte offsets 8 and 12.)
///
/// After first-touch on a thread, accesses inline to a
/// `pthread_getspecific(key) + offset`, which on darwin/arm64 is:
///
/// ```text
///   mrs Xn, TPIDRRO_EL0
///   bic Xn, Xn, #7
///   ldr Xn, [Xn, #(key * 8)]   ; tsd[key]
///   add Xn, Xn, #offset
/// ```
///
/// `(TPIDRRO_EL0 & ~7)` matches `thread_handle` returned by
/// `THREAD_IDENTIFIER_INFO`: the kernel stores `cthread_self`
/// (libpthread's per-thread TSD base) there and exposes it through
/// that flavour. We replicate the load remotely:
///   1. Read the descriptor from inferior memory.
///   2. Read `tsd[key]` from inferior pthread.
///   3. Return `tsd[key] + offset`, or fail if `tsd[key]` is null
///      (i.e. the variable hasn't been first-touched on this thread
///      yet — replicating `_tlv_bootstrap` would need an inferior
///      call, which the caller can degrade to "no value" instead).
pub fn resolve_tlv(
    task: task_t,
    thread: thread_act_t,
    descriptor_runtime_addr: u64,
) -> Result<u64, MachError> {
    let descriptor = vm_read_n(task, descriptor_runtime_addr as usize, 16)?;
    if descriptor.len() < 16 {
        return Err(MachError(mach2::kern_return::KERN_INVALID_ADDRESS));
    }
    let thunk = u64::from_ne_bytes(descriptor[0..8].try_into().unwrap());
    let key = u32::from_ne_bytes(descriptor[8..12].try_into().unwrap());
    let var_offset = u32::from_ne_bytes(descriptor[12..16].try_into().unwrap());

    // Sanity-check the descriptor before walking the TSD. A wrong
    // slide guess produces nonsense fields (commonly all-zero, since
    // dylibs that don't define this thread_local have zero-padding
    // at the matching __DATA offset). Real TLV descriptors have:
    //   * thunk pointing at `_tlv_get_addr` in libdyld (so >= 0x1000),
    //   * key non-zero and small (pthread key < 1024 in practice),
    //   * offset within the per-thread storage block (< 16 MB).
    if thunk < 0x1000 || key == 0 || key >= 0x1000 || var_offset >= 0x100_0000 {
        return Err(MachError(mach2::kern_return::KERN_INVALID_ARGUMENT));
    }

    let tid = thread_identity(thread)?;
    let tsd_base = tid.thread_handle & !0x7u64;
    if tsd_base == 0 {
        return Err(MachError(mach2::kern_return::KERN_INVALID_ADDRESS));
    }
    let slot_addr = tsd_base
        .checked_add((key as u64) * 8)
        .ok_or(MachError(mach2::kern_return::KERN_INVALID_ARGUMENT))?;
    let slot_bytes = vm_read_n(task, slot_addr as usize, 8)?;
    let tsd_value = u64::from_ne_bytes(slot_bytes[..8].try_into().unwrap());
    if tsd_value == 0 {
        // Uninitialised on this thread. Caller surfaces this as
        // "no TLS value" rather than running `_tlv_get_addr`'s
        // lazy-allocate fallback.
        return Err(MachError(mach2::kern_return::KERN_INVALID_ADDRESS));
    }
    Ok(tsd_value + var_offset as u64)
}

/// Read the aarch64 GP register set for a single Mach thread. Use
/// `task_threads_vec` to get the thread port; for the typical
/// "stopped at a breakpoint, only one thread" case the first
/// element is fine.
pub fn thread_get_arm_state64(thread: thread_act_t) -> Result<arm_thread_state64_t, MachError> {
    let mut state = arm_thread_state64_t::default();
    let mut count = arm_thread_state64_t::count();
    // SAFETY: thread is a valid port; state is sized by `count`
    // which we initialise to the matching value.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64,
            &mut state as *mut _ as *mut u32,
            &mut count,
        )
    };
    check(kr)?;
    Ok(state)
}

// --- ARM_DEBUG_STATE64 (hardware breakpoint + watchpoint registers) -
//
// `<mach/arm/_structs.h>` declares:
//   struct __darwin_arm_debug_state64 {
//       uint64_t __bvr[16];   // breakpoint value
//       uint64_t __bcr[16];   // breakpoint control
//       uint64_t __wvr[16];   // watchpoint value
//       uint64_t __wcr[16];   // watchpoint control
//       uint64_t __mdscr_el1; // single-step / debug-mode control
//   };
//
// mach2 0.6 exposes the `ARM_DEBUG_STATE64` flavor constant but not
// the matching struct, so we lay it out here.

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub struct arm_debug_state64_t {
    pub bvr: [u64; 16],
    pub bcr: [u64; 16],
    pub wvr: [u64; 16],
    pub wcr: [u64; 16],
    pub mdscr_el1: u64,
}

impl arm_debug_state64_t {
    /// Number of `int`s in the struct, as `thread_get_state` /
    /// `thread_set_state` expect.
    pub fn count() -> mach_msg_type_number_t {
        (mem::size_of::<Self>() / mem::size_of::<i32>()) as mach_msg_type_number_t
    }
}

const ARM_DEBUG_STATE64: i32 = 15;

pub fn thread_get_arm_debug_state64(
    thread: thread_act_t,
) -> Result<arm_debug_state64_t, MachError> {
    let mut state = arm_debug_state64_t::default();
    let mut count = arm_debug_state64_t::count();
    // SAFETY: thread valid; state sized by `count`.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_DEBUG_STATE64,
            &mut state as *mut _ as *mut u32,
            &mut count,
        )
    };
    check(kr)?;
    Ok(state)
}

pub fn thread_set_arm_debug_state64(
    thread: thread_act_t,
    state: &arm_debug_state64_t,
) -> Result<(), MachError> {
    let count = arm_debug_state64_t::count();
    // SAFETY: state lives across the call; kernel copies and
    // doesn't retain the pointer.
    let kr = unsafe {
        thread_set_state(
            thread,
            ARM_DEBUG_STATE64,
            state as *const _ as *mut u32,
            count,
        )
    };
    check(kr)?;
    Ok(())
}

/// Snapshot of a task's exception-port chain. Captured by
/// `swap_in_temp_exception_port` before installing a temporary
/// port (the LLDB-style pattern for inferior function calls
/// where the trampoline drives its own exception loop without
/// disturbing the main `Tracer`'s port).
///
/// Pass to `restore_exception_ports` to put the chain back.
/// `EXC_TYPES_COUNT = 14` is the maximum number of distinct
/// exception types — see `<mach/arm/exception.h>`.
pub struct ExceptionPortChain {
    masks: [exception_mask_t; 14],
    handlers: [mach_port_t; 14],
    behaviors: [u32; 14],
    flavors: [i32; 14],
    count: u32,
}

/// Atomically replace the task's exception ports for `mask` with
/// `new_port`, returning the prior chain so the caller can later
/// restore it via `restore_exception_ports`.
///
/// `mach2::task::task_swap_exception_ports` is the kernel's
/// atomic-swap primitive — strictly better than the
/// get-then-set pair LLDB uses, since there's no window where
/// an exception could be misrouted.
pub fn swap_in_temp_exception_port(
    task: task_t,
    mask: exception_mask_t,
    new_port: mach_port_t,
    behavior: u32,
) -> Result<ExceptionPortChain, MachError> {
    let mut masks = [0 as exception_mask_t; 14];
    let mut handlers = [0 as mach_port_t; 14];
    let mut behaviors = [0u32; 14];
    let mut flavors = [0i32; 14];
    let mut count: u32 = 14;
    // SAFETY: arrays sized for `count`; mach2's task_swap_exception_ports
    // signature matches.
    let kr = unsafe {
        mach2::task::task_swap_exception_ports(
            task,
            mask,
            new_port,
            behavior as i32,
            THREAD_STATE_NONE,
            masks.as_mut_ptr(),
            &mut count,
            handlers.as_mut_ptr(),
            behaviors.as_mut_ptr() as *mut i32,
            flavors.as_mut_ptr(),
        )
    };
    check(kr)?;
    Ok(ExceptionPortChain {
        masks,
        handlers,
        behaviors,
        flavors,
        count,
    })
}

/// Reinstall the exception chain captured by
/// `swap_in_temp_exception_port`. Each saved entry is pushed back
/// via `task_set_exception_ports`. Errors on individual entries
/// are logged and skipped — partial restore is better than no
/// restore.
pub fn restore_exception_ports(task: task_t, chain: &ExceptionPortChain) -> Result<(), MachError> {
    for i in 0..(chain.count as usize) {
        // SAFETY: entries within `count` are valid as written
        // by the kernel during the swap.
        let kr = unsafe {
            mach2::task::task_set_exception_ports(
                task,
                chain.masks[i],
                chain.handlers[i],
                chain.behaviors[i] as i32,
                chain.flavors[i],
            )
        };
        if kr != KERN_SUCCESS {
            log::warn!(
                target: "darwin_mach",
                "restore_exception_ports: entry {} failed kr={:#x}",
                i, kr
            );
        }
    }
    Ok(())
}

/// Arm or disarm hardware single-step on a single Mach thread.
///
/// Software single-step on aarch64 is two-bit cooperation between
/// `MDSCR_EL1.SS` (bit 0) — "single-step enable" in the debug
/// state — and `SPSR.SS` (bit 21 of `cpsr`) — "the next ERET
/// should generate a software-step exception". Both must be set
/// before `task_resume` for the kernel to deliver one
/// `EXC_BREAKPOINT`/SS-trap after exactly one instruction.
///
/// Set `enable = true` before stepping; clear via `enable = false`
/// after, otherwise the next normal `task_resume` would also
/// step. (The kernel typically clears `SPSR.SS` on exception
/// entry, but `MDSCR_EL1.SS` is sticky.)
pub fn arm_set_single_step(thread: thread_act_t, enable: bool) -> Result<(), MachError> {
    let mut dbg = thread_get_arm_debug_state64(thread)?;
    if enable {
        dbg.mdscr_el1 |= 1;
    } else {
        dbg.mdscr_el1 &= !1u64;
    }
    thread_set_arm_debug_state64(thread, &dbg)?;

    let mut s = thread_get_arm_state64(thread)?;
    if enable {
        s.__cpsr |= 1 << 21;
    } else {
        s.__cpsr &= !(1u32 << 21);
    }
    thread_set_arm_state64(thread, &s)?;
    Ok(())
}

// --- ARM_EXCEPTION_STATE64 (per-thread fault attribution) ----
//
// `<mach/arm/_structs.h>::__darwin_arm_exception_state64`:
//   uint64_t __far;       // FAR_EL1 — virtual fault address
//   uint32_t __esr;       // ESR_EL1 — exception syndrome
//   uint32_t __exception; // exception class (rough)
//
// The darwin equivalent of linux's `siginfo.si_addr` for a watch-
// point hit is `__far`. ESR carries the syndrome info (read vs
// write, byte-access-select, etc.) but for "which slot fired" the
// FAR is enough — `HardwareDebugState::detect_and_flush_hit`
// matches it against the BAS-encoded byte set of each enabled slot.

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub struct arm_exception_state64_t {
    pub far: u64,
    pub esr: u32,
    pub exception: u32,
}

impl arm_exception_state64_t {
    pub fn count() -> mach_msg_type_number_t {
        (mem::size_of::<Self>() / mem::size_of::<i32>()) as mach_msg_type_number_t
    }
}

const ARM_EXCEPTION_STATE64: i32 = 7;

/// Read the per-thread FAR + ESR. The kernel populates both
/// every time the thread takes a synchronous exception (debug,
/// page fault, alignment, …); they survive until the next
/// exception, so a debugger reads them at the stop and trusts
/// them to describe whatever just fired.
pub fn thread_get_arm_exception_state64(
    thread: thread_act_t,
) -> Result<arm_exception_state64_t, MachError> {
    let mut state = arm_exception_state64_t::default();
    let mut count = arm_exception_state64_t::count();
    // SAFETY: state is sized to `count`.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_EXCEPTION_STATE64,
            &mut state as *mut _ as *mut u32,
            &mut count,
        )
    };
    check(kr)?;
    Ok(state)
}

/// Mirror of `thread_get_arm_state64` for writes.
pub fn thread_set_arm_state64(
    thread: thread_act_t,
    state: &arm_thread_state64_t,
) -> Result<(), MachError> {
    let count = arm_thread_state64_t::count();
    // SAFETY: state lives across the call; the kernel copies and
    // doesn't retain the pointer.
    let kr = unsafe {
        thread_set_state(
            thread,
            ARM_THREAD_STATE64,
            state as *const _ as *mut u32,
            count,
        )
    };
    check(kr)?;
    Ok(())
}

/// Convenience: pick the first thread of `task`. Most early-port
/// codepaths assume single-thread debuggees; the multi-thread
/// flow lands once the exception-port loop is in.
pub fn first_thread_of(task: task_t) -> Result<thread_act_t, MachError> {
    let threads = task_threads_vec(task)?;
    threads
        .first()
        .copied()
        .ok_or(MachError(mach2::kern_return::KERN_FAILURE))
}

// --- Per-pid → thread-port registry --------------------------------
//
// The cross-arch debugger keys everything off `Pid` (a linux tid).
// Darwin doesn't have per-thread pids — threads are addressed by
// `thread_act_t` (a Mach IPC port). The Tracer manufactures a synthetic
// `Pid` for each thread it discovers and stashes the pairing here so
// that callers downstream (`RegisterMap::current`, `unwind`, the
// CallHelper, …) can resolve a `Pid` to its real thread port without
// having to thread (no pun intended) the mapping through every API.
//
// Lifetime is the parent process's: we never compact, but `clear_pid`
// is called on tracee exit. The size is bounded by the inferior's
// thread count, which for the workloads in our test suite is < 32.

pub fn set_thread_port(pid: Pid, port: thread_act_t) {
    let mut g = THREAD_PORT_BY_PID.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(pid.as_raw(), port);
}

/// Record that `synthetic_pid` (a per-thread Pid manufactured by
/// `Tracer::reconcile_threads`) belongs to inferior `proc_pid`.
/// Lookup via `synthetic_pid_proc` lets darwin task / memory APIs
/// fall back to the proc's task port for synthetic ids.
pub fn set_synthetic_pid_proc(synthetic: Pid, proc_pid: Pid) {
    let mut g = SYNTHETIC_PID_PROC.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(synthetic.as_raw(), proc_pid.as_raw());
}

pub fn synthetic_pid_proc(pid: Pid) -> Option<Pid> {
    let g = SYNTHETIC_PID_PROC.lock().unwrap();
    g.as_ref()
        .and_then(|m| m.get(&pid.as_raw()).copied())
        .map(Pid::from_raw)
}

static SYNTHETIC_PID_PROC: std::sync::Mutex<Option<HashMap<i32, i32>>> =
    std::sync::Mutex::new(None);

pub fn thread_port_for_pid(pid: Pid) -> Option<thread_act_t> {
    let g = THREAD_PORT_BY_PID.lock().unwrap();
    g.as_ref().and_then(|m| m.get(&pid.as_raw()).copied())
}

pub fn clear_thread_port(pid: Pid) {
    let mut g = THREAD_PORT_BY_PID.lock().unwrap();
    if let Some(m) = g.as_mut() {
        m.remove(&pid.as_raw());
    }
}

static THREAD_PORT_BY_PID: std::sync::Mutex<Option<HashMap<i32, thread_act_t>>> =
    std::sync::Mutex::new(None);

/// Best-effort thread port for `pid`: consult the registry first, fall
/// back to `first_thread_of(task_for_pid(pid))` for the
/// "single-thread, never registered" hot path that pre-multithreaded
/// callers rely on.
pub fn thread_port_for_pid_or_first(pid: Pid) -> Result<thread_act_t, MachError> {
    if let Some(t) = thread_port_for_pid(pid) {
        return Ok(t);
    }
    let task = task_for_pid(pid)?;
    first_thread_of(task)
}

// --- dyld image list (the Mach equivalent of GNU `r_debug`) ----------
//
// On linux the rendezvous protocol is `r_debug` — a struct ld.so
// maintains in the debuggee whose `link_map` field is the head of a
// linked list of loaded shared objects. On darwin, dyld publishes the
// equivalent in a slightly different shape:
//
//   1. `task_info(task, TASK_DYLD_INFO, …)` returns a virtual address
//      in the debuggee pointing at a `dyld_all_image_infos` struct.
//   2. That struct's `infoArray` field points at a contiguous array of
//      `dyld_image_info` records (one per loaded image), sized by
//      `infoArrayCount`.
//   3. Each `dyld_image_info` carries `imageLoadAddress` (the
//      `mach_header` virtual address of the image) and
//      `imageFilePath` (a NUL-terminated debuggee VA of the image's
//      filesystem path).
//
// We don't go through `mach2::structs` for these because they're not
// exposed there — the layout below mirrors `<mach-o/dyld_images.h>`.

#[repr(C)]
#[derive(Copy, Clone, Default)]
#[allow(non_camel_case_types)]
struct dyld_all_image_infos_v1 {
    version: u32,
    info_array_count: u32,
    info_array: u64, // *const dyld_image_info in the debuggee
    /// Function pointer dyld calls on every image add/remove. The
    /// linux equivalent is the `r_brk` field of the `r_debug`
    /// rendezvous struct: install a software BP at this address
    /// and dyld will trap into us each time an image enters or
    /// leaves the process.
    notification: u64,
    /// `processDetachedFromSharedRegion` (bool) + `libSystemInitialized`
    /// (bool) + 6 bytes of padding → 8 bytes. We only read this struct
    /// to extract `notification` and `dyld_image_load_address`, so we
    /// pack the two booleans + alignment into one u64 and never read
    /// it back.
    _flags_and_padding: u64,
    /// Virtual address of dyld itself (the real `/usr/lib/dyld`,
    /// not `libdyld.dylib`). Available when `version >= 2`. dyld
    /// does NOT include itself in `infoArray`, so this is the only
    /// way to find dyld's slid load address — needed to range-check
    /// PCs that fall inside dyld pages (e.g. the `_lldb_image_notifier`
    /// trap point that fires on every dlopen). 0 if `version < 2`.
    dyld_image_load_address: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
#[allow(non_camel_case_types)]
struct dyld_image_info {
    image_load_address: u64, // *const mach_header
    image_file_path: u64,    // *const c_char (NUL-term)
    image_file_mod_date: u64,
}

/// One entry in dyld's loaded-image list.
pub struct ImageInfo {
    /// Virtual address of the image's `mach_header` in the debuggee.
    pub load_addr: usize,
    /// Filesystem path of the image (read out of the debuggee).
    pub path: String,
}

/// Resolve the address of the debuggee's `dyld_all_image_infos`
/// struct via `task_info(TASK_DYLD_INFO)`.
fn task_dyld_all_image_infos_addr(task: task_t) -> Result<u64, MachError> {
    use mach2::task::task_info;
    use mach2::task_info::{TASK_DYLD_INFO, TASK_DYLD_INFO_COUNT, task_dyld_info};

    let mut info = task_dyld_info::default();
    let mut count = TASK_DYLD_INFO_COUNT;
    // SAFETY: kernel writes `info` iff success.
    let kr = unsafe {
        task_info(
            task,
            TASK_DYLD_INFO,
            &mut info as *mut _ as *mut i32,
            &mut count,
        )
    };
    check(kr)?;
    Ok(info.all_image_info_addr as u64)
}

/// Read `n` bytes at `addr` and decode them as `T`. Used to slurp
/// fixed-layout structs out of the debuggee.
fn read_struct<T: Copy + Default>(task: task_t, addr: u64) -> Result<T, MachError> {
    let bytes = vm_read_n(task, addr as usize, mem::size_of::<T>())?;
    let mut out = T::default();
    // SAFETY: bytes is exactly sizeof::<T> long; T is repr(C) and
    // Default for our internal types means all-zero, which is a
    // valid bit pattern for the integer fields they contain.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            &mut out as *mut T as *mut u8,
            mem::size_of::<T>(),
        );
    }
    Ok(out)
}

/// Read a NUL-terminated C string from the debuggee.
fn read_cstr(task: task_t, addr: u64, max_len: usize) -> Result<String, MachError> {
    let bytes = vm_read_n(task, addr as usize, max_len)?;
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..len]).into_owned())
}

// --- Mach exception ports --------------------------------------------
//
// The `Tracer::resume` darwin path in this crate uses a ptrace+SIGTRAP
// shortcut that works for single-thread debuggees but races with the
// debuggee's natural progression after a breakpoint hit. The
// LLDB-grade replacement is a Mach-native exception-port loop:
//
//   1. Allocate a Mach receive port (this struct's
//      `ExceptionPort::allocate`).
//   2. Insert a send right onto the same name so we can hand it to
//      `task_set_exception_ports`.
//   3. Subscribe to `EXC_MASK_BREAKPOINT | EXC_MASK_SOFTWARE |
//      EXC_MASK_BAD_ACCESS` on the debuggee's task port — when any
//      of those exceptions fires in the debuggee the kernel posts a
//      `mach_exception_raise` message to our port instead of
//      delivering a UNIX signal (this is how lldb captures
//      breakpoints with multi-thread discipline).
//   4. (Iteration 10) `mach_msg` to receive + decode the raised
//      exception, translate it to `StopReason`, and reply with
//      `KERN_SUCCESS` (continue) or a non-success kr (let the
//      kernel deliver the exception to the next handler).
//
// This iteration lands the port allocation + registration only;
// the receive loop and the Tracer wiring are the next chunk.

/// Owned Mach exception port. Drop it and the kernel reaps the
/// receive right (the send right we duplicated for the task is
/// also released because both share the same name in this task's
/// IPC space).
pub struct ExceptionPort {
    port: mach_port_name_t,
}

impl ExceptionPort {
    /// Allocate a fresh receive port + send right in our own task,
    /// suitable for handing to `task_set_exception_ports`.
    pub fn allocate() -> Result<Self, MachError> {
        let mut port: mach_port_name_t = MACH_PORT_NULL;
        // SAFETY: mach_task_self() is always valid; mach_port_allocate
        // writes to `port` iff KERN_SUCCESS.
        let kr =
            unsafe { mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
        check(kr)?;
        // Add a send right onto the same name so we can hand it to
        // task_set_exception_ports without dropping our own receive.
        // SAFETY: port is the name we just allocated.
        let kr = unsafe {
            mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND)
        };
        check(kr)?;
        Ok(Self { port })
    }

    /// Register this port for the standard debuggee-relevant
    /// exception set on `task`. The 64-bit `MACH_EXCEPTION_CODES`
    /// variant is requested so we get full-width fault addresses
    /// from `EXC_BAD_ACCESS` etc.
    pub fn register(&self, task: task_t) -> Result<(), MachError> {
        // EXC_MASK_BREAKPOINT — software-bp BRK and userland traps
        // EXC_MASK_SOFTWARE — debuggee's __builtin_trap and friends
        // EXC_MASK_BAD_ACCESS — segfaults, so the debugger can stop
        //                       at the fault rather than letting the
        //                       process die silently
        let mask: exception_mask_t = EXC_MASK_BREAKPOINT | EXC_MASK_SOFTWARE | EXC_MASK_BAD_ACCESS;
        let behavior: u32 = (EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES) as u32;
        // SAFETY: task and port are valid mach_port_t values.
        let kr = unsafe {
            task_set_exception_ports(task, mask, self.port, behavior as i32, THREAD_STATE_NONE)
        };
        check(kr)?;
        Ok(())
    }

    /// Underlying port name.
    #[allow(dead_code)]
    pub fn name(&self) -> mach_port_name_t {
        self.port
    }

    /// Block on the port until either an exception arrives or
    /// `timeout_ms` elapses. Returns `Ok(None)` on timeout.
    ///
    /// We decode the `mach_exception_raise` (message ID 2405)
    /// variant — the 64-bit-codes flavour, since that's the
    /// behaviour we asked for in `register`. Field offsets follow
    /// `<mach/exc.defs>`'s generated `__Request__mach_exception_raise_t`
    /// layout. The buffer is sized for the worst-case modern
    /// message (≤ 256 bytes); the kernel writes only as many bytes
    /// as the actual message needs.
    pub fn receive(&self, timeout_ms: u32) -> Result<Option<ReceivedException>, MachError> {
        const RECEIVE_BUF: usize = 256;
        let mut buf = [0u8; RECEIVE_BUF];
        // SAFETY: buf is large enough for the message; mach_msg
        // writes at most `recv_size` bytes into it. We pass our
        // owned port for `recv_name`. Notify port is null because
        // we don't want a notification.
        let kr = unsafe {
            mach_msg(
                buf.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                RECEIVE_BUF as u32,
                self.port,
                timeout_ms,
                MACH_PORT_NULL,
            )
        };
        if kr == MACH_RCV_TIMED_OUT {
            return Ok(None);
        }
        check(kr)?;

        // Header (24 bytes):
        //   bits         u32  @ 0
        //   size         u32  @ 4
        //   remote_port  u32  @ 8
        //   local_port   u32  @ 12
        //   voucher_port u32  @ 16
        //   id           i32  @ 20
        //
        // Body (4 bytes):
        //   descriptor_count u32  @ 24
        //
        // mach_msg_port_descriptor_t × 2  (12 bytes each on 64-bit):
        //   thread.name  u32 @ 28
        //   thread.pad1  u32 @ 32
        //   thread.pad2_disp_type u32 @ 36   // bitfield: pad2:16|disp:8|type:8
        //   task.name    u32 @ 40
        //   task.pad1    u32 @ 44
        //   task.pad2_disp_type u32 @ 48
        //
        // NDR_record_t  8 bytes @ 52..60
        //
        // exception     i32 @ 60
        // codeCnt       u32 @ 64
        // code[0]       i64 @ 68
        // code[1]       i64 @ 76
        let id = i32::from_ne_bytes(buf[20..24].try_into().unwrap());
        let remote_port = u32::from_ne_bytes(buf[8..12].try_into().unwrap());
        let thread_name = u32::from_ne_bytes(buf[28..32].try_into().unwrap());
        let task_name = u32::from_ne_bytes(buf[40..44].try_into().unwrap());
        let exception = i32::from_ne_bytes(buf[60..64].try_into().unwrap());
        let code_cnt = u32::from_ne_bytes(buf[64..68].try_into().unwrap()) as usize;
        let mut codes = Vec::with_capacity(code_cnt);
        for i in 0..code_cnt.min(2) {
            let off = 68 + i * 8;
            codes.push(i64::from_ne_bytes(buf[off..off + 8].try_into().unwrap()));
        }

        Ok(Some(ReceivedException {
            msg_id: id,
            remote_port,
            thread_port: thread_name,
            task_port: task_name,
            exception,
            codes,
        }))
    }

    /// Acknowledge an exception. The kernel parks the faulted thread
    /// until we send a reply on the request's `remote_port`; what
    /// we put in the `kern_return_t` field determines what happens
    /// next:
    ///
    /// * `KERN_SUCCESS` (0) — we handled it, please resume the
    ///   thread (after we've adjusted PC, fixed memory, etc.).
    /// * `KERN_FAILURE` (5) — let the next handler in the chain
    ///   take this; this is what we'd send for an exception we
    ///   didn't subscribe to but somehow got. Equivalent to
    ///   ptrace's "transparent passthrough" of an unwanted signal.
    ///
    /// The reply layout is the `__Reply__mach_exception_raise_t`
    /// generated from `<mach/exc.defs>`: header (24) + NDR (8) +
    /// kern_return_t (4) = 36 bytes total. The reply id is always
    /// `request_id + 100` (Mach RPC convention).
    ///
    /// `remote_port` came as a SEND_ONCE right; we move it back to
    /// the kernel by tagging the bits as `MOVE_SEND_ONCE`.
    pub fn reply(
        remote_port: u32,
        request_msg_id: i32,
        retcode: kern_return_t,
    ) -> Result<(), MachError> {
        const REPLY_LEN: usize = 36;
        let mut buf = [0u8; REPLY_LEN];

        // Header.
        let bits: u32 = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
        buf[0..4].copy_from_slice(&bits.to_ne_bytes());
        buf[4..8].copy_from_slice(&(REPLY_LEN as u32).to_ne_bytes());
        buf[8..12].copy_from_slice(&remote_port.to_ne_bytes());
        // local_port @ 12 = MACH_PORT_NULL (already zero).
        // voucher_port @ 16 = 0 (already zero).
        let reply_id: i32 = request_msg_id + 100;
        buf[20..24].copy_from_slice(&reply_id.to_ne_bytes());

        // NDR_record @ 24..32: zeros are fine for a single integer.

        // kern_return_t @ 32..36.
        buf[32..36].copy_from_slice(&retcode.to_ne_bytes());

        // SAFETY: buf holds a fully-formed reply message; we ask
        // mach_msg to send it with no receive part. We use a short
        // timeout so a misconfigured caller (e.g. dead remote port)
        // can't wedge the tracer.
        let kr = unsafe {
            mach_msg(
                buf.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                REPLY_LEN as u32,
                0,
                MACH_PORT_NULL,
                /* timeout_ms = */ 100,
                MACH_PORT_NULL,
            )
        };
        check(kr)
    }
}

/// Decoded `mach_exception_raise` message body.
///
/// The Mach kernel posts one of these to our subscribed port when
/// the debuggee raises an EXC_BREAKPOINT / EXC_SOFTWARE /
/// EXC_BAD_ACCESS exception. The Tracer translates it into a
/// `StopReason`; the response is sent by `ExceptionPort::reply`.
pub struct ReceivedException {
    /// Original message ID. For our subscription this is always
    /// 2405 (`mach_exception_raise`); we keep it because we need
    /// to compute the reply message ID = `msg_id + 100`.
    pub msg_id: i32,
    /// `msgh_remote_port` from the request header — we send the
    /// reply *back to* this port.
    pub remote_port: u32,
    /// Mach thread port that raised the exception.
    pub thread_port: u32,
    /// Mach task port containing that thread.
    pub task_port: u32,
    /// Exception type: EXC_BREAKPOINT (6), EXC_SOFTWARE (5),
    /// EXC_BAD_ACCESS (1), …
    pub exception: i32,
    /// Exception-type-specific codes. For EXC_BREAKPOINT on
    /// aarch64, `codes\[0\]` is the BRK immediate, `codes\[1\]`
    /// is `0`. For EXC_BAD_ACCESS, `codes\[0\]` is the
    /// `kern_return_t` reason (e.g. `KERN_INVALID_ADDRESS`),
    /// `codes\[1\]` is the fault address.
    pub codes: Vec<i64>,
}

impl Drop for ExceptionPort {
    fn drop(&mut self) {
        if self.port != MACH_PORT_NULL {
            // SAFETY: we own `self.port`; destroying it releases
            // both the receive and send rights we hold under that
            // name in our task's IPC space.
            unsafe {
                let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), self.port);
            }
        }
    }
}

/// Address of dyld's image-load/unload notification function in the
/// debuggee. Install a software breakpoint here and dyld will stop
/// the inferior every time it enters this routine to announce a
/// `dlopen` / `dlclose` (mode is in `x0`, count in `x1`,
/// info-array pointer in `x2` — AAPCS64).
///
/// Returns `Ok(0)` if dyld hasn't yet populated the field — that
/// happens transiently between exec and dyld's first run; callers
/// should retry after the first stop.
///
/// This is the macOS analogue of the linux `r_debug.r_brk` pointer
/// that `Rendezvous::r_brk` installs a BP at on the linux side.
pub fn dyld_notification_addr(task: task_t) -> Result<u64, MachError> {
    let infos_addr = task_dyld_all_image_infos_addr(task)?;
    let header: dyld_all_image_infos_v1 = read_struct(task, infos_addr)?;
    // Strip PAC bits if present. On arm64e, function pointers stored
    // in `dyld_all_image_infos.notification` are normally PAC-signed
    // (paciza). For arm64 inferiors the dyld stores the bare 47-bit
    // VA, but masking is harmless either way. We use 47-bit VA because
    // Apple Silicon uses 47-bit user VAs with PAC bits in [47..62].
    const VA_MASK: u64 = (1u64 << 47) - 1;
    Ok(header.notification & VA_MASK)
}

/// Slid virtual address of dyld itself (the real `/usr/lib/dyld`).
/// Read from `dyld_all_image_infos.dyldImageLoadAddress` (available
/// when `version >= 2` — every supported macOS does). Returns `Ok(0)`
/// if dyld hasn't yet populated the field.
///
/// dyld does *not* list itself in `infoArray`, so callers that need
/// to know whether a stopped PC is inside dyld pages have to ask
/// here rather than walking the image list.
pub fn dyld_self_load_addr(task: task_t) -> Result<u64, MachError> {
    let infos_addr = task_dyld_all_image_infos_addr(task)?;
    let header: dyld_all_image_infos_v1 = read_struct(task, infos_addr)?;
    if header.version < 2 {
        return Ok(0);
    }
    Ok(header.dyld_image_load_address)
}

// --- dyld_process_info_notify Mach IPC -------------------------------
//
// The legacy "set a SW BP at `_lldb_image_notifier`" protocol is a poor
// fit on darwin/aarch64 because (a) the function lives in the dyld
// shared cache, where text-page CoW + cross-core I-cache invalidation
// are unreliable through `mach_vm_write`, and (b) dyld has shipped a
// purpose-built Mach-IPC protocol for the same purpose since Big Sur.
// We use the latter.
//
// Wire protocol — all fields little-endian, source of truth is
// [`apple-oss-distributions/dyld:libdyld/dyld_process_info_internal.h`](
//   https://github.com/apple-oss-distributions/dyld/blob/main/libdyld/dyld_process_info_internal.h):
//
//   #define DYLD_PROCESS_INFO_NOTIFY_LOAD_ID           0x1000
//   #define DYLD_PROCESS_INFO_NOTIFY_UNLOAD_ID         0x2000
//   #define DYLD_PROCESS_INFO_NOTIFY_MAIN_ID           0x3000
//   #define DYLD_PROCESS_EVENT_ID_BASE                 0x4000
//   #define DYLD_PROCESS_INFO_NOTIFY_MAX_BUFFER_SIZE   (32*1024)
//
//   struct dyld_process_info_notify_header {
//       mach_msg_header_t header;       // 24 bytes
//       uint32_t version;
//       uint32_t imageCount;
//       uint32_t imagesOffset;
//       uint32_t stringsOffset;
//       uint64_t timestamp;
//   };                                  // 24 + 24 = 48 bytes
//
//   struct dyld_process_info_image_entry {
//       uuid_t uuid;                    // 16 bytes
//       uint64_t loadAddress;
//       uint32_t pathStringOffset;
//       uint32_t pathLength;
//   };                                  // 32 bytes
//
// Registration: pass a Mach send right to the inferior task via
// `task_dyld_process_info_notify_register(task, port)`. dyld in the
// inferior writes to `_allImageInfo->notifyPorts[i]`; on every
// `triggerNotifications()` it constructs a notify message and sends
// it to our port. We poll for those.

const DYLD_PROCESS_INFO_NOTIFY_LOAD_ID: i32 = 0x1000;
const DYLD_PROCESS_INFO_NOTIFY_UNLOAD_ID: i32 = 0x2000;

/// One image in a dyld notify message — what dyld is telling us about.
#[derive(Debug, Clone)]
pub struct DyldNotifyImage {
    pub load_addr: u64,
    pub path: String,
}

/// A decoded dyld notify message. Every message dyld sends here is
/// SYNCHRONOUS — `RemoteNotificationResponder::sendMessage` uses
/// `MACH_SEND_MSG | MACH_RCV_MSG`, so dyld blocks until we deliver
/// a reply on the included `remote_port` (a SEND_ONCE right). The
/// caller must invoke [`DyldNotifyPort::reply_to_event`] on every
/// arriving message — including Load/Unload — or the inferior wedges.
#[derive(Debug)]
pub enum DyldNotifyMsg {
    Load {
        images: Vec<DyldNotifyImage>,
        remote_port: u32,
        msg_id: i32,
    },
    Unload {
        images: Vec<DyldNotifyImage>,
        remote_port: u32,
        msg_id: i32,
    },
    /// A non-image-list event (e.g. dyld-before-initializers,
    /// main-called, atlas-changed, shared-cache-mapped).
    Event { remote_port: u32, msg_id: i32 },
}

unsafe extern "C" {
    /// Register a Mach port to receive image-load notifications from
    /// dyld in `task`. The port must carry a send right.
    /// Declared in `<mach/task.h>` (private — not in the public SDK
    /// but exported from libsystem_kernel).
    fn task_dyld_process_info_notify_register(
        task: task_t,
        port: mach_port_name_t,
    ) -> kern_return_t;

    /// Reverse of `_register`. Best-effort cleanup at Drop time.
    #[allow(dead_code)]
    fn task_dyld_process_info_notify_deregister(
        task: task_t,
        port: mach_port_name_t,
    ) -> kern_return_t;
}

/// Owned Mach port subscribed to dyld's load/unload notifications for
/// a specific task. Registration is per-task; one port can serve at
/// most one task at a time (dyld stores the send right in the
/// inferior's `dyld_all_image_infos.notifyPorts[]` array).
pub struct DyldNotifyPort {
    port: mach_port_name_t,
    /// Task this port is registered with — kept so `Drop` can call
    /// the deregister RPC. `None` if registration hasn't happened
    /// yet or has already been undone.
    registered_task: Option<task_t>,
}

impl DyldNotifyPort {
    /// Allocate a fresh receive port + send right in our own task.
    /// The send right is what we hand to dyld via
    /// `task_dyld_process_info_notify_register`.
    pub fn allocate() -> Result<Self, MachError> {
        let mut port: mach_port_name_t = MACH_PORT_NULL;
        let kr =
            unsafe { mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
        check(kr)?;
        let kr = unsafe {
            mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND)
        };
        check(kr)?;
        Ok(Self {
            port,
            registered_task: None,
        })
    }

    /// Hand the send right to dyld in `task`. After this returns,
    /// `triggerNotifications()` calls in the inferior will deliver
    /// messages to this port.
    pub fn register(&mut self, task: task_t) -> Result<(), MachError> {
        let kr = unsafe { task_dyld_process_info_notify_register(task, self.port) };
        check(kr)?;
        self.registered_task = Some(task);
        Ok(())
    }

    /// Non-blocking poll for one notification message.
    ///
    /// Returns `Ok(None)` when nothing was queued. The caller owns
    /// the loop — call repeatedly to drain the backlog (dyld emits
    /// one message per `triggerNotifications` call, and on inferior
    /// startup that's once per statically-linked image).
    pub fn poll(&self, timeout_ms: u32) -> Result<Option<DyldNotifyMsg>, MachError> {
        // The maximum dyld message is 32 KiB; we size for that plus
        // the audit trailer. mach_msg writes only as many bytes as
        // the actual message takes.
        const RECEIVE_BUF: usize = 32 * 1024 + 256;
        let mut buf = vec![0u8; RECEIVE_BUF];
        let kr = unsafe {
            mach_msg(
                buf.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                RECEIVE_BUF as u32,
                self.port,
                timeout_ms,
                MACH_PORT_NULL,
            )
        };
        if kr == MACH_RCV_TIMED_OUT {
            return Ok(None);
        }
        check(kr)?;

        // Header (24 bytes):
        //   bits          u32 @ 0
        //   size          u32 @ 4
        //   remote_port   u32 @ 8
        //   local_port    u32 @ 12
        //   voucher_port  u32 @ 16
        //   id            i32 @ 20
        let msg_size = u32::from_ne_bytes(buf[4..8].try_into().unwrap()) as usize;
        let remote_port = u32::from_ne_bytes(buf[8..12].try_into().unwrap());
        let msg_id = i32::from_ne_bytes(buf[20..24].try_into().unwrap());

        // For events (synchronous block-on-event), the body is just
        // the header — caller must reply for dyld to unblock.
        if msg_id != DYLD_PROCESS_INFO_NOTIFY_LOAD_ID
            && msg_id != DYLD_PROCESS_INFO_NOTIFY_UNLOAD_ID
        {
            return Ok(Some(DyldNotifyMsg::Event {
                remote_port,
                msg_id,
            }));
        }

        // Notify header body starts at offset 24 (right after the
        // mach_msg header). Layout:
        //   version       u32 @ 24
        //   imageCount    u32 @ 28
        //   imagesOffset  u32 @ 32  (from start of message buffer)
        //   stringsOffset u32 @ 36
        //   timestamp     u64 @ 40
        if msg_size < 48 {
            return Err(MachError(mach2::kern_return::KERN_INVALID_ARGUMENT));
        }
        let image_count = u32::from_ne_bytes(buf[28..32].try_into().unwrap()) as usize;
        let images_offset = u32::from_ne_bytes(buf[32..36].try_into().unwrap()) as usize;
        let strings_offset = u32::from_ne_bytes(buf[36..40].try_into().unwrap()) as usize;
        if images_offset > msg_size || strings_offset > msg_size {
            return Err(MachError(mach2::kern_return::KERN_INVALID_ARGUMENT));
        }

        // Per-image entries (32 bytes each, layout in the comment
        // block above). We skip the uuid (debugger doesn't need it
        // for deferred-BP resolution) and read load address + path.
        const ENTRY_SIZE: usize = 32;
        const ENTRY_LOAD_OFF: usize = 16;
        const ENTRY_PATH_OFF_OFF: usize = 24;
        const ENTRY_PATH_LEN_OFF: usize = 28;
        let mut images = Vec::with_capacity(image_count);
        for i in 0..image_count {
            let off = images_offset + i * ENTRY_SIZE;
            if off + ENTRY_SIZE > msg_size {
                break;
            }
            let load_addr = u64::from_ne_bytes(
                buf[off + ENTRY_LOAD_OFF..off + ENTRY_LOAD_OFF + 8]
                    .try_into()
                    .unwrap(),
            );
            let path_off = u32::from_ne_bytes(
                buf[off + ENTRY_PATH_OFF_OFF..off + ENTRY_PATH_OFF_OFF + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let path_len = u32::from_ne_bytes(
                buf[off + ENTRY_PATH_LEN_OFF..off + ENTRY_PATH_LEN_OFF + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let path_start = strings_offset + path_off;
            let path_end = path_start.saturating_add(path_len).min(msg_size);
            let path = if path_start < path_end {
                String::from_utf8_lossy(&buf[path_start..path_end])
                    .trim_end_matches('\0')
                    .to_string()
            } else {
                String::new()
            };
            images.push(DyldNotifyImage { load_addr, path });
        }

        Ok(Some(if msg_id == DYLD_PROCESS_INFO_NOTIFY_LOAD_ID {
            DyldNotifyMsg::Load {
                images,
                remote_port,
                msg_id,
            }
        } else {
            DyldNotifyMsg::Unload {
                images,
                remote_port,
                msg_id,
            }
        }))
    }

    /// Send a one-shot reply to a synchronous event message so the
    /// inferior's `RemoteNotificationResponder::blockOnSynchronousEvent`
    /// returns. Body is just the empty Mach header — dyld doesn't
    /// inspect the reply contents, only that it arrived.
    pub fn reply_to_event(remote_port: u32, msg_id: i32) -> Result<(), MachError> {
        const REPLY_LEN: usize = 24;
        let mut buf = [0u8; REPLY_LEN];
        let bits: u32 = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
        buf[0..4].copy_from_slice(&bits.to_ne_bytes());
        buf[4..8].copy_from_slice(&(REPLY_LEN as u32).to_ne_bytes());
        buf[8..12].copy_from_slice(&remote_port.to_ne_bytes());
        let reply_id: i32 = msg_id + 100;
        buf[20..24].copy_from_slice(&reply_id.to_ne_bytes());
        let kr = unsafe {
            mach_msg(
                buf.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                REPLY_LEN as u32,
                0,
                MACH_PORT_NULL,
                /* timeout_ms = */ 100,
                MACH_PORT_NULL,
            )
        };
        check(kr)
    }
}

impl Drop for DyldNotifyPort {
    fn drop(&mut self) {
        if let Some(task) = self.registered_task.take() {
            // SAFETY: deregister is best-effort; the task may have
            // exited, in which case the kernel has already cleaned up.
            unsafe {
                let _ = task_dyld_process_info_notify_deregister(task, self.port);
            }
        }
        if self.port != MACH_PORT_NULL {
            unsafe {
                let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), self.port);
            }
        }
    }
}

/// Fallback for `dyld_image_list` when dyld's legacy `infoArray` is
/// empty (typical for already-running darwin processes on dyld 4 /
/// macOS 14+). Walks the inferior's VM mappings and emits one
/// `ImageInfo` per unique `(file, lowest start)` pair, with the main
/// executable returned first (so `link_map_main()` keeps its
/// "first entry is the main image" contract). Returns the same
/// shape as the dyld-image-walk path so the caller doesn't need to
/// distinguish.
fn image_list_from_proc_maps(task: task_t) -> Result<Vec<ImageInfo>, MachError> {
    use proc_maps::get_process_maps;
    use std::collections::HashMap;
    use std::path::Path;

    // Find the inferior's pid for proc_maps. We don't have a
    // task→pid Mach API, but the cache is populated keyed on
    // pid → task, so we walk the cache. (The cache is the only
    // persistent task→pid relationship in this crate.)
    let pid = match task_to_pid(task) {
        Some(pid) => pid,
        None => return Ok(Vec::new()),
    };
    let maps = match get_process_maps(pid.as_raw()) {
        Ok(m) => m,
        Err(_) => return Ok(Vec::new()),
    };

    // Group ranges by filename; for each filename keep the lowest
    // start address. proc_maps on darwin emits one MapRange per
    // segment per image, so the lowest-start entry per file is the
    // mach_header of that image.
    let mut by_file: HashMap<String, u64> = HashMap::new();
    for r in &maps {
        let Some(path) = r.filename() else { continue };
        let path_s = path.to_string_lossy().to_string();
        if path_s.is_empty() {
            continue;
        }
        // Skip the kernel-internal special mappings.
        if path_s.contains("[shared cache]") {
            continue;
        }
        let entry = by_file.entry(path_s).or_insert(u64::MAX);
        if (r.start() as u64) < *entry {
            *entry = r.start() as u64;
        }
    }

    // Identify the main executable. proc_pidinfo on darwin reports
    // the main exec's path as the first non-shared-cache region;
    // the `proc_maps::get_process_maps` order isn't guaranteed, so
    // we cross-check via `sysinfo`.
    let main_exec = {
        use sysinfo::{RefreshKind, System};
        let sys =
            System::new_with_specifics(RefreshKind::everything().without_cpu().without_memory());
        sysinfo::System::process(&sys, sysinfo::Pid::from_u32(pid.as_raw() as u32))
            .and_then(|p| p.exe().map(|p| p.to_string_lossy().to_string()))
    };

    let mut out: Vec<ImageInfo> = Vec::with_capacity(by_file.len());
    if let Some(exe) = main_exec.as_deref()
        && let Some(addr) = by_file.remove(exe)
    {
        out.push(ImageInfo {
            load_addr: addr as usize,
            path: exe.to_string(),
        });
    }
    // Sort the remaining images by address for stable iteration
    // order (the rendezvous protocol doesn't promise a specific
    // order, but tests sometimes assume a stable list).
    let mut rest: Vec<(String, u64)> = by_file.into_iter().collect();
    rest.sort_by_key(|(_, addr)| *addr);
    for (path, addr) in rest {
        // Skip what's clearly not an image (e.g. anonymous zero-fill
        // pages can sneak in with arbitrary names).
        if !Path::new(&path).is_absolute() {
            continue;
        }
        out.push(ImageInfo {
            load_addr: addr as usize,
            path,
        });
    }
    Ok(out)
}

/// Reverse `task_for_pid`: given a `task_t` we previously resolved
/// from a pid, return that pid. Used by the proc_maps fallback
/// in `image_list_from_proc_maps` so we can ask `proc_pidinfo` for
/// the inferior's mappings; there's no Mach API that goes from a
/// task port back to a pid, so we walk our own cache.
fn task_to_pid(task: task_t) -> Option<Pid> {
    let g = TASK_FOR_PID_CACHE.lock().ok()?;
    let map = g.as_ref()?;
    for (&p, &t) in map.iter() {
        if t == task {
            return Some(Pid::from_raw(p));
        }
    }
    None
}

/// Walk the dyld image list and return one `ImageInfo` per loaded
/// image. The first entry is conventionally the main executable.
pub fn dyld_image_list(task: task_t) -> Result<Vec<ImageInfo>, MachError> {
    let infos_addr = task_dyld_all_image_infos_addr(task)?;
    let header: dyld_all_image_infos_v1 = read_struct(task, infos_addr)?;
    let count = header.info_array_count as usize;
    if count == 0 || header.info_array == 0 {
        // dyld 4 (macOS 14+) doesn't always maintain the legacy
        // `infoArray` for already-running processes — it's
        // populated during dyld's startup phase and may be cleared
        // afterwards in favour of the compact format. Fall back to
        // walking the process's VM mappings: each unique mach_header
        // there is an image. The caller's `Rendezvous::link_maps()`
        // re-walks on every dlopen notification anyway, so any later
        // loads come through `DyldNotifyPort` regardless.
        return image_list_from_proc_maps(task);
    }

    // Read the whole array in one round-trip.
    let array_bytes = vm_read_n(
        task,
        header.info_array as usize,
        count * mem::size_of::<dyld_image_info>(),
    )?;

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * mem::size_of::<dyld_image_info>();
        let mut entry = dyld_image_info::default();
        // SAFETY: array_bytes covers `count` whole entries.
        unsafe {
            std::ptr::copy_nonoverlapping(
                array_bytes.as_ptr().add(off),
                &mut entry as *mut _ as *mut u8,
                mem::size_of::<dyld_image_info>(),
            );
        }
        // PATH_MAX on darwin is 1024; cap reads at that.
        let path = read_cstr(task, entry.image_file_path, 1024).unwrap_or_default();
        out.push(ImageInfo {
            load_addr: entry.image_load_address as usize,
            path,
        });
    }
    Ok(out)
}

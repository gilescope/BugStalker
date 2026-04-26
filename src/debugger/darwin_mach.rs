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
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::mach_types::{task_t, vm_task_entry_t};
use mach2::port::mach_port_t;
use mach2::traps::{mach_task_self, task_for_pid as raw_task_for_pid};
use mach2::vm::{mach_vm_protect, mach_vm_read_overwrite, mach_vm_write};
use mach2::vm_prot::{VM_PROT_COPY, VM_PROT_READ, VM_PROT_WRITE};
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};
use nix::errno::Errno;
use nix::unistd::Pid;
use std::mem;

/// Coarse error envelope for Mach-side failures. We round-trip
/// through `Errno::EFAULT` at the boundary so callers using the
/// `nix::Error` type don't grow a Mach awareness; for callers
/// using `crate::debugger::Error` we also offer a direct `Ptrace`
/// mapping that carries `EFAULT`.
#[derive(Debug)]
pub struct MachError(pub kern_return_t);

impl From<MachError> for Error {
    fn from(_: MachError) -> Self {
        // The closest existing variant — the caller failed to poke
        // the inferior. The wrapped `Errno::EFAULT` is the most
        // honest mapping for "Mach denied access".
        Ptrace(Errno::EFAULT)
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
/// allowed unconditionally.
pub fn task_for_pid(pid: Pid) -> Result<task_t, MachError> {
    let mut task: mach_port_t = 0;
    // SAFETY: mach_task_self() is always valid; raw_task_for_pid
    // takes an out-port and writes to it iff KERN_SUCCESS.
    let kr = unsafe { raw_task_for_pid(mach_task_self(), pid.as_raw(), &mut task) };
    check(kr)?;
    Ok(task)
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
pub fn vm_write_word(task: task_t, addr: usize, value: usize) -> Result<(), Error> {
    let bytes = value.to_ne_bytes();
    let len = mem::size_of::<usize>() as mach_vm_size_t;

    // Widen protection to W (CoW); ignore the result — many pages
    // are already writable, and the subsequent write surfaces any
    // real failure with a meaningful kr.
    let _ = unsafe {
        mach_vm_protect(
            task as vm_task_entry_t,
            addr as mach_vm_address_t,
            len,
            0,
            VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
        )
    };

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
    check(kr)?;
    Ok(())
}

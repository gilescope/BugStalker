// SPDX-License-Identifier: MIT
//! aarch64 register layout + ptrace I/O for the recorder
//! (sub-phase 3G port).
//!
//! aarch64 uses the kernel's `struct user_pt_regs` (NT_PRSTATUS
//! regset), which differs from x86-64 in every field. The
//! syscall ABI also differs:
//!
//! | Item                  | x86-64                | aarch64           |
//! | --------------------- | --------------------- | ----------------- |
//! | Syscall number        | RAX (`orig_rax`)      | x8                |
//! | Args 0..5             | RDI, RSI, RDX, R10, R8, R9 | x0..x5       |
//! | Return value          | RAX                   | x0                |
//! | Instruction pointer   | RIP                   | pc                |
//! | Stack pointer         | RSP                   | sp                |
//!
//! Layout asserts in the test module pin the kernel ABI to
//! 272 bytes (34 × 8) — a tripwire for any libc/kernel drift.

#![cfg(target_os = "linux")]

use std::io;
#[cfg(target_arch = "aarch64")]
use std::mem;

/// Linux aarch64 `struct user_pt_regs`. From the kernel's
/// `arch/arm64/include/uapi/asm/ptrace.h`. 34 `u64` fields,
/// 272 bytes total. Layout is stable since the architecture
/// shipped.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub struct UserRegsAarch64 {
    /// x0..x30 — general-purpose register file. x8 is the
    /// syscall number; x0..x5 are syscall args 0..5.
    pub regs: [u64; 31],
    /// Stack pointer.
    pub sp: u64,
    /// Program counter.
    pub pc: u64,
    /// Processor state register (NZCV + condition flags).
    pub pstate: u64,
}

/// `NT_PRSTATUS` regset ID — same value across all archs.
#[cfg(target_arch = "aarch64")]
const NT_PRSTATUS: i32 = 1;

/// `PTRACE_GETREGSET` wrapper for aarch64 NT_PRSTATUS.
#[cfg(target_arch = "aarch64")]
pub fn get_regs_aarch64(pid: i32) -> io::Result<UserRegsAarch64> {
    let mut regs: UserRegsAarch64 = unsafe { mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: &mut regs as *mut _ as *mut libc::c_void,
        iov_len: mem::size_of::<UserRegsAarch64>(),
    };
    // SAFETY: PTRACE_GETREGSET fills the iov_base buffer with
    // up to iov_len bytes; on success iov_len is set to the
    // actually-written length (≤ initial). We provide the
    // canonical kernel struct size.
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(regs)
}

/// `PTRACE_SETREGSET` wrapper for aarch64 NT_PRSTATUS.
#[cfg(target_arch = "aarch64")]
pub fn set_regs_aarch64(pid: i32, regs: &UserRegsAarch64) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: regs as *const _ as *mut libc::c_void,
        iov_len: mem::size_of::<UserRegsAarch64>(),
    };
    let r = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Off-arch builds get stubs so callers can compile a unified
// dispatch. The stubs return ENOSYS so any caller that reaches
// them on the wrong arch surfaces a clean diagnostic.
/// Off-arch stub for `get_regs_aarch64` so dispatch sites compile on
/// non-aarch64 targets. Always returns `ENOSYS`.
#[cfg(not(target_arch = "aarch64"))]
pub fn get_regs_aarch64(_pid: i32) -> io::Result<UserRegsAarch64> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}
/// Off-arch stub for `set_regs_aarch64` so dispatch sites compile on
/// non-aarch64 targets. Always returns `ENOSYS`.
#[cfg(not(target_arch = "aarch64"))]
pub fn set_regs_aarch64(_pid: i32, _regs: &UserRegsAarch64) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

use crate::record::syscall_capture::CallFrame;

/// Build a [`CallFrame`] from aarch64 registers. Mirrors the
/// x86-64 `call_frame_from_regs` but reads the syscall number
/// from `x8` and args from `x0..x5`.
pub fn call_frame_from_regs(regs: &UserRegsAarch64) -> CallFrame {
    CallFrame {
        nr: regs.regs[8] as u32,
        args: [
            regs.regs[0],
            regs.regs[1],
            regs.regs[2],
            regs.regs[3],
            regs.regs[4],
            regs.regs[5],
        ],
    }
}

/// Sign-extending result-register read for aarch64. The kernel
/// returns the syscall result in `x0` (signed); negative is
/// `-errno`. Mirrors `result_register_x86_64`.
pub fn result_register(regs: &UserRegsAarch64) -> i64 {
    regs.regs[0] as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_regs_layout_is_34_u64_fields() {
        // sizeof(struct user_pt_regs) on Linux aarch64 = 272
        // = 34 * 8. Anyone reordering fields hits this.
        assert_eq!(std::mem::size_of::<UserRegsAarch64>(), 34 * 8);
    }

    #[test]
    fn user_regs_field_offsets_are_canonical() {
        let z: UserRegsAarch64 = unsafe { std::mem::zeroed() };
        let base = &z as *const _ as usize;
        let regs_off = &z.regs as *const _ as usize - base;
        let sp_off = &z.sp as *const _ as usize - base;
        let pc_off = &z.pc as *const _ as usize - base;
        let pstate_off = &z.pstate as *const _ as usize - base;
        assert_eq!(regs_off, 0, "regs[] at offset 0");
        assert_eq!(sp_off, 31 * 8, "sp at offset {}", 31 * 8);
        assert_eq!(pc_off, 32 * 8, "pc at offset {}", 32 * 8);
        assert_eq!(pstate_off, 33 * 8, "pstate at offset {}", 33 * 8);
    }

    #[test]
    fn call_frame_reads_x8_and_x0_through_x5() {
        let mut r = UserRegsAarch64::default();
        r.regs[8] = 64; // write
        r.regs[0] = 1;
        r.regs[1] = 0xCAFE_BA00;
        r.regs[2] = 5;
        r.regs[3] = 0;
        r.regs[4] = 0;
        r.regs[5] = 0;
        let f = call_frame_from_regs(&r);
        assert_eq!(f.nr, 64);
        assert_eq!(f.args, [1, 0xCAFE_BA00, 5, 0, 0, 0]);
    }

    #[test]
    fn result_register_sign_extends_negative_errno() {
        let mut r = UserRegsAarch64::default();
        r.regs[0] = (-2i64) as u64; // -ENOENT
        assert_eq!(result_register(&r), -2);
    }

    #[test]
    fn result_register_passes_through_positive() {
        let mut r = UserRegsAarch64::default();
        r.regs[0] = 1234;
        assert_eq!(result_register(&r), 1234);
    }
}

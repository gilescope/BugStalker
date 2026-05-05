// SPDX-License-Identifier: MIT
//! Replay path — sub-phase 3C.
//!
//! Replay re-launches the binary with the recorded environment +
//! cwd + args + fd table reconstituted. The same `seccomp-bpf`
//! filter is installed but on each notification we *do not* let
//! the syscall through; instead we read the next syscall event
//! from the trace and write the recorded output buffers + return
//! registers back into the tracee's address space.
//!
//! The program executes exactly as recorded, byte-for-byte.

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod darwin;

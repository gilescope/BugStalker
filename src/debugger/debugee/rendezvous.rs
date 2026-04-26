#![allow(dead_code)]

use crate::debugger::address::RelocatedAddress;
use nix::unistd::Pid;
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use nix::libc;
#[cfg(target_os = "linux")]
use object::elf::DT_DEBUG;

#[derive(Debug)]
pub struct LinkMap {
    pub addr: RelocatedAddress,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RendezvousError {
    #[error("section \".dynamic\" not found")]
    DynamicSectNotFound,
    #[error("read from remote process: {0}")]
    PtraceRead(#[from] nix::Error),
    #[error("rendezvous not found")]
    NotFound,
}

/// Rendezvous structure maintained by dynamic linker.
/// This structure maintains a list of shared library descriptors.
pub struct Rendezvous {
    #[allow(dead_code)]
    pid: Pid,
    #[cfg(target_os = "linux")]
    inner: ffi::r_debug,
    /// Snapshot of dyld's loaded-image list, captured at
    /// `Rendezvous::new` time. Refreshing on shared-library
    /// load/unload is a follow-up — wire
    /// `_dyld_register_func_for_add_image` later.
    #[cfg(not(target_os = "linux"))]
    images: Vec<crate::debugger::darwin_mach::ImageInfo>,
}

impl Rendezvous {
    #[cfg(target_os = "linux")]
    pub fn new(
        proc_pid: Pid,
        mapping_offset: usize,
        sections: &HashMap<String, u64>,
    ) -> Result<Self, RendezvousError> {
        let dyn_sect_addr = sections
            .get(".dynamic")
            .cloned()
            .ok_or(RendezvousError::DynamicSectNotFound)? as usize;

        let dyn_sect_addr = dyn_sect_addr + mapping_offset;
        let mut addr = dyn_sect_addr;

        let mut val = ffi::read_val::<usize>(proc_pid, &mut addr)?;

        while val != 0 {
            if val == DT_DEBUG as usize {
                let mut rend_addr = ffi::read_val::<usize>(proc_pid, &mut addr)?;
                let rendezvous = ffi::read_val::<ffi::r_debug>(proc_pid, &mut rend_addr)?;
                return Ok(Self {
                    pid: proc_pid,
                    inner: rendezvous,
                });
            }

            val = ffi::read_val::<usize>(proc_pid, &mut addr)?;
        }

        Err(RendezvousError::NotFound)
    }

    #[cfg(target_os = "linux")]
    pub fn link_map_main(&self) -> RelocatedAddress {
        RelocatedAddress::from(self.inner.link_map as usize)
    }

    #[cfg(target_os = "linux")]
    pub fn link_maps(&self) -> Result<Vec<LinkMap>, RendezvousError> {
        let mut result = vec![];
        let mut next_link_map_addr = usize::from(self.link_map_main()) as *const libc::c_void;

        while !next_link_map_addr.is_null() {
            let lm = ffi::read_val::<ffi::link_map>(self.pid, &mut (next_link_map_addr as usize))?;
            let name = ffi::read_string(self.pid, lm.l_name as usize)?;

            result.push(LinkMap {
                addr: RelocatedAddress::from(next_link_map_addr as usize),
                name,
            });

            next_link_map_addr = lm.l_next;
        }

        Ok(result)
    }

    /// Return an address of a function internal to the run-time linker,
    /// that will always be called when the linker begins to map in a
    /// library or unmap it, and again when the mapping change is complete.
    #[cfg(target_os = "linux")]
    pub fn r_brk(&self) -> RelocatedAddress {
        RelocatedAddress::from(self.inner.r_brk)
    }

    /// Darwin path: build a `Rendezvous` from
    /// `task_info(TASK_DYLD_INFO)` — that gives us a debuggee VA
    /// pointing at dyld's `dyld_all_image_infos`, which we walk to
    /// snapshot the loaded-image list. The `mapping_offset` and
    /// `sections` arguments come from the GNU ELF rendezvous flow
    /// and have no darwin equivalent — kept in the signature so
    /// the cross-platform call site (in `Debugee::new_*`) doesn't
    /// have to cfg-branch.
    #[cfg(not(target_os = "linux"))]
    pub fn new(
        proc_pid: Pid,
        _mapping_offset: usize,
        _sections: &HashMap<String, u64>,
    ) -> Result<Self, RendezvousError> {
        use crate::debugger::darwin_mach;
        let task = darwin_mach::task_for_pid(proc_pid)
            .map_err(|_| RendezvousError::NotFound)?;
        let images = darwin_mach::dyld_image_list(task)
            .map_err(|_| RendezvousError::NotFound)?;
        if images.is_empty() {
            return Err(RendezvousError::NotFound);
        }
        Ok(Self {
            pid: proc_pid,
            images,
        })
    }

    /// Darwin: the first dyld image is the main executable's
    /// `mach_header`; that's the cross-platform equivalent of
    /// linux's `link_map` head pointer.
    #[cfg(not(target_os = "linux"))]
    pub fn link_map_main(&self) -> RelocatedAddress {
        let load = self
            .images
            .first()
            .map(|i| i.load_addr)
            .unwrap_or(0);
        RelocatedAddress::from(load)
    }

    /// Darwin: one `LinkMap` per loaded dyld image. We use each
    /// image's `mach_header` load address as the `addr` field
    /// (analogous to linux's `link_map` node address) and the
    /// path string as the name.
    #[cfg(not(target_os = "linux"))]
    pub fn link_maps(&self) -> Result<Vec<LinkMap>, RendezvousError> {
        Ok(self
            .images
            .iter()
            .map(|i| LinkMap {
                addr: RelocatedAddress::from(i.load_addr),
                name: i.path.clone(),
            })
            .collect())
    }

    /// Darwin: dyld doesn't publish an exact `r_brk` equivalent —
    /// the conventional way to learn about image load/unload is
    /// `_dyld_register_func_for_add_image` (in-process callbacks).
    /// For the POC we hand back the address of the first image's
    /// `mach_header` so the caller's "set a BP here" code installs
    /// a no-op (the BP will sit at module-start memory which is
    /// already executable, but won't fire on dyld changes). Module
    /// load/unload tracking lands as a later follow-up.
    #[cfg(not(target_os = "linux"))]
    pub fn r_brk(&self) -> RelocatedAddress {
        self.link_map_main()
    }
}

// The rendezvous protocol read here is GNU ld.so's `r_debug` /
// `link_map` linked list, accessed via `process_vm_readv` (linux-only).
// Darwin has a completely different image-list discovery path:
// `task_info(TASK_DYLD_INFO)` returns a `dyld_image_info_array` —
// that's what the macOS port will plumb in when we get there.
#[cfg(target_os = "linux")]
mod ffi {
    #![allow(non_camel_case_types)]

    use nix::libc;
    use nix::sys::uio;
    use nix::sys::uio::RemoteIoVec;
    use nix::unistd::Pid;
    use std::io::IoSliceMut;
    use std::mem;

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub(super) struct r_debug {
        /// Version number for this protocol.
        pub(super) r_version: i32,
        /// Head of the chain of loaded objects.
        pub(super) link_map: *const libc::c_void,
        /// This is the address of a function internal to the run-time linker,
        /// that will always be called when the linker begins to map in a
        /// library or unmap it, and again when the mapping change is complete.
        /// The debugger can set a breakpoint at this address if it wants to
        /// notice shared object mapping changes.
        pub(super) r_brk: usize,
    }

    #[derive(Debug, Clone, Copy)]
    #[repr(C)]
    pub(super) struct link_map {
        /// Difference between the address in the ELF file and the address in memory
        pub(super) l_addr: *mut libc::c_void,
        /// Absolute pathname where object was found
        pub(super) l_name: *const libc::c_char,
        /// Dynamic section of the shared object
        pub(super) l_ld: *mut libc::c_void,

        pub(super) l_next: *mut libc::c_void,
        pub(super) l_prev: *mut libc::c_void,
    }

    pub(super) fn read_val<T: Copy>(pid: Pid, addr: &mut usize) -> nix::Result<T> {
        let size = mem::size_of::<T>();
        let mut buff = vec![0; size];
        let local_iov = IoSliceMut::new(buff.as_mut_slice());
        let remote_iov = RemoteIoVec {
            base: *addr,
            len: size,
        };
        let local_iov_slice = &mut [local_iov];

        let _reads = uio::process_vm_readv(pid, local_iov_slice.as_mut_slice(), &[remote_iov])?;

        let ptr = local_iov_slice[0].as_ptr();

        let val_ptr: *const T = ptr.cast::<T>();
        let val = unsafe { *val_ptr };

        *addr += size;

        Ok(val)
    }

    pub(super) fn read_string(pid: Pid, mut addr: usize) -> nix::Result<String> {
        let mut buff = vec![];
        let mut word = read_val::<usize>(pid, &mut addr)?;

        loop {
            for b in word.to_ne_bytes() {
                if b as char == '\0' {
                    return Ok(String::from_utf8_lossy(&buff).to_string());
                }
                buff.push(b);
            }
            word = read_val::<usize>(pid, &mut addr)?;
        }
    }
}

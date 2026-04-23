//! Architecture shim for the `thread_db` crate.
//!
//! `thread_db` binds glibc's `libthread_db`, which is only implemented for
//! x86_64 (and i686) at the time of writing. On other architectures this
//! module exposes stubs that preserve the public shape of the API but
//! always report "not supported", allowing the rest of the debugger to
//! compile and run with TLS-related features gracefully degraded.
//!
//! The expected failure path on non-x86_64 is:
//!   * `Lib::try_load()` succeeds (a zero-sized handle).
//!   * `Lib::attach(pid)` returns `Err(ThreadDbError)`.
//!   * `Debugee::attach_libthread_db` already logs a warning and
//!     continues when attach fails, so no thread_db-dependent feature is
//!     silently exercised.

#[cfg(target_arch = "x86_64")]
pub use ::thread_db::{Lib, Process, Thread, ThreadDbError};

#[cfg(not(target_arch = "x86_64"))]
pub use stub::{Lib, Process, Thread, ThreadDbError};

#[cfg(not(target_arch = "x86_64"))]
mod stub {
    use nix::unistd::Pid;
    use std::fmt;
    use std::marker::PhantomData;

    #[derive(Debug)]
    pub struct ThreadDbError;

    impl fmt::Display for ThreadDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "libthread_db is not available on this architecture (upstream `thread_db` crate supports x86_64/i686 only)"
            )
        }
    }

    impl std::error::Error for ThreadDbError {}

    pub struct Lib;

    impl Lib {
        pub fn try_load() -> Result<Self, ThreadDbError> {
            Ok(Lib)
        }

        pub fn attach(&self, _pid: Pid) -> Result<Process<'_>, ThreadDbError> {
            Err(ThreadDbError)
        }
    }

    pub struct Process<'a> {
        _lib: PhantomData<&'a Lib>,
    }

    impl<'a> Process<'a> {
        pub fn get_thread(&self, _pid: Pid) -> Result<Thread, ThreadDbError> {
            Err(ThreadDbError)
        }
    }

    pub struct Thread;

    impl Thread {
        pub fn tls_addr(&self, _link_map: u64, _offset: usize) -> Result<u64, ThreadDbError> {
            Err(ThreadDbError)
        }
    }
}

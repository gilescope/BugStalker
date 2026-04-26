use crate::debugger::error::Error;
use nix::unistd::Pid;
use os_pipe::PipeWriter;
use std::marker::PhantomData;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use sysinfo::{RefreshKind, System};

// Linux uses ptrace + the GNU `personality` syscall to disable ASLR
// in the debuggee. Darwin uses `posix_spawnattr_set_disable_aslr_np`
// + Mach exception ports; the spawn/attach paths below are gated on
// linux only until the darwin path lands.
#[cfg(target_os = "linux")]
use crate::debugger::error::Error::{Ptrace, Waitpid};
#[cfg(target_os = "linux")]
use nix::sys;
#[cfg(target_os = "linux")]
use nix::sys::personality::Persona;
#[cfg(target_os = "linux")]
use nix::sys::ptrace::Options;
#[cfg(target_os = "linux")]
use nix::sys::signal::{SIGSTOP, SIGTRAP};
#[cfg(target_os = "linux")]
use nix::sys::wait::WaitStatus::PtraceEvent;
#[cfg(target_os = "linux")]
use nix::sys::wait::{WaitPidFlag, waitpid};
#[cfg(target_os = "linux")]
use nix::unistd::{ForkResult, fork};
#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::iter;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::process::Command;

/// Process state.
pub trait State {}

/// Process running and attached with `ptrace` system call.
pub struct Installed;

impl State for Installed {}

/// Process prepare for instantiation by a `fork` call.
pub struct Template;

impl State for Template {}

/// External process information.
pub struct ExternalInfo {
    /// List of threads observed at the time of attaching
    pub threads: Vec<Pid>,
}

/// Process attached to tracer with ptrace.
pub struct Child<S: State> {
    program: String,
    stdout: PipeWriter,
    stderr: PipeWriter,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    pid: Option<Pid>,
    external_info: Option<ExternalInfo>,
    _p: PhantomData<S>,
}

impl Child<Template> {
    /// Create new process, but dont start it.
    ///
    /// # Arguments
    ///
    /// * `program`: program name
    /// * `stdout`: stdout pipe
    /// * `stderr`: stderr pipe
    /// * `args`: program arguments
    pub fn new<ARGS: IntoIterator<Item = I>, I: Into<String>>(
        program: impl Into<String>,
        args: ARGS,
        cwd: Option<impl Into<PathBuf>>,
        stdout: PipeWriter,
        stderr: PipeWriter,
    ) -> Child<Template> {
        Self {
            stdout,
            stderr,
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.map(Into::into),
            pid: None,
            external_info: None,
            _p: PhantomData,
        }
    }
}

impl Child<Installed> {
    /// Return running process pid.
    pub fn pid(&self) -> Pid {
        self.pid.unwrap()
    }

    ///  Create [`Child`] from already running external process.
    ///
    /// # Arguments
    ///
    /// * `pid`: an external process pid
    /// * `stdout`: stdout pipe, this pipe will not be used for the current process but it will be used after a possible restart
    /// * `stderr`: stderr pipe, this pipe will not be used for the current process but it will be used after a possible restart
    #[cfg(target_os = "linux")]
    pub fn from_external(pid: Pid, stdout: PipeWriter, stderr: PipeWriter) -> Result<Self, Error> {
        let sys =
            System::new_with_specifics(RefreshKind::everything().without_cpu().without_memory());

        let external_process = System::process(&sys, sysinfo::Pid::from_u32(pid.as_raw() as u32))
            .ok_or(Error::AttachedProcessNotFound(pid))?;

        let program_name = external_process
            .exe()
            .ok_or(Error::AttachedProcessNotFound(pid))?
            .to_string_lossy()
            .to_string();

        let cwd = external_process.cwd().map(ToOwned::to_owned);

        let mut interrupted_threads = HashSet::new();
        // two interrupt rounds, like in [`Tracer`]
        for _ in 0..2 {
            let treads_iter = iter::once(pid);
            let threads: Vec<Pid> = if let Some(tasks) = external_process.tasks() {
                treads_iter
                    .chain(tasks.iter().map(|tid| Pid::from_raw(tid.as_u32() as i32)))
                    .collect()
            } else {
                treads_iter.collect()
            };

            // remove already interrupted threads
            let threads: Vec<Pid> = threads
                .into_iter()
                .filter(|t| !interrupted_threads.contains(t))
                .collect();

            for tid in &threads {
                sys::ptrace::seize(
                    *tid,
                    Options::PTRACE_O_TRACECLONE
                        .union(Options::PTRACE_O_TRACEEXEC)
                        .union(Options::PTRACE_O_TRACEEXIT),
                )
                .map_err(Error::Attach)?;
            }

            for tid in &threads {
                sys::ptrace::interrupt(*tid).map_err(Error::Attach)?;
            }

            for tid in &threads {
                let status = waitpid(*tid, None).map_err(Error::Attach)?;
                // currently we assume that attached process not in stop status
                debug_assert!(matches!(status, PtraceEvent(_, SIGTRAP, _)));
            }

            interrupted_threads.extend(threads);
        }

        Ok(Self {
            stdout,
            stderr,
            program: program_name,
            args: external_process.cmd()[1..].to_vec(),
            cwd,
            pid: Some(pid),
            external_info: Some(ExternalInfo {
                threads: interrupted_threads.into_iter().collect(),
            }),
            _p: PhantomData,
        })
    }

    /// Darwin: attach to an already-running process by pid via
    /// pure Mach. `task_for_pid` resolves the task port (requires
    /// the `com.apple.security.cs.debugger` entitlement on the
    /// caller for cross-process attach), then `task_suspend`
    /// parks the inferior so the engine can install BPs / read
    /// state before the next `Tracer::resume` releases it.
    ///
    /// **Multi-thread limitation:** the linux side records every
    /// tid (from `/proc/<pid>/task/`) so each thread ends up as a
    /// separate `Tracee`. On darwin, threads are Mach `thread_act_t`
    /// ports — u32 IPC names, not pids — and the cross-platform
    /// `Tracee` API is keyed by `Pid`. Until the multi-thread
    /// enumeration code grows a stable Mach-port → Pid mapping,
    /// we record only the main pid; per-thread inspection still
    /// works through `task_threads_vec` from the inside.
    #[cfg(not(target_os = "linux"))]
    pub fn from_external(pid: Pid, stdout: PipeWriter, stderr: PipeWriter) -> Result<Self, Error> {
        use crate::debugger::darwin_mach;
        use sysinfo::{RefreshKind, System};

        let sys = System::new_with_specifics(
            RefreshKind::everything().without_cpu().without_memory(),
        );
        let external = System::process(&sys, sysinfo::Pid::from_u32(pid.as_raw() as u32))
            .ok_or(Error::AttachedProcessNotFound(pid))?;
        let program = external
            .exe()
            .ok_or(Error::AttachedProcessNotFound(pid))?
            .to_string_lossy()
            .to_string();
        let cwd = external.cwd().map(ToOwned::to_owned);
        let args: Vec<String> = external.cmd().get(1..).unwrap_or(&[]).to_vec();

        // Pure-Mach attach: task_for_pid + task_suspend. No ptrace.
        // The caller must have com.apple.security.cs.debugger for
        // cross-process task_for_pid; that's the same requirement
        // the entitlement-gated tests document.
        let task = darwin_mach::task_for_pid(pid)?;
        darwin_mach::task_suspend(task)?;

        Ok(Self {
            stdout,
            stderr,
            program,
            args,
            cwd,
            pid: Some(pid),
            external_info: Some(ExternalInfo { threads: vec![pid] }),
            _p: PhantomData,
        })
    }
}

impl<S: State> Child<S> {
    /// Return a program name.
    pub fn program(&self) -> &str {
        self.program.as_str()
    }

    /// True when process was attached by its pid, false elsewhere.
    pub fn is_external(&self) -> bool {
        self.external_info.is_some()
    }

    /// Return [`ExternalInfo`] if underline process is external (attached by pid).
    pub fn external_info(&self) -> Option<&ExternalInfo> {
        self.external_info.as_ref()
    }

    /// Instantiate process by `fork()` system call with caller as a parent process.
    /// After installation child process stopped by `SIGSTOP` signal.
    #[cfg(target_os = "linux")]
    pub fn install(&self) -> Result<Child<Installed>, Error> {
        let mut debugee_cmd = Command::new(&self.program);
        let debugee_cmd = debugee_cmd
            .args(&self.args)
            .stdout(self.stdout.try_clone()?)
            .stderr(self.stderr.try_clone()?);

        if let Some(cwd) = self.cwd.as_deref() {
            debugee_cmd.current_dir(cwd);
        }

        unsafe {
            debugee_cmd.pre_exec(move || {
                // Best-effort: some environments (e.g. Docker containers with
                // restricted seccomp profiles) don't allow the personality
                // syscall.  ASLR being enabled makes addresses non-deterministic
                // across runs but doesn't break debugging.
                let _ = sys::personality::set(Persona::ADDR_NO_RANDOMIZE);
                Ok(())
            });
        }

        match unsafe { fork().expect("fork() error") } {
            ForkResult::Parent { child: pid } => {
                waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WSTOPPED)).map_err(Waitpid)?;
                sys::ptrace::seize(
                    pid,
                    Options::PTRACE_O_TRACECLONE
                        .union(Options::PTRACE_O_TRACEEXEC)
                        .union(Options::PTRACE_O_TRACEEXIT),
                )
                .map_err(Ptrace)?;

                Ok(Child {
                    stdout: self.stdout.try_clone()?,
                    stderr: self.stderr.try_clone()?,
                    program: self.program.clone(),
                    args: self.args.clone(),
                    cwd: self.cwd.clone(),
                    pid: Some(pid),
                    external_info: None,
                    _p: PhantomData,
                })
            }
            ForkResult::Child => {
                sys::signal::raise(SIGSTOP).unwrap();
                let err = debugee_cmd.exec();
                panic!("run debugee fail with: {err}");
            }
        }
    }

    /// Darwin path: `posix_spawnp` with
    /// `POSIX_SPAWN_START_SUSPENDED`. The child is created in a
    /// SIGSTOP-equivalent state — the kernel suspends it before
    /// any user-space instruction runs — so the parent has time
    /// to call `task_for_pid`, register a Mach exception port,
    /// install breakpoints, and only then `task_resume` it.
    ///
    /// **No ptrace.** The whole debugger backend on darwin runs
    /// through Mach (memory I/O via `mach_vm_*`, registers via
    /// `thread_get/set_state`, BP and watchpoint events via
    /// `task_set_exception_ports`). ptrace doesn't expose
    /// memory access on aarch64 anyway, and mixing ptrace's
    /// signal-translation chain with our Mach exception port
    /// fights for the same routing — see `doc/ROADMAP.md`.
    ///
    /// The parent gets `task_for_pid` rights for free here
    /// because we're the parent of the spawned child (the kernel
    /// allows it across the parent/child relationship without
    /// needing the `com.apple.security.cs.debugger` entitlement —
    /// the entitlement is for *unrelated* pids).
    #[cfg(target_os = "macos")]
    pub fn install(&self) -> Result<Child<Installed>, Error> {
        use nix::unistd::Pid as NixPid;
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::ptr;

        let path = CString::new(self.program.as_str())
            .map_err(|_| Error::Attach(nix::errno::Errno::EINVAL))?;
        let mut argv: Vec<CString> = std::iter::once(path.clone())
            .chain(
                self.args
                    .iter()
                    .filter_map(|a| CString::new(a.as_str()).ok()),
            )
            .collect();
        // posix_spawnp wants a NULL-terminated `*const *const c_char`.
        let mut argv_ptrs: Vec<*mut libc::c_char> =
            argv.iter_mut().map(|s| s.as_ptr() as *mut _).collect();
        argv_ptrs.push(ptr::null_mut());
        // Inherit the parent's environment — Command does this by
        // default and we want the same shape.
        let envp_ptrs: Vec<*mut libc::c_char> = vec![ptr::null_mut()];

        // Build the spawn attributes: POSIX_SPAWN_START_SUSPENDED
        // is the magic flag — the kernel creates the child as if
        // it had received SIGSTOP, leaving it parked until we
        // task_resume (or send SIGCONT) it.
        let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
        // SAFETY: out-pointer; libc writes if KERN_SUCCESS.
        let r = unsafe { libc::posix_spawnattr_init(&mut attr) };
        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }
        struct AttrGuard(libc::posix_spawnattr_t);
        impl Drop for AttrGuard {
            fn drop(&mut self) {
                // SAFETY: paired init/destroy; idempotent on null.
                unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
            }
        }
        let _attr_guard = AttrGuard(attr);
        // SAFETY: attr is freshly initialised.
        let r = unsafe {
            libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_START_SUSPENDED as i16)
        };
        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }

        // File actions: dup the caller-provided pipes onto the
        // child's stdout/stderr. stdin is left as-is (inherited).
        let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
        let r = unsafe { libc::posix_spawn_file_actions_init(&mut actions) };
        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }
        struct ActionsGuard(libc::posix_spawn_file_actions_t);
        impl Drop for ActionsGuard {
            fn drop(&mut self) {
                // SAFETY: paired init/destroy.
                unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
            }
        }
        let _actions_guard = ActionsGuard(actions);
        // SAFETY: stdout_w / stderr_w are owned PipeWriters; their
        // raw fds remain valid for the duration of this function.
        let stdout_fd = self.stdout.as_raw_fd();
        let stderr_fd = self.stderr.as_raw_fd();
        let r = unsafe {
            libc::posix_spawn_file_actions_adddup2(&mut actions, stdout_fd, libc::STDOUT_FILENO)
        };
        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }
        let r = unsafe {
            libc::posix_spawn_file_actions_adddup2(&mut actions, stderr_fd, libc::STDERR_FILENO)
        };
        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }

        // posix_spawn doesn't have a "set cwd" option that we can
        // rely on portably; chdir before the spawn and restore
        // after if the caller asked for one. The child inherits the
        // parent's cwd at spawn time.
        let saved_cwd = self
            .cwd
            .as_deref()
            .map(|cwd| -> Result<std::path::PathBuf, Error> {
                let prev = std::env::current_dir()
                    .map_err(|_| Error::Attach(nix::errno::Errno::EIO))?;
                std::env::set_current_dir(cwd)
                    .map_err(|_| Error::Attach(nix::errno::Errno::EIO))?;
                Ok(prev)
            })
            .transpose()?;

        let mut child_pid: libc::pid_t = 0;
        // SAFETY: argv/envp arrays are NUL-terminated (we pushed
        // the trailing null above). path lives across the call.
        let r = unsafe {
            libc::posix_spawnp(
                &mut child_pid,
                path.as_ptr(),
                &actions,
                &attr,
                argv_ptrs.as_ptr() as *const *mut _,
                envp_ptrs.as_ptr() as *const *mut _,
            )
        };

        if let Some(prev) = saved_cwd {
            let _ = std::env::set_current_dir(prev);
        }

        if r != 0 {
            return Err(Error::Attach(nix::errno::Errno::from_i32(r)));
        }

        Ok(Child {
            stdout: self.stdout.try_clone()?,
            stderr: self.stderr.try_clone()?,
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            pid: Some(NixPid::from_raw(child_pid)),
            external_info: None,
            _p: PhantomData,
        })
    }
}

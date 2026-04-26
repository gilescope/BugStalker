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

    /// Darwin path: attach to an existing pid via `task_for_pid` +
    /// Mach exception ports. Stubbed for the macOS port.
    #[cfg(not(target_os = "linux"))]
    pub fn from_external(_pid: Pid, _stdout: PipeWriter, _stderr: PipeWriter) -> Result<Self, Error> {
        unimplemented!("darwin: Child::from_external via task_for_pid + Mach exception ports")
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

    /// Darwin path: fork + `PT_TRACE_ME` + exec, mirroring the linux
    /// shape. We use the BSD-flavour ptrace just to:
    ///   * grant the parent the right to call `task_for_pid` on the
    ///     child (the kernel only allows that across an unrelated
    ///     pid pair when the caller has the
    ///     `com.apple.security.cs.debugger` entitlement; for a
    ///     ptraced child it's allowed unconditionally), and
    ///   * make the child stop with `SIGTRAP` immediately after
    ///     `execve`, so we can attach the Mach exception ports
    ///     before any debuggee instruction runs.
    ///
    /// The actual debugging primitives (memory R/W, registers,
    /// breakpoints, single-step) will then go through Mach
    /// (`mach_vm_*`, `thread_get_state`, exception ports) — *not*
    /// through ptrace, which on macOS has a deliberately limited
    /// surface that doesn't cover memory access on aarch64.
    #[cfg(target_os = "macos")]
    pub fn install(&self) -> Result<Child<Installed>, Error> {
        use nix::sys::ptrace;
        use nix::sys::signal::SIGSTOP;
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
        use nix::unistd::{ForkResult, Pid as NixPid, fork};
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let mut debugee_cmd = Command::new(&self.program);
        debugee_cmd
            .args(&self.args)
            .stdout(self.stdout.try_clone()?)
            .stderr(self.stderr.try_clone()?);
        if let Some(cwd) = self.cwd.as_deref() {
            debugee_cmd.current_dir(cwd);
        }

        unsafe {
            debugee_cmd.pre_exec(move || {
                // PT_TRACE_ME — child says "I'm a tracee". After
                // execve the kernel will deliver SIGTRAP to us, which
                // the parent's waitpid below catches.
                ptrace::traceme().map_err(std::io::Error::from)?;
                // Mirror the linux ADDR_NO_RANDOMIZE step by raising
                // SIGSTOP first; the parent waitpids that and then
                // we'll bring the debuggee back up only after Mach
                // exception ports are wired (next iteration).
                Ok(())
            });
        }

        match unsafe { fork() }.map_err(Error::Attach)? {
            ForkResult::Parent { child: pid } => {
                // Two stops to expect from a PT_TRACE_ME'd child:
                //   * SIGTRAP at the post-exec attach point, OR
                //   * SIGSTOP if the child raise()s before exec.
                // Either way we just need the child to be paused so
                // the next phase can attach its Mach exception port.
                let status = waitpid(pid, Some(WaitPidFlag::WUNTRACED)).map_err(Error::Attach)?;
                debug_assert!(matches!(
                    status,
                    WaitStatus::Stopped(_, signal)
                        if signal == nix::sys::signal::SIGTRAP
                            || signal == SIGSTOP
                ));

                Ok(Child {
                    stdout: self.stdout.try_clone()?,
                    stderr: self.stderr.try_clone()?,
                    program: self.program.clone(),
                    args: self.args.clone(),
                    cwd: self.cwd.clone(),
                    pid: Some(NixPid::from_raw(pid.as_raw())),
                    external_info: None,
                    _p: PhantomData,
                })
            }
            ForkResult::Child => {
                let err = debugee_cmd.exec();
                panic!("run debugee fail with: {err}");
            }
        }
    }
}

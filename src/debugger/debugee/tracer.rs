use crate::debugger::address::RelocatedAddress;
use crate::debugger::breakpoint::Breakpoint;
use crate::debugger::debugee::tracee::TraceeCtl;
use crate::debugger::error::Error;
use crate::debugger::register::debug::DebugRegisterNumber;
use crate::debugger::watchpoint::WatchpointRegistry;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
#[cfg(target_os = "linux")]
use std::collections::VecDeque;

// The whole `impl Tracer` below is built around Linux ptrace
// (`PTRACE_SEIZE`, `PTRACE_INTERRUPT`, `PTRACE_GETSIGINFO`,
// `WaitStatus::PtraceEvent`, …). Darwin uses Mach exception ports
// for the equivalent flow; until that backend lands, the cross-arch
// `impl Tracer` at the bottom of the file routes everything through
// `unimplemented!()`. The imports below are linux-only because they
// only resolve when `target_os = "linux"`.
#[cfg(target_os = "linux")]
use crate::debugger::breakpoint::BrkptType;
#[cfg(target_os = "linux")]
use crate::debugger::debugee::tracee::{StopType, TraceeStatus};
#[cfg(target_os = "linux")]
use crate::debugger::error::Error::{MultipleErrors, ProcessExit, Ptrace, Waitpid};
#[cfg(target_os = "linux")]
use crate::debugger::{code, register};
#[cfg(target_os = "linux")]
use crate::weak_error;
#[cfg(target_os = "linux")]
use log::{debug, warn};
#[cfg(target_os = "linux")]
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::libc::pid_t;
#[cfg(target_os = "linux")]
use nix::sys::signal::SIGSTOP;
#[cfg(target_os = "linux")]
use nix::sys::wait::{WaitStatus, waitpid};
#[cfg(target_os = "linux")]
use nix::{libc, sys};

/// List of signals that dont interrupt a debugging process and send
/// to debugee directly on fire.
#[cfg(target_os = "linux")]
static QUIET_SIGNALS: &[Signal] = &[
    Signal::SIGALRM,
    Signal::SIGURG,
    Signal::SIGCHLD,
    Signal::SIGIO,
    Signal::SIGVTALRM,
    Signal::SIGPROF,
    //Signal::SIGWINCH,
];

/// List of signals that may interrupt a debugging process but debugger will not inject it into.
#[cfg(target_os = "linux")]
static TRANSPARENT_SIGNALS: &[Signal] = &[Signal::SIGINT];

#[derive(Debug, Clone)]
pub enum WatchpointHitType {
    /// Hit of the underlying hardware breakpoint cause value changed.
    DebugRegister(DebugRegisterNumber),
    /// Hit of the underlying breakpoint at the end of the watchpoint scope.
    EndOfScope(Vec<u32>),
}

#[derive(Debug)]
pub enum StopReason {
    /// Whole debugee process exited with code.
    DebugeeExit(i32),
    /// Debugee just started.
    DebugeeStart,
    /// Debugee stopped at breakpoint.
    Breakpoint(Pid, RelocatedAddress),
    /// Debugee stopped at watchpoint.
    Watchpoint(Pid, RelocatedAddress, WatchpointHitType),
    /// Debugee stopped with OS signal.
    SignalStop(Pid, Signal),
    /// Debugee stopped with Errno::ESRCH.
    NoSuchProcess(Pid),
}

/// Trace context (or tcx).
#[derive(Clone, Copy)]
pub struct TraceContext<'a> {
    pub breakpoints: &'a [&'a Breakpoint],
    pub watchpoints: &'a WatchpointRegistry,
}

impl<'a> TraceContext<'a> {
    pub fn new(
        breakpoints: &'a [&'a Breakpoint],
        watchpoint_registry: &'a WatchpointRegistry,
    ) -> Self {
        Self {
            breakpoints,
            watchpoints: watchpoint_registry,
        }
    }
}

/// Ptrace tracer.
pub struct Tracer {
    pub(super) tracee_ctl: TraceeCtl,

    /// Linux-only: signals queued for re-injection on the next
    /// resume (`PTRACE_CONT(sig)`). Darwin's Mach exception port
    /// flow doesn't have an equivalent — replies to exceptions
    /// are the resume primitive.
    #[cfg(target_os = "linux")]
    inject_signal_queue: VecDeque<(Pid, Signal)>,
    /// Linux-only: guards the group-stop race in `PTRACE_O_TRACECLONE`.
    #[cfg(target_os = "linux")]
    group_stop_guard: bool,
    /// Darwin: lazily-initialised Mach supervision state. Set up
    /// on the first call to `resume`/`single_step`: allocates the
    /// exception port and registers it on the task. Subsequent
    /// calls reuse the cached `(task, port, pending_reply)` tuple.
    #[cfg(not(target_os = "linux"))]
    darwin_state: Option<DarwinSupervision>,
    /// Darwin: did we already report the synthetic `DebugeeStart`
    /// for the initial spawn-suspend stop? POSIX_SPAWN_START_SUSPENDED
    /// leaves the inferior parked from creation; we surface that
    /// as DebugeeStart on the first resume so the engine can run
    /// its init path.
    #[cfg(not(target_os = "linux"))]
    darwin_seen_initial_stop: bool,
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct DarwinSupervision {
    task: mach2::mach_types::task_t,
    /// Process pid the inferior spawned with — also the pid we use
    /// for the main `Tracee`. Worker-thread tracees get synthetic
    /// pids allocated via `next_synthetic_pid`.
    proc_pid: nix::unistd::Pid,
    port: crate::debugger::darwin_mach::ExceptionPort,
    /// Kernel thread_id (`pthread_threadid_np`-flavour) → synthetic
    /// per-thread `Pid`. Populated by `reconcile_threads`; lookup
    /// path for translating an exception's `thread_port` into the
    /// `Pid` the rest of the engine expects.
    thread_id_to_pid: std::collections::HashMap<u64, nix::unistd::Pid>,
    /// Reverse of `thread_id_to_pid` so we can clean up registry
    /// entries when a thread exits.
    pid_to_thread_id: std::collections::HashMap<nix::unistd::Pid, u64>,
    /// Next synthetic pid handed out for a worker thread. Starts at
    /// `proc_pid + 1_000_000` so it can't collide with any real pid
    /// the kernel might recycle for a future child of the parent
    /// process.
    next_synthetic_pid: i32,
    /// `(remote_port, msg_id)` of the most recent
    /// `mach_exception_raise` we received but haven't replied to.
    /// The kernel parks the faulting thread until reply; we hold
    /// off until the next `resume`/`single_step` so the user can
    /// inspect coherent state.
    ///
    /// `Cell` so this can be mutated via `&self` — the Mach-native
    /// CallHelper drives the trampoline through `&CallContext`
    /// (which holds `&Debugger`); going through `&mut Tracer`
    /// would cascade `&mut self` through the entire CallHelper +
    /// Print Handler stack and conflict with `QueryResult<'a>`'s
    /// shared borrow on `Debugger`. Linux gets the equivalent
    /// "FFI-opaque mutability" for free since `ptrace::cont`/`step`
    /// aren't visible to the borrow checker.
    /// `(remote_port, msg_id, retcode)` — `retcode` is the
    /// `kern_return_t` we'll send when we finally reply. Default is
    /// `KERN_SUCCESS` ("debugger handled this exception, kernel
    /// resumes the thread normally"). For an `EXC_SOFT_SIGNAL`
    /// we want to forward through to the BSD signal layer, the
    /// caller stores `KERN_FAILURE` here so the kernel proceeds with
    /// the original signal delivery (the user's signal handler
    /// runs).
    pending_reply: std::cell::Cell<Option<(u32, i32, mach2::kern_return::kern_return_t)>>,
    /// Mach port subscribed to dyld's image-load/unload notifications
    /// for this task. Replaces the legacy "SW BP at
    /// `_lldb_image_notifier`" rendezvous, which doesn't fire
    /// reliably on darwin/aarch64 (shared-cache CoW + cross-core
    /// I-cache). dyld writes one message here per
    /// `triggerNotifications()` call; we poll between exception
    /// receives and synthesise a `LinkerMapFn` breakpoint event so
    /// the existing higher-level handler refreshes deferred BPs.
    /// `None` until first registration succeeds — registration can
    /// fail transiently if dyld hasn't installed its notifyPorts
    /// table yet.
    dyld_notify: Option<crate::debugger::darwin_mach::DyldNotifyPort>,
}

#[cfg(not(target_os = "linux"))]
impl DarwinSupervision {
    pub(crate) fn task(&self) -> mach2::mach_types::task_t {
        self.task
    }
    pub(crate) fn port(&self) -> &crate::debugger::darwin_mach::ExceptionPort {
        &self.port
    }
    pub(crate) fn take_pending_reply(
        &self,
    ) -> Option<(u32, i32, mach2::kern_return::kern_return_t)> {
        self.pending_reply.take()
    }
    pub(crate) fn set_pending_reply(
        &self,
        v: Option<(u32, i32, mach2::kern_return::kern_return_t)>,
    ) {
        self.pending_reply.set(v);
    }
}

#[cfg(not(target_os = "linux"))]
impl Tracer {
    pub(crate) fn darwin_state(&self) -> Option<&DarwinSupervision> {
        self.darwin_state.as_ref()
    }
}

#[cfg(target_os = "linux")]
impl Tracer {
    /// Create new [`Tracer`] for internally created debugee process.
    ///
    /// # Arguments
    ///
    /// * `proc_pid`: process id
    pub fn new(proc_pid: Pid) -> Self {
        Self {
            tracee_ctl: TraceeCtl::new(proc_pid),
            inject_signal_queue: VecDeque::new(),
            group_stop_guard: false,
        }
    }

    /// Create [`Tracer`] for external process attached by pid.
    ///
    /// # Arguments
    ///
    /// * `proc_pid`: process id
    /// * `threads`: id's of process threads
    pub fn new_external(proc_pid: Pid, threads: &[Pid]) -> Self {
        Self {
            tracee_ctl: TraceeCtl::new_external(proc_pid, threads),
            inject_signal_queue: VecDeque::new(),
            group_stop_guard: false,
        }
    }

    /// Continue debugee execution until stop happened.
    pub fn resume(&mut self, tcx: TraceContext) -> Result<StopReason, Error> {
        loop {
            if let Some(req) = self.inject_signal_queue.pop_front() {
                self.tracee_ctl.cont_stopped_ex(
                    Some(req),
                    self.inject_signal_queue
                        .iter()
                        .map(|(pid, _)| *pid)
                        .collect(),
                )?;

                if let Some((pid, sign)) = self.inject_signal_queue.front().copied() {
                    // if there are more signals - stop debugee again
                    self.group_stop_interrupt(tcx, Pid::from_raw(-1))?;
                    return Ok(StopReason::SignalStop(pid, sign));
                }
            } else {
                self.tracee_ctl.cont_stopped().map_err(MultipleErrors)?;
            }

            debug!(target: "tracer", "resume debugee execution, wait for updates");
            let status = match waitpid(Pid::from_raw(-1), None) {
                Ok(status) => status,
                Err(Errno::ECHILD) => {
                    return Ok(StopReason::NoSuchProcess(self.tracee_ctl.proc_pid()));
                }
                Err(e) => return Err(Waitpid(e)),
            };

            debug!(target: "tracer", "received new thread status: {status:?}");
            if let Some(stop) = self.apply_new_status(tcx, status)? {
                // if stop fired by quiet signal - go to next iteration, this will inject signal at
                // a tracee process and resume it
                if let StopReason::SignalStop(_, signal) = stop
                    && QUIET_SIGNALS.contains(&signal)
                {
                    continue;
                }

                debug!(target: "tracer", "debugee stopped, reason: {stop:?}");
                return Ok(stop);
            }
        }
    }

    /// Interrupt (pause) execution of the whole debugee process.
    ///
    /// This is a best-effort group-stop implemented via `PTRACE_INTERRUPT` for all running tracees.
    /// The function does not return a `StopReason` because the stop is artificial from the debugger side.
    pub fn pause(&mut self, tcx: TraceContext) -> Result<(), Error> {
        // `Pid::from_raw(-1)` means: there is no already-stopped initiator thread.
        self.group_stop_interrupt(tcx, Pid::from_raw(-1))?;
        Ok(())
    }

    fn group_stop_in_progress(&self) -> bool {
        self.group_stop_guard
    }

    fn lock_group_stop(&mut self) {
        self.group_stop_guard = true
    }

    fn unlock_group_stop(&mut self) {
        self.group_stop_guard = false
    }

    /// For stop whole debugee process this function stops tracees (threads) one by one
    /// using PTRACE_INTERRUPT request.
    ///
    /// Stops only already running tracees.
    ///
    /// If tracee receives signals before interrupt - then tracee in signal-stop and no need to interrupt it.
    ///
    /// # Arguments
    ///
    /// * `initiator_pid`: tracee with this thread id already stopped, there is no need to interrupt it.
    fn group_stop_interrupt(&mut self, tcx: TraceContext, initiator_pid: Pid) -> Result<(), Error> {
        if self.group_stop_in_progress() {
            return Ok(());
        }
        self.lock_group_stop();

        debug!(
            target: "tracer",
            "initiate group stop, initiator: {initiator_pid}, debugee state: {:?}",
            self.tracee_ctl.snapshot()
        );

        let non_stopped_exist = self
            .tracee_ctl
            .tracee_iter()
            .any(|t| t.pid != initiator_pid);
        if !non_stopped_exist {
            // no need to group-stop
            debug!(
                target: "tracer",
                "group stop complete, debugee state: {:?}",
                self.tracee_ctl.snapshot()
            );
            self.unlock_group_stop();
            return Ok(());
        }

        // two rounds, cause may be new tracees at first round, they stopped at round 2
        for _ in 0..2 {
            let tracees = self.tracee_ctl.snapshot();

            for tid in tracees.into_iter().map(|t| t.pid) {
                // load current tracee snapshot
                let mut tracee = match self.tracee_ctl.tracee(tid) {
                    None => continue,
                    Some(tracee) => {
                        if tracee.is_stopped() {
                            continue;
                        } else {
                            tracee.clone()
                        }
                    }
                };

                if let Err(e) = sys::ptrace::interrupt(tracee.pid) {
                    // if no such process - continue, it will be removed later, on PTRACE_EVENT_EXIT event.
                    if Errno::ESRCH == e {
                        warn!("thread {} not found, ESRCH", tracee.pid);
                        if let Some(t) = self.tracee_ctl.tracee_mut(tracee.pid) {
                            t.set_stop(StopType::Interrupt);
                        }
                        continue;
                    }
                    return Err(Ptrace(e));
                }

                let mut wait = tracee.wait_one()?;

                while !matches!(wait, WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP)) {
                    let stop = self.apply_new_status(tcx, wait)?;
                    match stop {
                        None => {}
                        Some(StopReason::Breakpoint(pid, _))
                        | Some(StopReason::Watchpoint(pid, _, _)) => {
                            // tracee already stopped cause breakpoint or watchpoint are reached
                            if pid == tracee.pid {
                                break;
                            }
                        }
                        Some(StopReason::DebugeeExit(code)) => return Err(ProcessExit(code)),
                        Some(StopReason::DebugeeStart) => {
                            unreachable!("stop at debugee entry point twice")
                        }
                        Some(StopReason::SignalStop(_, _)) => {
                            // tracee in signal-stop
                            break;
                        }
                        Some(StopReason::NoSuchProcess(_)) => {
                            // expect that tracee will be removed later
                            break;
                        }
                    }

                    // reload tracee, it states must be changed after handle signal
                    tracee = match self.tracee_ctl.tracee(tracee.pid).cloned() {
                        None => break,
                        Some(t) => t,
                    };
                    if tracee.is_stopped()
                        && matches!(tracee.status, TraceeStatus::Stopped(StopType::Interrupt))
                    {
                        break;
                    }

                    wait = tracee.wait_one()?;
                }

                if let Some(t) = self.tracee_ctl.tracee_mut(tracee.pid)
                    && !t.is_stopped()
                {
                    t.set_stop(StopType::Interrupt);
                }
            }
        }

        self.unlock_group_stop();

        debug!(
            target: "tracer",
            "group stop complete, debugee state: {:?}",
            self.tracee_ctl.snapshot()
        );

        Ok(())
    }

    /// Handle tracee event fired by `wait` syscall.
    /// After this function ends tracee_ctl must be in consistent state.
    /// If debugee process stop detected - returns a stop reason.
    ///
    /// # Arguments
    ///
    /// * `status`: new status returned by `waitpid`.
    fn apply_new_status(
        &mut self,
        tcx: TraceContext,
        status: WaitStatus,
    ) -> Result<Option<StopReason>, Error> {
        match status {
            WaitStatus::Exited(pid, code) => {
                // Thread exited with tread id
                self.tracee_ctl.remove(pid);
                if pid == self.tracee_ctl.proc_pid() {
                    return Ok(Some(StopReason::DebugeeExit(code)));
                }
                Ok(None)
            }
            WaitStatus::PtraceEvent(pid, _signal, code) => {
                match code {
                    libc::PTRACE_EVENT_EXEC => {
                        // fire just before debugee start
                        // cause currently `fork()`
                        // in debugee is unsupported we expect this code to call once
                        self.tracee_ctl.add(pid);
                        return Ok(Some(StopReason::DebugeeStart));
                    }
                    libc::PTRACE_EVENT_CLONE => {
                        // fire just before new thread created
                        self.tracee_ctl
                            .tracee_ensure_mut(pid)
                            .set_stop(StopType::Interrupt);
                        let new_thread_id =
                            Pid::from_raw(sys::ptrace::getevent(pid).map_err(Ptrace)? as pid_t);

                        // PTRACE_EVENT_STOP may be received first, and new tracee may be already registered at this point
                        if self.tracee_ctl.tracee_mut(new_thread_id).is_none() {
                            let new_tracee = self.tracee_ctl.add(new_thread_id);
                            let new_trace_status = new_tracee.wait_one()?;
                            if matches!(new_trace_status, WaitStatus::Exited(_, _)) {
                                // this situation can occur if the process has already completed
                                self.tracee_ctl.remove(new_thread_id);
                            } else {
                                // all watchpoints must be distributed to a new tracee
                                weak_error!(tcx.watchpoints.distribute_to_tracee(new_tracee));

                                debug_assert!(
                                    matches!(
                                        new_trace_status,
                                        WaitStatus::PtraceEvent(tid, _, libc::PTRACE_EVENT_STOP) if tid == new_thread_id
                                    ),
                                    "the newly cloned thread must start with PTRACE_EVENT_STOP (cause PTRACE_SEIZE was used), got {new_trace_status:?}"
                                )
                            }
                        }
                    }
                    libc::PTRACE_EVENT_STOP => {
                        // fire right after new thread started or PTRACE_INTERRUPT called.
                        match self.tracee_ctl.tracee_mut(pid) {
                            Some(tracee) => tracee.set_stop(StopType::Interrupt),
                            None => {
                                let tracee = self.tracee_ctl.add(pid);
                                weak_error!(tcx.watchpoints.distribute_to_tracee(tracee));
                            }
                        }
                    }
                    libc::PTRACE_EVENT_EXIT => {
                        // Stop the tracee at exit
                        let tracee = self.tracee_ctl.remove(pid);
                        if let Some(mut tracee) = tracee {
                            // TODO
                            // There is one interesting situation, when tracee may not exist
                            // at this point (according to ptrace documentation, it must exist).
                            // Tracee not exist when thread created inside `std::thread::scoped`.
                            // This can be verified by running watchpoints functional tests.
                            // It is a flaky behavior, but sometimes an error
                            // will be returned at this point.
                            // Currently error here muted, but this behaviour NFR.
                            _ = tracee.r#continue(None);
                        }
                    }
                    _ => {
                        warn!("unsupported (ignored) ptrace event, code: {code}");
                    }
                }
                Ok(None)
            }
            WaitStatus::Stopped(pid, signal) => {
                let info = match sys::ptrace::getsiginfo(pid) {
                    Ok(info) => info,
                    Err(Errno::ESRCH) => return Ok(Some(StopReason::NoSuchProcess(pid))),
                    Err(e) => return Err(Ptrace(e)),
                };

                match signal {
                    Signal::SIGTRAP => match info.si_code {
                        code::TRAP_TRACE => {
                            todo!()
                        }
                        code::TRAP_BRKPT | code::SI_KERNEL => {
                            // Compute the trap's PC (after arch-specific rewind) and
                            // match it against our installed breakpoints *before*
                            // mutating tracee state, so unrelated debuggee traps
                            // (e.g. __builtin_trap / BRK #1000 on aarch64, or a
                            // user-level INT3 on x86) don't get misattributed.
                            let trap_pc = RelocatedAddress::from(
                                self.tracee_ctl.tracee_ensure(pid).pc()?.as_u64()
                                    - crate::debugger::breakpoint::Breakpoint::PC_ADJUST,
                            );
                            let mb_hit_brkpt = tcx
                                .breakpoints
                                .iter()
                                .find(|brkpt| brkpt.addr == trap_pc);
                            let Some(&brkpt) = mb_hit_brkpt else {
                                // A trap we didn't install — surface it as a
                                // SIGTRAP signal-stop so the UI can report and
                                // backtrace from it instead of panicking.
                                self.tracee_ctl
                                    .tracee_ensure_mut(pid)
                                    .set_stop(StopType::SignalStop(signal));
                                if !QUIET_SIGNALS.contains(&signal) {
                                    self.group_stop_interrupt(tcx, pid)?;
                                }
                                return Ok(Some(StopReason::SignalStop(pid, signal)));
                            };
                            // It's one of ours — apply the PC rewind now so the
                            // rest of the handler sees the corrected PC.
                            self.tracee_ctl
                                .tracee_ensure(pid)
                                .set_pc(trap_pc.as_u64())?;
                            let current_pc = trap_pc;

                            let has_tmp_breakpoints = tcx
                                .breakpoints
                                .iter()
                                .any(|b| b.is_temporary() | b.is_temporary_async());
                            if has_tmp_breakpoints {
                                let temporary_hit = brkpt.is_temporary() && pid == brkpt.pid;
                                let temporary_async_hit = brkpt.is_temporary_async();
                                let watchpoint_hit = brkpt.is_wp_companion();
                                if !temporary_hit && !watchpoint_hit && !temporary_async_hit {
                                    let mut unusual_brkpt = brkpt.clone();
                                    unusual_brkpt.pid = pid;
                                    if unusual_brkpt.is_enabled() {
                                        unusual_brkpt.disable()?;
                                        while self.single_step(tcx, pid)?.is_some() {}
                                        unusual_brkpt.enable()?;
                                    }
                                    self.tracee_ctl
                                        .tracee_ensure_mut(pid)
                                        .set_stop(StopType::Interrupt);

                                    return Ok(None);
                                }
                            }

                            self.tracee_ctl
                                .tracee_ensure_mut(pid)
                                .set_stop(StopType::Interrupt);
                            self.group_stop_interrupt(tcx, pid)?;

                            if let BrkptType::WatchpointCompanion(wps) = brkpt.r#type() {
                                return Ok(Some(StopReason::Watchpoint(
                                    pid,
                                    current_pc,
                                    WatchpointHitType::EndOfScope(wps.clone()),
                                )));
                            }

                            Ok(Some(StopReason::Breakpoint(pid, current_pc)))
                        }
                        code::TRAP_HWBKPT => {
                            let current_pc = {
                                let tracee = self.tracee_ctl.tracee_ensure(pid);
                                tracee.pc()?
                            };

                            self.tracee_ctl
                                .tracee_ensure_mut(pid)
                                .set_stop(StopType::Interrupt);
                            self.group_stop_interrupt(tcx, pid)?;

                            let mut state = register::debug::HardwareDebugState::current(pid)?;
                            // x86 recovers the firing slot from DR6; aarch64
                            // doesn't have a per-slot trap bit, so it needs
                            // `si_addr` (the faulting byte) to match against
                            // each slot's watched bytes. Both arches accept
                            // the same `Option<usize>` and ignore it when not
                            // needed.
                            let si_addr = unsafe { info.si_addr() } as usize;
                            let reg = state
                                .detect_and_flush_hit(Some(si_addr))
                                .expect("watchpoint fired but no slot matched");
                            state.sync(pid)?;
                            let hit_type = WatchpointHitType::DebugRegister(reg);
                            Ok(Some(StopReason::Watchpoint(pid, current_pc, hit_type)))
                        }
                        code => {
                            debug!(
                                target: "tracer",
                                "unexpected SIGTRAP code {code}",
                            );
                            Ok(None)
                        }
                    },
                    _ => {
                        if !TRANSPARENT_SIGNALS.contains(&signal) {
                            self.inject_signal_queue.push_back((pid, signal));
                        }

                        self.tracee_ctl
                            .tracee_ensure_mut(pid)
                            .set_stop(StopType::SignalStop(signal));

                        if !QUIET_SIGNALS.contains(&signal) {
                            self.group_stop_interrupt(tcx, pid)?;
                        }

                        Ok(Some(StopReason::SignalStop(pid, signal)))
                    }
                }
            }
            WaitStatus::Signaled(_, _, _) => Ok(None),
            _ => {
                warn!("unexpected wait status: {status:?}");
                Ok(None)
            }
        }
    }

    /// Execute next instruction, then stop with `TRAP_TRACE`.
    ///
    /// # Arguments
    ///
    /// * `tcx`: trace context
    /// * `pid`: tracee pid
    ///
    /// returns: [`None`] if an instruction step is done successfully.
    /// A [`StopReason::SignalStop`] returned if step interrupt causes tracee in a signal-stop.
    /// A [`StopReason::Watchpoint`] returned if step interrupt causes hardware breakpoint is hit.
    /// Error returned otherwise.
    pub fn single_step(
        &mut self,
        tcx: TraceContext,
        pid: Pid,
    ) -> Result<Option<StopReason>, Error> {
        let tracee = self.tracee_ctl.tracee_ensure(pid);
        let initial_pc = tracee.pc()?;
        tracee.step(None)?;

        let reason = loop {
            let tracee = self.tracee_ctl.tracee_ensure_mut(pid);
            let status = tracee.wait_one()?;
            let info = sys::ptrace::getsiginfo(pid).map_err(Ptrace)?;

            // check that debugee step into an expected trap
            // (breakpoints ignored and are also considered as a trap)
            let in_trap = matches!(status, WaitStatus::Stopped(_, Signal::SIGTRAP))
                && (info.si_code == code::TRAP_TRACE
                    || info.si_code == code::TRAP_BRKPT
                    || info.si_code == code::SI_KERNEL
                    || info.si_code == code::TRAP_HWBKPT);
            if in_trap {
                let pc = tracee.pc()?;
                // check that we aren't on original pc value
                if pc == initial_pc {
                    tracee.step(None)?;
                    continue;
                }

                let mut state = register::debug::HardwareDebugState::current(pid)?;
                let si_addr = unsafe { info.si_addr() } as usize;
                let maybe_dr = state.detect_and_flush_hit(Some(si_addr));
                state.sync(pid)?;
                if let Some(dr) = maybe_dr {
                    let hit_type = WatchpointHitType::DebugRegister(dr);
                    break Some(StopReason::Watchpoint(pid, pc, hit_type));
                }

                let mb_brkpt = tcx.breakpoints.iter().find(|brkpt| brkpt.addr == pc);
                if let Some(BrkptType::WatchpointCompanion(wps)) = mb_brkpt.map(|b| b.r#type()) {
                    let hit_type = WatchpointHitType::EndOfScope(wps.clone());
                    break Some(StopReason::Watchpoint(pid, pc, hit_type));
                }

                break None;
            }

            let in_trap =
                matches!(status, WaitStatus::Stopped(_, Signal::SIGTRAP)) && (info.si_code == 5);
            if in_trap {
                // if in syscall step to syscall end
                sys::ptrace::syscall(tracee.pid, None).map_err(Ptrace)?;
                let syscall_status = tracee.wait_one()?;
                debug_assert!(matches!(
                    syscall_status,
                    WaitStatus::Stopped(_, Signal::SIGTRAP)
                ));

                // then do step again
                tracee.step(None)?;

                continue;
            }

            let is_interrupt = matches!(
                status,
                WaitStatus::PtraceEvent(p, SIGSTOP, libc::PTRACE_EVENT_STOP) if pid == p,
            );
            if is_interrupt {
                break None;
            }

            let stop = self.apply_new_status(tcx, status)?;
            match stop {
                None => {}
                Some(StopReason::Breakpoint(_, _)) => {
                    unreachable!("breakpoints must be ignore");
                }
                Some(StopReason::Watchpoint(_, _, _)) => {
                    unreachable!("watchpoints must be ignore");
                }
                Some(StopReason::DebugeeExit(code)) => return Err(ProcessExit(code)),
                Some(StopReason::DebugeeStart) => {
                    unreachable!("stop at debugee entry point twice")
                }
                Some(StopReason::SignalStop(_, signal)) => {
                    if QUIET_SIGNALS.contains(&signal) {
                        self.tracee_ctl.tracee_ensure(pid).step(Some(signal))?;
                        continue;
                    }

                    // tracee in signal-stop
                    break stop;
                }
                Some(StopReason::NoSuchProcess(_)) => {
                    // expect that tracee will be removed later
                    break None;
                }
            }
        };
        Ok(reason)
    }
}

/// Darwin POC of `impl Tracer`. Uses macOS-flavoured ptrace
/// (`PT_CONTINUE` / `PT_STEP` via the BSD branch of `nix`) plus
/// `waitpid` to drive the wait/event loop. This works for
/// single-thread debuggees attached via `Child::install`'s
/// `PT_TRACE_ME` path because `BRK #0` exceptions get demoted by
/// the kernel into `SIGTRAP` when the task has no exception port
/// installed.
///
/// The full Mach exception-ports loop (multi-thread,
/// `EXC_BREAKPOINT` / `EXC_SOFTWARE` / `EXC_BAD_ACCESS` routed
/// through a dedicated Mach port) is the later, richer
/// implementation that lands once the POC is stable.
#[cfg(not(target_os = "linux"))]
impl Tracer {
    pub fn new(proc_pid: Pid) -> Self {
        Self {
            tracee_ctl: TraceeCtl::new(proc_pid),
            darwin_state: None,
            darwin_seen_initial_stop: false,
        }
    }

    pub fn new_external(proc_pid: Pid, threads: &[Pid]) -> Self {
        Self {
            tracee_ctl: TraceeCtl::new_external(proc_pid, threads),
            darwin_state: None,
            // Attached to a running process — no initial-stop event.
            darwin_seen_initial_stop: true,
        }
    }

    /// Path A: pure Mach. Lazy-init the exception port + register
    /// it on the task on first call. Subsequent calls reuse the
    /// cached state. No ptrace involvement — `Child::install`
    /// spawned the child via `posix_spawn(POSIX_SPAWN_START_SUSPENDED)`,
    /// so we own the suspend state from the start.
    fn ensure_darwin_supervision(&mut self) -> Result<&mut DarwinSupervision, Error> {
        if self.darwin_state.is_none() {
            use crate::debugger::darwin_mach::{self, DyldNotifyPort, ExceptionPort};
            let pid = self.tracee_ctl.proc_pid();
            let task = darwin_mach::task_for_pid(pid)?;
            let port = ExceptionPort::allocate()?;
            port.register(task)?;
            // Registering the dyld notify port can fail right after
            // posix_spawn-suspend if dyld hasn't yet built its
            // notifyPorts table. We retry on demand from the resume
            // loop below — until then, deferred-BP resolution falls
            // back to the SW BP at `_lldb_image_notifier` (which is
            // typically a no-op on dyld 4 but harmless).
            let mut dyld_notify = DyldNotifyPort::allocate().ok();
            if let Some(np) = dyld_notify.as_mut()
                && np.register(task).is_err()
            {
                dyld_notify = None;
            }
            // Seed the per-pid thread-port registry for the main
            // thread. Right after posix_spawn-suspend the inferior
            // has exactly one thread; bind that port to `proc_pid`
            // so the existing single-thread RegisterMap callers
            // continue to resolve correctly.
            let mut thread_id_to_pid = std::collections::HashMap::new();
            let mut pid_to_thread_id = std::collections::HashMap::new();
            if let Ok(main_thread) = darwin_mach::first_thread_of(task) {
                darwin_mach::set_thread_port(pid, main_thread);
                if let Ok(id) = darwin_mach::thread_identity(main_thread) {
                    thread_id_to_pid.insert(id.thread_id, pid);
                    pid_to_thread_id.insert(pid, id.thread_id);
                }
            }
            self.darwin_state = Some(DarwinSupervision {
                task,
                proc_pid: pid,
                port,
                pending_reply: std::cell::Cell::new(None),
                dyld_notify,
                thread_id_to_pid,
                pid_to_thread_id,
                next_synthetic_pid: pid.as_raw().saturating_add(1_000_000),
            });
        }
        Ok(self.darwin_state.as_mut().expect("just initialised"))
    }

    /// Bring `tracee_ctl` and the per-pid thread-port registry into
    /// sync with the inferior's current thread set.
    ///
    /// Called after every Mach exception, where new threads may have
    /// appeared (pthread_create) or old ones disappeared (thread
    /// returned from start fn → kernel terminated). We:
    ///
    /// * enumerate live threads via `task_threads_vec`,
    /// * map each port → kernel thread_id, allocate a synthetic Pid
    ///   if we haven't seen this thread before,
    /// * insert/update `darwin_mach`'s pid → port registry so
    ///   `RegisterMap::current(pid)` resolves to the right thread,
    /// * add new tracees to `tracee_ctl`, drop tracees for threads
    ///   that aren't live any more.
    ///
    /// Returns the synthetic Pid corresponding to `faulting_port`
    /// (so the caller knows which thread to report to the engine).
    fn reconcile_threads(
        &mut self,
        faulting_port: Option<mach2::mach_types::thread_act_t>,
    ) -> Result<Option<nix::unistd::Pid>, Error> {
        use crate::debugger::darwin_mach;
        use std::collections::HashSet;

        let state = self.darwin_state.as_mut().expect("supervision must exist");
        let live = darwin_mach::task_threads_vec(state.task)
            .map_err(|e| Error::from(e))?;

        let mut seen_tids: HashSet<u64> = HashSet::new();
        let mut faulting_pid: Option<nix::unistd::Pid> = None;
        for &port in &live {
            let id = match darwin_mach::thread_identity(port) {
                Ok(i) => i,
                Err(_) => continue, // thread terminated mid-enumerate
            };
            seen_tids.insert(id.thread_id);
            let pid = if let Some(&existing) = state.thread_id_to_pid.get(&id.thread_id) {
                // The Mach port name can change across resumes (the
                // kernel rotates send-once rights); refresh the
                // registry every iteration so RegisterMap::current
                // never holds a stale port.
                darwin_mach::set_thread_port(existing, port);
                existing
            } else {
                let new_pid = nix::unistd::Pid::from_raw(state.next_synthetic_pid);
                state.next_synthetic_pid = state.next_synthetic_pid.saturating_add(1);
                state.thread_id_to_pid.insert(id.thread_id, new_pid);
                state.pid_to_thread_id.insert(new_pid, id.thread_id);
                darwin_mach::set_thread_port(new_pid, port);
                self.tracee_ctl.add(new_pid);
                new_pid
            };
            if Some(port) == faulting_port {
                faulting_pid = Some(pid);
            }
        }

        // Drop tracees for threads that have exited. Walk a snapshot
        // because tracee_ctl::remove mutates the underlying map.
        let dead: Vec<_> = self
            .darwin_state
            .as_ref()
            .unwrap()
            .pid_to_thread_id
            .iter()
            .filter_map(|(pid, tid)| (!seen_tids.contains(tid)).then_some((*pid, *tid)))
            .collect();
        let state = self.darwin_state.as_mut().unwrap();
        for (pid, tid) in dead {
            // Never drop the proc_pid tracee — even after the main
            // thread "ends" the engine still uses proc_pid as the
            // process identity. The process is gone only when
            // waitpid says so.
            if pid == state.proc_pid {
                continue;
            }
            state.thread_id_to_pid.remove(&tid);
            state.pid_to_thread_id.remove(&pid);
            darwin_mach::clear_thread_port(pid);
            self.tracee_ctl.remove(pid);
        }

        Ok(faulting_pid)
    }

    pub fn resume(&mut self, tcx: TraceContext) -> Result<StopReason, Error> {
        use crate::debugger::darwin_mach::{self, ExceptionPort};
        use crate::debugger::register::RegisterMap;
        use mach2::kern_return::{KERN_FAILURE, KERN_SUCCESS};

        let pid = self.tracee_ctl.proc_pid();

        // First call after Child::install: the inferior is in the
        // POSIX_SPAWN_START_SUSPENDED stop state and the engine
        // needs to see DebugeeStart so it can initialise the debug
        // info registry. We don't actually run the inferior here —
        // we just confirm Mach supervision is up and report the
        // synthetic start event. The next resume() drives the loop.
        if !self.darwin_seen_initial_stop {
            self.ensure_darwin_supervision()?;
            self.darwin_seen_initial_stop = true;
            return Ok(StopReason::DebugeeStart);
        }

        // Classify. aarch64 darwin Mach exception encoding:
        //   EXC_BREAKPOINT (6) + codes[0]=EXC_ARM_BREAKPOINT (1)
        //                                          → BRK instr
        //   EXC_BAD_ACCESS (1) + codes[0]=EXC_ARM_DA_DEBUG (0x102)
        //                       + codes[1]=fault addr (FAR_EL1)
        //                                          → HW watchpoint
        //   EXC_BAD_ACCESS (1) + codes[0]=KERN_INVALID_ADDRESS,...
        //                                          → memory fault
        //   EXC_SOFTWARE   (5) + codes[0]=EXC_SOFT_SIGNAL (0x10003)
        //                       + codes[1]=signal number
        //                                          → Unix signal
        const EXC_BREAKPOINT: i32 = 6;
        const EXC_BAD_ACCESS: i32 = 1;
        const EXC_SOFTWARE: i32 = 5;
        const EXC_ARM_BREAKPOINT: i64 = 1;
        const EXC_ARM_DA_DEBUG: i64 = 0x102;
        const EXC_SOFT_SIGNAL: i64 = 0x10003;

        // Outer loop: lets us swallow stray BRKs (e.g. dyld's
        // `_dyld_debugger_notification` on darwin, fired from
        // inside dyld on every dylib load) without surfacing them
        // to the user as a SignalStop. We advance PC past the
        // unknown BRK and re-resume.
        loop {
            let state = self.ensure_darwin_supervision()?;

            // Reply to the previously-saved exception (if any) —
            // that unblocks the kernel-side handler chain so the
            // parked thread continues from the fault. `retcode` is
            // KERN_SUCCESS for everything we consume locally
            // (breakpoints, watchpoints) and KERN_FAILURE for soft
            // signals we want to forward to the BSD signal layer
            // so the user's signal handler runs.
            if let Some((remote, id, retcode)) = state.pending_reply.take() {
                ExceptionPort::reply(remote, id, retcode)?;
            }

            // If we never managed to register the dyld notify port at
            // setup time (typical right after posix_spawn-suspend),
            // try once more now that the inferior has started running.
            // Once registered, every dlopen/dlclose surfaces here as
            // a `LinkerMapFn` event without needing a SW BP in dyld.
            if state.dyld_notify.is_none() {
                use crate::debugger::darwin_mach::DyldNotifyPort;
                if let Ok(mut np) = DyldNotifyPort::allocate()
                    && np.register(state.task).is_ok()
                {
                    state.dyld_notify = Some(np);
                }
            }

            // Resume the inferior. After Child::install's
            // spawn-suspend, the task suspend count is 1; this drops
            // it to 0 and the child runs.
            darwin_mach::task_resume(state.task)?;

            // Poll the exception port + dyld notify port + waitpid in
            // turn. The Mach exception path covers BRK / WP / signals,
            // but image-load notifications come on a separate port
            // (registered in `ensure_darwin_supervision`); we drain it
            // first on every iteration so a dlopen that finishes
            // between exception receives doesn't get lost. The kernel
            // does NOT raise a Mach exception for clean process exit
            // (return 0 from main), so we waitpid for that.
            //
            // The 50 ms poll on the exception port is a balance
            // between dyld-notify responsiveness (fast inferiors run
            // print_sum within microseconds of dlopen, so we want
            // tight latency) and not burning CPU on idle waits.
            let exc = loop {
                use crate::debugger::darwin_mach::{DyldNotifyMsg, DyldNotifyPort};

                // Drain any queued dyld notifications first.
                let mut got_image_change = false;
                while let Some(notify) = state.dyld_notify.as_ref() {
                    match notify.poll(0)? {
                        None => break,
                        Some(msg) => {
                            // Every message dyld sends is synchronous
                            // (`mach_msg(MACH_SEND_MSG|MACH_RCV_MSG)`),
                            // so reply IMMEDIATELY for every kind —
                            // Load and Unload included — or dyld
                            // wedges in `mach_msg_overwrite`.
                            let (rp, id) = match &msg {
                                DyldNotifyMsg::Load {
                                    remote_port,
                                    msg_id,
                                    ..
                                }
                                | DyldNotifyMsg::Unload {
                                    remote_port,
                                    msg_id,
                                    ..
                                }
                                | DyldNotifyMsg::Event {
                                    remote_port,
                                    msg_id,
                                } => (*remote_port, *msg_id),
                            };
                            let _ = DyldNotifyPort::reply_to_event(rp, id);
                            if matches!(
                                msg,
                                DyldNotifyMsg::Load { .. } | DyldNotifyMsg::Unload { .. }
                            ) {
                                got_image_change = true;
                            }
                        }
                    }
                }
                if got_image_change
                    && let Some(bp) = tcx.breakpoints.iter().find(|b| {
                        matches!(
                            b.r#type(),
                            crate::debugger::breakpoint::BrkptType::LinkerMapFn
                        )
                    })
                {
                    // Surface the event through the same higher-level
                    // handler the SW-BP path used. The synthesised PC
                    // won't match the inferior's current PC;
                    // `step_over_breakpoint` becomes a no-op (no BP
                    // at PC) and the `BrkptType::LinkerMapFn` arm
                    // runs `refresh_deferred` as before. If no
                    // LinkerMapFn BP is registered yet (e.g. during
                    // dyld's initial init flood — entry-point hasn't
                    // installed it yet), we silently drain the
                    // notifications.
                    darwin_mach::task_suspend(state.task)?;
                    return Ok(StopReason::Breakpoint(pid, bp.addr));
                }

                match state.port.receive(50)? {
                    Some(e) => break e,
                    None => {
                        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
                        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                            Ok(WaitStatus::Exited(_, code)) => {
                                return Ok(StopReason::DebugeeExit(code));
                            }
                            Ok(WaitStatus::Signaled(_, sig, _)) => {
                                return Ok(StopReason::DebugeeExit(128 + sig as i32));
                            }
                            _ => continue,
                        }
                    }
                }
            };

            // Suspend the task again so the rest of the threads
            // don't keep running while the user inspects coherent
            // state. The faulting thread is already parked by the
            // kernel awaiting our reply.
            darwin_mach::task_suspend(state.task)?;
            // Default reply action is KERN_SUCCESS — the EXC_SOFT_SIGNAL
            // arm below switches it to KERN_FAILURE so the BSD signal
            // layer takes over and the user's signal handler runs.
            state
                .pending_reply
                .set(Some((exc.remote_port, exc.msg_id, KERN_SUCCESS)));
            // End the &mut borrow of `darwin_state` held via `state`
            // so we can call `reconcile_threads` (which also wants
            // &mut self).
            let _ = state;

            // Reconcile the tracee table against the live thread
            // list and translate the faulting `thread_port` into
            // the Pid the engine expects. For single-thread
            // inferiors this collapses to `pid == proc_pid`.
            let faulting_pid = self
                .reconcile_threads(Some(exc.thread_port))?
                .unwrap_or(pid);
            // From here on use the faulting Pid so RegisterMap
            // reads/writes target the correct thread.
            let pid = faulting_pid;
            let raw_pc = RegisterMap::current(pid)?.pc();

            return match exc.exception {
                EXC_BREAKPOINT if exc.codes.first().copied() == Some(EXC_ARM_BREAKPOINT) => {
                    let candidate_pc = crate::debugger::address::RelocatedAddress::from(
                        raw_pc - crate::debugger::breakpoint::Breakpoint::PC_ADJUST,
                    );
                    let is_ours = tcx.breakpoints.iter().any(|b| b.addr == candidate_pc);
                    if is_ours {
                        Ok(StopReason::Breakpoint(pid, candidate_pc))
                    } else {
                        // Stray BRK — most commonly dyld's
                        // `_dyld_debugger_notification` on darwin,
                        // which dyld hits internally on every
                        // dylib load to give a debugger a chance
                        // to refresh its module table. We don't
                        // (yet) consume these as proper rendezvous
                        // notifications, but we must not crash on
                        // them either: skip the 4-byte BRK and
                        // re-arm. The mapping/global-PC lookup
                        // would fail for dyld pages anyway since
                        // the registry tracks only modules with
                        // their own DWARF.
                        let mut regs = RegisterMap::current(pid)?;
                        regs.set_pc(raw_pc + 4);
                        regs.persist(pid)?;
                        // Continue the outer loop to re-resume.
                        continue;
                    }
                }
                EXC_BAD_ACCESS if exc.codes.first().copied() == Some(EXC_ARM_DA_DEBUG) => {
                    let fault_addr = exc.codes.get(1).copied().unwrap_or(0) as usize;
                    let mut state =
                        crate::debugger::register::debug::HardwareDebugState::current(pid)?;
                    if let Some(dr) = state.detect_and_flush_hit(Some(fault_addr)) {
                        let _ = state.sync(pid);
                        Ok(StopReason::Watchpoint(
                            pid,
                            crate::debugger::address::RelocatedAddress::from(raw_pc as usize),
                            WatchpointHitType::DebugRegister(dr),
                        ))
                    } else {
                        Ok(StopReason::SignalStop(pid, nix::sys::signal::SIGTRAP))
                    }
                }
                EXC_SOFTWARE if exc.codes.first().copied() == Some(EXC_SOFT_SIGNAL) => {
                    let signum = exc.codes.get(1).copied().unwrap_or(0) as i32;
                    let signal = nix::sys::signal::Signal::try_from(signum)
                        .unwrap_or(nix::sys::signal::SIGTRAP);
                    // Forward the soft signal to the BSD signal layer so
                    // the user's signal handler runs once we resume.
                    // KERN_SUCCESS would tell the kernel "debugger
                    // consumed this signal" — silently dropping it.
                    //
                    // Note: in practice this arm rarely fires today —
                    // the kernel only routes async signals (kill())
                    // through Mach when the inferior has been
                    // ptrace-attached (PT_ATTACHEXC). Without ptrace
                    // the signal goes straight to the BSD path and we
                    // never see it. The arm stays so synchronous
                    // signal-like exceptions (e.g. raise()) still
                    // surface as SignalStop, and so the codepath is
                    // ready when PT_ATTACHEXC lands.
                    if let Some(s) = self.darwin_state.as_ref() {
                        s.pending_reply
                            .set(Some((exc.remote_port, exc.msg_id, KERN_FAILURE)));
                    }
                    Ok(StopReason::SignalStop(pid, signal))
                }
                _ => Ok(StopReason::SignalStop(pid, nix::sys::signal::SIGTRAP)),
            };
        }
    }

    pub fn pause(&mut self, _tcx: TraceContext) -> Result<(), Error> {
        // task_suspend bumps the kernel's suspend count, parking
        // every thread. Idempotent vs the count we already hold
        // from the most recent resume() — the next resume will
        // task_resume to drop back to 0.
        if let Some(state) = self.darwin_state.as_ref() {
            crate::debugger::darwin_mach::task_suspend(state.task)?;
        }
        Ok(())
    }

    pub fn single_step(
        &mut self,
        _tcx: TraceContext,
        pid: Pid,
    ) -> Result<Option<StopReason>, Error> {
        use crate::debugger::darwin_mach::{self, ExceptionPort};
        use crate::debugger::register::RegisterMap;
        use mach2::kern_return::KERN_SUCCESS;

        let state = self.ensure_darwin_supervision()?;

        // Reply to any prior pending exception so the parked
        // thread can leave the exception handler before we re-arm.
        if let Some((remote, id, retcode)) = state.pending_reply.take() {
            ExceptionPort::reply(remote, id, retcode)?;
        }

        // Arm software single-step on the *focus* thread — the one
        // the caller asked to step. Falls back to first_thread_of
        // for legacy single-thread paths that haven't been
        // registered yet (early init).
        let focus = darwin_mach::thread_port_for_pid_or_first(pid)?;
        darwin_mach::arm_set_single_step(focus, true)?;

        // Suspend every other thread so only `focus` runs while we
        // step. Without this, a worker thread can hit one of our
        // breakpoints during the brief task_resume window and the
        // resulting EXC_BREAKPOINT gets consumed here as if it were
        // our step trap — leaving the real step trap parked and
        // mis-attributing the BP hit. Linux gets this for free
        // because PTRACE_SINGLESTEP is per-tid.
        let live_threads = darwin_mach::task_threads_vec(state.task).unwrap_or_default();
        let mut suspended = Vec::with_capacity(live_threads.len());
        for &t in &live_threads {
            if t == focus {
                continue;
            }
            if darwin_mach::thread_suspend(t).is_ok() {
                suspended.push(t);
            }
        }

        // Resume — the kernel executes one instruction then traps.
        darwin_mach::task_resume(state.task)?;

        // Block until the resulting Mach exception lands. With
        // `MDSCR_EL1.SS=1 + SPSR.SS=1` the kernel reports a software
        // step as `EXC_BREAKPOINT`; on aarch64 codes[0] is unset
        // (or 0) for SS — distinct from BRK which has codes[0]=1.
        let exc = loop {
            match state.port.receive(u32::MAX)? {
                Some(e) => break e,
                None => continue,
            }
        };

        // Re-suspend so the rest of the threads stay coherent.
        darwin_mach::task_suspend(state.task)?;
        // Drop the per-thread suspend we added on every non-focus
        // thread so a subsequent resume() unblocks them. (task_suspend
        // already keeps them paused via the task-level count, so they
        // won't actually run until the next task_resume.)
        for t in suspended {
            let _ = darwin_mach::thread_resume(t);
        }
        state
            .pending_reply
            .set(Some((exc.remote_port, exc.msg_id, KERN_SUCCESS)));

        // Disarm the SS bits so the next plain resume() doesn't
        // accidentally step again. (MDSCR_EL1.SS is sticky across
        // exception entry; SPSR.SS may already be cleared but be
        // explicit.)
        let _ = darwin_mach::arm_set_single_step(focus, false);

        const EXC_BREAKPOINT: i32 = 6;
        const EXC_BAD_ACCESS: i32 = 1;
        const EXC_ARM_DA_DEBUG: i64 = 0x102;

        // Watchpoint may also fire mid-step if the stepped
        // instruction touched a watched address. Check the
        // exception type before declaring a clean step.
        if exc.exception == EXC_BAD_ACCESS
            && exc.codes.first().copied() == Some(EXC_ARM_DA_DEBUG)
        {
            let raw_pc = RegisterMap::current(pid)?.pc();
            let fault_addr = exc.codes.get(1).copied().unwrap_or(0) as usize;
            let mut hwstate =
                crate::debugger::register::debug::HardwareDebugState::current(pid)?;
            if let Some(dr) = hwstate.detect_and_flush_hit(Some(fault_addr)) {
                let _ = hwstate.sync(pid);
                return Ok(Some(StopReason::Watchpoint(
                    pid,
                    crate::debugger::address::RelocatedAddress::from(raw_pc as usize),
                    WatchpointHitType::DebugRegister(dr),
                )));
            }
        }

        // EXC_BREAKPOINT with no breakpoint registry match is the
        // step trap itself — return None so the caller knows the
        // step landed cleanly.
        if exc.exception == EXC_BREAKPOINT {
            return Ok(None);
        }

        // Anything else: surface as a SignalStop with SIGTRAP
        // (matches the linux Tracer's catch-all shape).
        Ok(Some(StopReason::SignalStop(
            pid,
            nix::sys::signal::SIGTRAP,
        )))
    }
}

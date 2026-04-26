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
    port: crate::debugger::darwin_mach::ExceptionPort,
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
    pending_reply: std::cell::Cell<Option<(u32, i32)>>,
}

#[cfg(not(target_os = "linux"))]
impl DarwinSupervision {
    pub(crate) fn task(&self) -> mach2::mach_types::task_t {
        self.task
    }
    pub(crate) fn port(&self) -> &crate::debugger::darwin_mach::ExceptionPort {
        &self.port
    }
    pub(crate) fn take_pending_reply(&self) -> Option<(u32, i32)> {
        self.pending_reply.take()
    }
    pub(crate) fn set_pending_reply(&self, v: Option<(u32, i32)>) {
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
            use crate::debugger::darwin_mach::{self, ExceptionPort};
            let pid = self.tracee_ctl.proc_pid();
            let task = darwin_mach::task_for_pid(pid)?;
            let port = ExceptionPort::allocate()?;
            port.register(task)?;
            self.darwin_state = Some(DarwinSupervision {
                task,
                port,
                pending_reply: std::cell::Cell::new(None),
            });
        }
        Ok(self.darwin_state.as_mut().expect("just initialised"))
    }

    pub fn resume(&mut self, tcx: TraceContext) -> Result<StopReason, Error> {
        use crate::debugger::darwin_mach::{self, ExceptionPort};
        use crate::debugger::register::RegisterMap;
        use mach2::kern_return::KERN_SUCCESS;

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

        let state = self.ensure_darwin_supervision()?;

        // Reply to the previously-saved exception (if any) — that
        // unblocks the kernel-side handler chain so the parked
        // thread continues from the fault.
        if let Some((remote, id)) = state.pending_reply.take() {
            ExceptionPort::reply(remote, id, KERN_SUCCESS)?;
        }

        // Resume the inferior. After Child::install's spawn-suspend,
        // the task suspend count is 1; this drops it to 0 and the
        // child runs.
        darwin_mach::task_resume(state.task)?;

        // Poll the exception port + waitpid in turn. The Mach
        // exception path covers BRK / WP / signals, but the
        // kernel does NOT raise a Mach exception for clean
        // process exit (return 0 from main). For that we need
        // waitpid to surface SIGCHLD/Exited. The 200 ms poll is
        // a balance between responsiveness on stop events and
        // not burning CPU on idle waits.
        let exc = loop {
            match state.port.receive(200)? {
                Some(e) => break e,
                None => {
                    // Port timed out — check if the inferior exited.
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

        // Suspend the task again so the rest of the threads don't
        // keep running while the user inspects coherent state. The
        // faulting thread is already parked by the kernel awaiting
        // our reply.
        darwin_mach::task_suspend(state.task)?;
        state.pending_reply.set(Some((exc.remote_port, exc.msg_id)));

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

        let raw_pc = RegisterMap::current(pid)?.pc();

        match exc.exception {
            EXC_BREAKPOINT if exc.codes.first().copied() == Some(EXC_ARM_BREAKPOINT) => {
                let candidate_pc = crate::debugger::address::RelocatedAddress::from(
                    raw_pc - crate::debugger::breakpoint::Breakpoint::PC_ADJUST,
                );
                let is_ours = tcx
                    .breakpoints
                    .iter()
                    .any(|b| b.addr == candidate_pc);
                if is_ours {
                    Ok(StopReason::Breakpoint(pid, candidate_pc))
                } else {
                    Ok(StopReason::SignalStop(pid, nix::sys::signal::SIGTRAP))
                }
            }
            EXC_BAD_ACCESS if exc.codes.first().copied() == Some(EXC_ARM_DA_DEBUG) => {
                let fault_addr =
                    exc.codes.get(1).copied().unwrap_or(0) as usize;
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
                let signum =
                    exc.codes.get(1).copied().unwrap_or(0) as i32;
                let signal = nix::sys::signal::Signal::try_from(signum)
                    .unwrap_or(nix::sys::signal::SIGTRAP);
                Ok(StopReason::SignalStop(pid, signal))
            }
            _ => Ok(StopReason::SignalStop(pid, nix::sys::signal::SIGTRAP)),
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
        if let Some((remote, id)) = state.pending_reply.take() {
            ExceptionPort::reply(remote, id, KERN_SUCCESS)?;
        }

        // Arm software single-step on the focus thread.
        // first_thread_of returns the main task thread which is
        // what `pid` aliases to in our single-thread Tracee model.
        let focus = darwin_mach::first_thread_of(state.task)?;
        darwin_mach::arm_set_single_step(focus, true)?;

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
        state.pending_reply.set(Some((exc.remote_port, exc.msg_id)));

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

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
        }
    }

    pub fn new_external(proc_pid: Pid, threads: &[Pid]) -> Self {
        Self {
            tracee_ctl: TraceeCtl::new_external(proc_pid, threads),
        }
    }

    pub fn resume(&mut self, tcx: TraceContext) -> Result<StopReason, Error> {
        use crate::debugger::register::RegisterMap;
        use nix::sys::ptrace;
        use nix::sys::wait::{WaitStatus, waitpid};

        let pid = self.tracee_ctl.proc_pid();
        loop {
            // Continue the inferior. `PT_CONTINUE` with addr=1
            // (which is what `nix::ptrace::cont` does internally on
            // BSD) means "resume from the current PC".
            ptrace::cont(pid, None).map_err(Error::Ptrace)?;
            let status = waitpid(pid, None).map_err(Error::Waitpid)?;
            match status {
                WaitStatus::Exited(_, code) => return Ok(StopReason::DebugeeExit(code)),
                WaitStatus::Signaled(_, sig, _) => {
                    // Treat fatal signals like a non-zero exit so
                    // the rest of the debugger sees a single
                    // termination shape regardless of how it ended.
                    return Ok(StopReason::DebugeeExit(128 + sig as i32));
                }
                WaitStatus::Stopped(stopped_pid, signal) => {
                    if signal == nix::sys::signal::SIGTRAP {
                        // Could be (a) an installed breakpoint
                        // firing — the BRK at PC will appear as
                        // SIGTRAP; (b) a hardware watchpoint hit
                        // — same SIGTRAP shape, but PC is *after*
                        // the faulting instruction and FAR_EL1
                        // holds the fault address; (c) a user-
                        // level BRK (`__builtin_trap`); or (d)
                        // the post-exec attach trap on the very
                        // first resume.
                        let raw_pc = RegisterMap::current(stopped_pid)?.pc();
                        let candidate_pc = crate::debugger::address::RelocatedAddress::from(
                            raw_pc - crate::debugger::breakpoint::Breakpoint::PC_ADJUST,
                        );
                        let is_ours = tcx
                            .breakpoints
                            .iter()
                            .any(|b| b.addr == candidate_pc);
                        if is_ours {
                            return Ok(StopReason::Breakpoint(stopped_pid, candidate_pc));
                        }
                        // Watchpoint check — darwin's analogue of
                        // linux's `siginfo.si_addr`/`TRAP_HWBKPT`
                        // path is the per-thread FAR_EL1 captured
                        // in `ARM_EXCEPTION_STATE64`. Match against
                        // the BAS-encoded byte set of every armed
                        // slot; on a hit, surface as Watchpoint.
                        if let Ok(task) = crate::debugger::darwin_mach::task_for_pid(stopped_pid)
                            && let Ok(thread) = crate::debugger::darwin_mach::first_thread_of(task)
                            && let Ok(exc) =
                                crate::debugger::darwin_mach::thread_get_arm_exception_state64(
                                    thread,
                                )
                            && let Ok(mut state) =
                                crate::debugger::register::debug::HardwareDebugState::current(
                                    stopped_pid,
                                )
                        {
                            if let Some(dr) =
                                state.detect_and_flush_hit(Some(exc.far as usize))
                            {
                                let _ = state.sync(stopped_pid);
                                let hit_type = WatchpointHitType::DebugRegister(dr);
                                return Ok(StopReason::Watchpoint(
                                    stopped_pid,
                                    crate::debugger::address::RelocatedAddress::from(
                                        raw_pc as usize,
                                    ),
                                    hit_type,
                                ));
                            }
                        }
                        return Ok(StopReason::SignalStop(stopped_pid, signal));
                    }
                    if signal == nix::sys::signal::SIGSTOP {
                        // First-time post-exec stop: surface as
                        // DebugeeStart so the front-end can
                        // initialise the debug-info registry.
                        return Ok(StopReason::DebugeeStart);
                    }
                    // Pass-through: signals the inferior produces
                    // for its own bookkeeping (timers, async I/O,
                    // child reaping) shouldn't drop us back to a
                    // user prompt every time they fire. Re-inject
                    // them into the inferior via `PT_CONTINUE(sig)`
                    // and keep waiting. Linux Tracer does the same
                    // via `QUIET_SIGNALS`; this is the darwin
                    // analogue.
                    use nix::sys::signal::Signal::{
                        SIGALRM, SIGCHLD, SIGIO, SIGPROF, SIGURG, SIGVTALRM,
                    };
                    if matches!(
                        signal,
                        SIGALRM | SIGURG | SIGCHLD | SIGIO | SIGVTALRM | SIGPROF
                    ) {
                        ptrace::cont(stopped_pid, Some(signal)).map_err(Error::Ptrace)?;
                        continue;
                    }
                    return Ok(StopReason::SignalStop(stopped_pid, signal));
                }
                _ => {
                    // Any other wait status (continued, etc.) — keep
                    // looping until a real stop arrives.
                    continue;
                }
            }
        }
    }

    pub fn pause(&mut self, _tcx: TraceContext) -> Result<(), Error> {
        // SIGSTOP the whole process; on darwin the next `waitpid`
        // will see the stop. The Mach-native equivalent
        // (`task_suspend`) lands with the exception-port loop.
        nix::sys::signal::kill(self.tracee_ctl.proc_pid(), nix::sys::signal::SIGSTOP)
            .map_err(Error::Ptrace)?;
        Ok(())
    }

    pub fn single_step(
        &mut self,
        _tcx: TraceContext,
        pid: Pid,
    ) -> Result<Option<StopReason>, Error> {
        use nix::sys::ptrace;
        use nix::sys::wait::{WaitStatus, waitpid};

        ptrace::step(pid, None).map_err(Error::Ptrace)?;
        let status = waitpid(pid, None).map_err(Error::Waitpid)?;
        match status {
            WaitStatus::Stopped(_, sig) if sig == nix::sys::signal::SIGTRAP => Ok(None),
            WaitStatus::Stopped(p, sig) => Ok(Some(StopReason::SignalStop(p, sig))),
            WaitStatus::Exited(_, code) => Ok(Some(StopReason::DebugeeExit(code))),
            _ => Ok(None),
        }
    }
}

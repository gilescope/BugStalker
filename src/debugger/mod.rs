// SPDX-License-Identifier: MIT
pub mod address;
pub mod r#async;
mod breakpoint;
pub mod call;
mod code;
mod context;
#[cfg(target_os = "macos")]
pub mod darwin_mach;
mod debugee;
mod enc_checkpoint;
mod error;
pub(crate) mod platform_checkpoint;
pub mod process;
pub mod register;
pub mod rust;
mod step;
pub(crate) mod thread_db_compat;
mod utils;
pub mod variable;
pub mod viz;
mod watchpoint;

pub use breakpoint::BreakpointView;
pub use breakpoint::BreakpointViewOwned;
pub use breakpoint::CreateTransparentBreakpointRequest;
pub use debugee::FrameInfo;
pub use debugee::FunctionAssembly;
pub use debugee::FunctionRange;
pub use debugee::RegionInfo;
pub use debugee::ThreadSnapshot;
pub use debugee::dwarf::CandidateStatus as LineCandidateStatus;
pub use debugee::dwarf::InlineFrame;
pub use debugee::dwarf::LineCandidate;
pub use debugee::dwarf::LineDiagnostics;
pub use debugee::dwarf::Symbol;
pub use debugee::dwarf::r#type::ComplexType;
pub use debugee::dwarf::r#type::TypeDeclaration;
pub use debugee::dwarf::unit::FunctionInfo;
pub use debugee::dwarf::unit::PlaceDescriptor;
pub use debugee::dwarf::unit::PlaceDescriptorOwned;
/// Public unwind API backed by the internal DWARF unwinder (no libunwind feature gate).
pub use debugee::dwarf::unwind;
pub use debugee::tracee::Tracee;
pub use debugee::tracee::TraceeStatus;
pub use debugee::tracer::StopReason;
pub use error::Error;
pub use watchpoint::WatchpointView;
pub use watchpoint::WatchpointViewOwned;

use crate::debugger::Error::Syscall;
use crate::debugger::address::{Address, GlobalAddress, RelocatedAddress};
use crate::debugger::breakpoint::{Breakpoint, BreakpointRegistry, BrkptType, UninitBreakpoint};
use crate::debugger::debugee::dwarf::DwarfUnwinder;
use crate::debugger::debugee::dwarf::unwind::Backtrace;
use crate::debugger::debugee::tracer::TraceContext;
use crate::debugger::debugee::{Debugee, ExecutionStatus, Location};
use crate::debugger::error::Error::{
    FrameNotFound, Hook, ProcessNotStarted, Ptrace, RegisterNameNotFound, UnwindNoContext,
};
use crate::debugger::process::{Child, Installed};
use crate::debugger::register::debug::BreakCondition;
use crate::debugger::register::{DwarfRegisterMap, Register, RegisterMap};
use crate::debugger::step::StepResult;
use crate::debugger::variable::dqe::{Dqe, Selector};
use crate::debugger::variable::execute::QueryResult;
use crate::debugger::variable::value::Value;
use crate::debugger::watchpoint::WatchpointRegistry;
use crate::oracle::Oracle;
use crate::{print_warns, weak_error};
use indexmap::IndexMap;
use log::debug;
#[cfg(target_os = "linux")]
use nix::libc::c_void;
use nix::libc::uintptr_t;
#[cfg(target_os = "linux")]
use nix::sys;
use nix::sys::signal;
use nix::sys::signal::{SIGKILL, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
// `Object` is consumed for trait-method dispatch (e.g. `object.entry()`).
// On macOS the cfg-gated call sites can elide all uses of it; rather
// than litter call-site cfgs, allow the unused-import lint here.
#[allow(unused_imports)]
use object::Object;
use os_pipe::PipeWriter;
use regex::Regex;
#[cfg(target_os = "linux")]
use std::ffi::c_long;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::{fs, mem};

/// Trait for the reverse interaction between the debugger and the user interface.
pub trait EventHook {
    /// Called when user defined breakpoint is reached.
    ///
    /// # Arguments
    ///
    /// * `pc`: address of instruction where breakpoint is reached
    /// * `num`: breakpoint number
    /// * `place`: stop place information
    /// * `function`: function debug information entry
    /// * `thread_num`: number of in focus thread
    fn on_breakpoint(
        &self,
        pc: RelocatedAddress,
        num: u32,
        place: Option<PlaceDescriptor>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
    ) -> anyhow::Result<()>;

    /// Phase 9 follow-up — same as `on_breakpoint`, but the call
    /// site also provides the `addr2line`-computed inline chain
    /// (innermost first; `chain.last()` is the concrete enclosing
    /// subprogram). Default impl drops the chain and forwards to
    /// `on_breakpoint`, so existing hooks compile unchanged. Hooks
    /// that want the chain (the JSON-RPC ScriptHook) override this.
    fn on_breakpoint_with_chain(
        &self,
        pc: RelocatedAddress,
        num: u32,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
        _inline_chain: &[InlineFrame],
    ) -> anyhow::Result<()> {
        self.on_breakpoint(pc, num, place, function, thread_num)
    }

    /// Called when watchpoint is activated.
    ///
    /// # Arguments
    ///
    /// * `pc`: address of instruction where breakpoint is reached
    /// * `num`: breakpoint number
    /// * `place`: breakpoint number
    /// * `condition`: reason of a watchpoint activation
    /// * `dqe_string`: stringified data query expression (if exist)
    /// * `old_value`: previous expression or mem location value
    /// * `new_value`: current expression or mem location value
    /// * `end_of_scope`: true if watchpoint activated cause end of scope is reached
    #[allow(clippy::too_many_arguments)]
    fn on_watchpoint(
        &self,
        pc: RelocatedAddress,
        num: u32,
        place: Option<PlaceDescriptor>,
        condition: BreakCondition,
        dqe_string: Option<&str>,
        old_value: Option<&Value>,
        new_value: Option<&Value>,
        end_of_scope: bool,
    ) -> anyhow::Result<()>;

    /// Called when one of step commands is done.
    ///
    /// # Arguments
    ///
    /// * `pc`: address of instruction where breakpoint is reached
    /// * `place`: stop place information
    /// * `function`: function debug information entry
    /// * `thread_num`: number of in focus thread
    fn on_step(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
    ) -> anyhow::Result<()>;

    /// Step-event variant carrying the inline chain (same shape as
    /// `on_breakpoint_with_chain`). Default impl forwards to
    /// `on_step`.
    fn on_step_with_chain(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor<'_>>,
        function: Option<&FunctionInfo>,
        thread_num: Option<u32>,
        _inline_chain: &[InlineFrame],
    ) -> anyhow::Result<()> {
        self.on_step(pc, place, function, thread_num)
    }

    /// Called when one of async step commands is done.
    ///
    /// # Arguments
    ///
    /// * `pc`: address of instruction where breakpoint is reached
    /// * `place`: stop place information
    /// * `function`: function debug information entry
    /// * `task_id`: asynchronous task id
    /// * `task_completed`: true if task is already completed
    fn on_async_step(
        &self,
        pc: RelocatedAddress,
        place: Option<PlaceDescriptor>,
        function: Option<&FunctionInfo>,
        task_id: u64,
        task_completed: bool,
    ) -> anyhow::Result<()>;

    /// Called when debugee receive an OS signal. Debugee is in signal-stop at this moment.
    ///
    /// # Arguments
    ///
    /// * `signal`: received OS signal
    fn on_signal(&self, signal: Signal);

    /// Called right after debugee exit.
    ///
    /// # Arguments
    ///
    /// * `code`: exit code
    fn on_exit(&self, code: i32);

    /// Called single time for each debugee process (on start or after reinstall).
    ///
    /// # Arguments
    ///
    /// * `pid`: debugee process pid
    fn on_process_install(&self, pid: Pid, object: Option<&object::File>);
}

pub struct NopHook {}

impl EventHook for NopHook {
    fn on_breakpoint(
        &self,
        _: RelocatedAddress,
        _: u32,
        _: Option<PlaceDescriptor>,
        _: Option<&FunctionInfo>,
        _: Option<u32>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_watchpoint(
        &self,
        _: RelocatedAddress,
        _: u32,
        _: Option<PlaceDescriptor>,
        _: BreakCondition,
        _: Option<&str>,
        _: Option<&Value>,
        _: Option<&Value>,
        _: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_step(
        &self,
        _: RelocatedAddress,
        _: Option<PlaceDescriptor>,
        _: Option<&FunctionInfo>,
        _: Option<u32>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_async_step(
        &self,
        _: RelocatedAddress,
        _: Option<PlaceDescriptor>,
        _: Option<&FunctionInfo>,
        _: u64,
        _: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_signal(&self, _: Signal) {}

    fn on_exit(&self, _: i32) {}

    fn on_process_install(&self, _: Pid, _: Option<&object::File>) {}
}

#[macro_export]
macro_rules! disable_when_not_stared {
    ($this: expr) => {
        if !$this.debugee.is_in_progress() {
            return Err($crate::debugger::error::Error::ProcessNotStarted);
        }
    };
}

/// Exploration context (or ecx). Contains current explored thread and program counter.
/// May be changed by user (by `thread` or `frame` command)
/// or by debugger (at breakpoints, after steps, etc.).
#[derive(Clone, Debug)]
pub struct ExplorationContext {
    focus_location: Location,
    focus_frame: u32,
}

impl ExplorationContext {
    /// Create a new context with known thread but without known program counter-value.
    /// It is useful when debugee is not started yet or restarted.
    ///
    /// # Arguments
    ///
    /// * `pid`: thread id
    pub fn new_non_running(pid: Pid) -> ExplorationContext {
        Self {
            focus_location: Location {
                pc: 0_u64.into(),
                global_pc: 0_u64.into(),
                pid,
            },
            focus_frame: 0,
        }
    }

    /// Create new context.
    pub fn new(location: Location, frame_num: u32) -> Self {
        Self {
            focus_location: location,
            focus_frame: frame_num,
        }
    }

    #[inline(always)]
    pub fn location(&self) -> Location {
        self.focus_location
    }

    #[inline(always)]
    pub fn frame_num(&self) -> u32 {
        self.focus_frame
    }

    #[inline(always)]
    pub fn pid_on_focus(&self) -> Pid {
        self.location().pid
    }
}

/// Debugger structure builder.
#[derive(Default)]
pub struct DebuggerBuilder<H: EventHook + 'static = NopHook> {
    oracles: Vec<Arc<dyn Oracle>>,
    hooks: Option<H>,
    auto_traps: bool,
    force_restart: bool,
}

impl<H: EventHook + 'static> DebuggerBuilder<H> {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            oracles: vec![],
            hooks: None,
            auto_traps: true,
            force_restart: false,
        }
    }

    /// Enable or disable the panic / process-exit auto-trap
    /// breakpoints. Default: enabled. Test scenarios that drive
    /// the inferior to completion (and expect it to *exit*) want
    /// to disable these — otherwise the run stops at
    /// `std::process::exit` instead of letting the OS reap the
    /// process.
    pub fn with_auto_traps(self, auto_traps: bool) -> Self {
        Self { auto_traps, ..self }
    }

    /// Bypass the EnC restart safety check that refuses to
    /// auto-restart functions whose body contains outbound CALL/BL
    /// instructions. Default: false (refuse). Set to true if you
    /// know the function is safe to restart from entry — e.g. you
    /// have a Tier-2 fn-entry checkpoint to pair with the restart,
    /// or you're testing a specific code path and accept the
    /// possibility of garbage output. See
    /// [`crate::debugger::error::Error::RestartRefusedInnerCalls`]
    /// for the rationale.
    pub fn with_force_restart(self, force_restart: bool) -> Self {
        Self {
            force_restart,
            ..self
        }
    }

    /// Add oracles.
    ///
    /// # Arguments
    ///
    /// * `oracles`: list of oracles
    pub fn with_oracles(self, oracles: Vec<Arc<dyn Oracle>>) -> Self {
        Self { oracles, ..self }
    }

    /// Add event hooks implementation
    ///
    /// # Arguments
    ///
    /// * `hooks`: hooks implementation
    pub fn with_hooks(self, hooks: H) -> Self {
        Self {
            hooks: Some(hooks),
            ..self
        }
    }

    /// Return all oracles.
    pub fn oracles(&self) -> impl Iterator<Item = &dyn Oracle> {
        self.oracles.iter().map(|oracle| oracle.as_ref())
    }

    /// Create a debugger.
    ///
    /// # Arguments
    ///
    /// * `process`: debugee process
    pub fn build(self, process: Child<Installed>) -> Result<Debugger, Error> {
        if let Some(hooks) = self.hooks {
            Debugger::new(
                process,
                hooks,
                self.oracles,
                self.auto_traps,
                self.force_restart,
            )
        } else {
            Debugger::new(
                process,
                NopHook {},
                self.oracles,
                self.auto_traps,
                self.force_restart,
            )
        }
    }

    /// Create a debugger attached to a running process.
    ///
    /// # Arguments
    ///
    /// * `pid`: debugee process id
    /// * `stdout`: stdout pipe for future restarts
    /// * `stderr`: stderr pipe for future restarts
    pub fn build_attached(
        self,
        pid: Pid,
        stdout: PipeWriter,
        stderr: PipeWriter,
    ) -> Result<Debugger, Error> {
        let process = Child::from_external(pid, stdout, stderr)?;
        self.build(process)
    }
}

/// Main structure of bug-stalker, control debugee state and provides application functionality.
pub struct Debugger {
    /// Child process where debugee is running.
    process: Child<Installed>,
    /// Debugee static/runtime state and control flow.
    debugee: Debugee,
    /// Active and non-active breakpoints lists.
    breakpoints: BreakpointRegistry,
    /// Watchpoints lists.
    watchpoints: WatchpointRegistry,
    /// Debugger interrupt with UI by EventHook trait.
    hooks: Box<dyn EventHook>,
    /// Current exploration context.
    expl_context: ExplorationContext,
    /// Map of name -> (oracle, installed flag) pairs.
    oracles: IndexMap<&'static str, (Arc<dyn Oracle>, bool)>,
    /// Detach flag to skip destructive cleanup on drop.
    detached: bool,
    /// When false, the EntryPoint handler skips `install_auto_traps`.
    /// Test scenarios that drive the inferior to completion need to
    /// disable this so the run doesn't stop at `std::process::exit`.
    auto_traps: bool,
    /// When true, `restart_top_frame` skips the safety check that
    /// refuses functions with outbound CALL/BL instructions in their
    /// body. Default: false. Set via
    /// [`DebuggerBuilder::with_force_restart`]. See
    /// [`Error::RestartRefusedInnerCalls`] for the rationale.
    force_restart: bool,
    /// Phase 4 Tier-A — declarative visualiser registry,
    /// populated from the debuggee's `.bs_viz_spec` /
    /// `__bs_viz_spec` section at construction. Empty when the
    /// debuggee was built without `bs-viz-sdk` or compiled with
    /// `--release` (specs are debug-build artefacts by
    /// convention).
    viz: viz::VizRegistry,
    /// EnC restart Tier-2: per-function fn-entry snapshots
    /// captured by a hidden transparent breakpoint at the
    /// function's start IP. Restored by `restart_top_frame` when
    /// the function body contains outbound CALLs and the DWARF-
    /// only restore path can't reconstruct enough state. See
    /// [`enc_checkpoint`] for the full architecture.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    enc_checkpoints: enc_checkpoint::EncCheckpointStore,
}

impl Debugger {
    fn new(
        process: Child<Installed>,
        hooks: impl EventHook + 'static,
        oracles: impl IntoIterator<Item = Arc<dyn Oracle>>,
        auto_traps: bool,
        force_restart: bool,
    ) -> Result<Self, Error> {
        let program_path = Path::new(process.program());

        let file = fs::File::open(program_path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let object = object::File::parse(&*mmap)?;

        // `object.entry()`:
        //   linux ELF: returns a virtual address (PIE: an RVA;
        //     non-PIE: a fixed VA). The downstream `GlobalAddress`
        //     + `mapping_offset` flow expects this RVA-style value.
        //   darwin Mach-O: returns the LC_MAIN `entryoff` — a
        //     __TEXT-relative offset (e.g. `0x9F8` for hello_world,
        //     not `0x1000009F8`). Add `__TEXT.vmaddr` to convert
        //     into a runtime VA assuming the default load base,
        //     matching the convention DWARF line tables use on
        //     Mach-O. The shared `mapping_offset = slide` then
        //     applies uniformly across DWARF and entry.
        #[cfg(target_os = "linux")]
        let entry_point = GlobalAddress::from(object.entry());
        #[cfg(not(target_os = "linux"))]
        let entry_point = {
            use object::{Object, ObjectSegment};
            let text_vmaddr = object
                .segments()
                .find(|s| s.name().ok().flatten() == Some("__TEXT"))
                .map(|s| s.address())
                .unwrap_or(0);
            GlobalAddress::from(object.entry() + text_vmaddr)
        };
        let mut breakpoints = BreakpointRegistry::default();
        breakpoints.add_uninit(UninitBreakpoint::new_entry_point(
            None::<PathBuf>,
            Address::Global(entry_point),
            process.pid(),
        ));

        let process_id = process.pid();
        hooks.on_process_install(process_id, Some(&object));

        let debugee = if process.is_external() {
            Debugee::new_from_external_process(program_path, &process, &object)?
        } else {
            Debugee::new_non_running(program_path, &process, &object)?
        };

        // Phase 4 Tier-A: scan visualiser specs out of the
        // executable. Done here, while the `object::File` is
        // still alive and we don't have to re-parse it later.
        let viz = viz::VizRegistry::from_object(&object);
        if !viz.is_empty() {
            log::debug!(
                target: "viz",
                "loaded {} #[derive(DebugView)] spec(s) from {}",
                viz.len(),
                program_path.display(),
            );
        }

        Ok(Self {
            debugee,
            process,
            breakpoints,
            watchpoints: WatchpointRegistry::default(),
            hooks: Box::new(hooks),
            expl_context: ExplorationContext::new_non_running(process_id),
            oracles: oracles
                .into_iter()
                .map(|oracle| (oracle.name(), (oracle, false)))
                .collect(),
            detached: false,
            auto_traps,
            force_restart,
            viz,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            enc_checkpoints: enc_checkpoint::EncCheckpointStore::default(),
        })
    }

    /// Phase 4 Tier-A — return the registered
    /// [`bs_viz_spec::TypeViewSpec`] for `type_name`, if any.
    /// `type_name` should be the fully-qualified rustc/v0
    /// demangled form (e.g. `my_crate::Person`); the registry
    /// also matches against the local-only name the proc-macro
    /// currently emits, so callers don't have to pre-strip the
    /// module path.
    pub fn view_spec_for(&self, type_name: &str) -> Option<&bs_viz_spec::TypeViewSpec> {
        self.viz.find(type_name)
    }

    /// Total number of visualiser specs loaded from the debuggee.
    /// Useful for tests + the eventual `bs/visualiserList` DAP
    /// request.
    pub fn view_spec_count(&self) -> usize {
        self.viz.len()
    }

    /// Borrow the full visualiser registry. Render-layer callers
    /// (DAP `variables` response, TUI rendering pipeline) thread
    /// this through so registered types render via their
    /// declarative spec.
    pub fn view_registry(&self) -> &viz::VizRegistry {
        &self.viz
    }

    /// Disable every currently-enabled breakpoint and return the
    /// list of their runtime addresses so the caller can restore
    /// them later via [`enable_breakpoints_at`]. Used by the
    /// edit-and-continue flow: while wild's patch overwrites text
    /// bytes, any `INT3` (0xCC) bytes the debugger has injected
    /// would cause the patch's pre-image drift check to fail —
    /// we drop them, apply the patch against the clean original
    /// bytes, then re-arm.
    ///
    /// Idempotent — calling twice with no intervening
    /// [`enable_breakpoints_at`] returns an empty list the second
    /// time.
    pub fn disable_all_breakpoints(&self) -> Vec<RelocatedAddress> {
        let mut addrs = Vec::new();
        for bp in self.breakpoints.active_breakpoints() {
            if bp.is_enabled() {
                addrs.push(bp.addr);
                let _ = bp.disable();
            }
        }
        addrs
    }

    /// Re-enable breakpoints at each of the given runtime
    /// addresses. Addresses not present in the registry are
    /// silently skipped (a breakpoint may have been removed
    /// between the disable + re-enable). Errors on a single bp's
    /// `enable()` are logged but don't abort the whole batch —
    /// best-effort restore so a partial failure doesn't leave
    /// the user with no breakpoints at all.
    pub fn enable_breakpoints_at(&self, addrs: &[RelocatedAddress]) {
        for bp in self.breakpoints.active_breakpoints() {
            if addrs.contains(&bp.addr)
                && !bp.is_enabled()
                && let Err(e) = bp.enable()
            {
                log::warn!(
                    target: "breakpoint",
                    "failed to re-enable breakpoint at {}: {e}",
                    bp.addr,
                );
            }
        }
    }

    /// Return installed oracle, or `None` if oracle not found or not installed.
    ///
    /// # Arguments
    ///
    /// * `name`: oracle name
    pub fn get_oracle(&self, name: &str) -> Option<&dyn Oracle> {
        self.oracles
            .get(name)
            .and_then(|(oracle, install)| install.then_some(oracle.as_ref()))
    }

    /// Same as `get_oracle` but return an `Arc<dyn Oracle>`
    pub fn get_oracle_arc(&self, name: &str) -> Option<Arc<dyn Oracle>> {
        self.oracles
            .get(name)
            .and_then(|(oracle, install)| install.then_some(oracle.clone()))
    }

    /// Return all oracles.
    pub fn all_oracles(&self) -> impl Iterator<Item = &dyn Oracle> {
        self.oracles.values().map(|(oracle, _)| oracle.as_ref())
    }

    /// Same as `all_oracles` but return iterator over `Arc<dyn Oracle>`
    pub fn all_oracles_arc(&self) -> impl Iterator<Item = Arc<dyn Oracle>> + '_ {
        self.oracles.values().map(|(oracle, _)| oracle.clone())
    }

    pub fn process(&self) -> &Child<Installed> {
        &self.process
    }

    pub(crate) fn debugee(&self) -> &Debugee {
        &self.debugee
    }

    pub fn detach(&mut self) -> Result<(), Error> {
        if self.detached {
            return Ok(());
        }

        _ = self.breakpoints.disable_all_breakpoints(&self.debugee);
        self.watchpoints
            .clear_all(self.debugee.tracee_ctl(), &mut self.breakpoints);

        let current_tids: Vec<Pid> = self
            .debugee
            .tracee_ctl()
            .tracee_iter()
            .map(|t| t.pid)
            .collect();

        if !current_tids.is_empty() {
            #[cfg(target_os = "linux")]
            {
                current_tids
                    .iter()
                    .try_for_each(|tid| sys::ptrace::detach(*tid, None).map_err(Ptrace))?;

                signal::kill(self.debugee.tracee_ctl().proc_pid(), Signal::SIGCONT)
                    .map_err(|e| Syscall("kill", e))?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                // Darwin: no ptrace relationship to detach. Drop
                // the Mach suspend count so the inferior can run
                // free from us.
                if let Ok(task) = darwin_mach::task_for_pid(self.debugee.tracee_ctl().proc_pid()) {
                    let _ = darwin_mach::task_resume(task);
                }
            }
        }

        self.detached = true;
        Ok(())
    }

    pub fn set_hook(&mut self, hooks: impl EventHook + 'static) {
        self.hooks = Box::new(hooks);
    }

    /// Return last set exploration context.
    #[inline(always)]
    pub fn ecx(&self) -> &ExplorationContext {
        &self.expl_context
    }

    /// Update current program counters for current in focus thread.
    fn ecx_update_location(&mut self) -> Result<&ExplorationContext, Error> {
        let old_ecx = self.ecx();
        self.expl_context = ExplorationContext::new(
            self.debugee
                .get_tracee_ensure(old_ecx.pid_on_focus())
                .location(&self.debugee)?,
            0,
        );
        Ok(&self.expl_context)
    }

    fn ecx_swap(&mut self, new: ExplorationContext) {
        self.expl_context = new;
    }

    /// Restore frame from user defined to real.
    fn ecx_restore_frame(&mut self) -> Result<&ExplorationContext, Error> {
        self.ecx_update_location()
    }

    /// Change in focus thread and update program counters.
    ///
    /// # Arguments
    ///
    /// * `pid`: new in focus thread id
    fn ecx_switch_thread(&mut self, pid: Pid) -> Result<&ExplorationContext, Error> {
        self.expl_context = ExplorationContext::new(
            self.debugee
                .get_tracee_ensure(pid)
                .location(&self.debugee)?,
            0,
        );
        Ok(&self.expl_context)
    }

    /// Continue debugee execution. Step over breakpoint if called at it.
    /// Return if breakpoint is reached or signal occurred or debugee exit.
    ///
    /// **! change exploration context**
    fn continue_execution(&mut self) -> Result<StopReason, Error> {
        if let Some(sign_or_wp) = self.step_over_breakpoint()? {
            match sign_or_wp {
                StopReason::Watchpoint(pid, current_pc, ty) => {
                    self.execute_on_watchpoint_hook(pid, current_pc, &ty)?;
                    return Ok(StopReason::Watchpoint(pid, current_pc, ty));
                }
                StopReason::SignalStop(pid, sign) => {
                    self.hooks.on_signal(sign);
                    return Ok(StopReason::SignalStop(pid, sign));
                }
                _ => {
                    unreachable!("unexpected reason")
                }
            }
        }

        let stop_reason = loop {
            let event = self.debugee.trace_until_stop(TraceContext::new(
                &self.breakpoints.active_breakpoints(),
                &self.watchpoints,
            ))?;
            match event {
                StopReason::DebugeeExit(code) => {
                    // ignore all possible errors on watchpoints disabling
                    _ = self.watchpoints.clear_local_disable_global(
                        self.debugee.tracee_ctl(),
                        &mut self.breakpoints,
                    );
                    // ignore all possible errors on breakpoints disabling
                    _ = self.breakpoints.disable_all_breakpoints(&self.debugee);
                    self.hooks.on_exit(code);
                    break event;
                }
                StopReason::DebugeeStart => {
                    self.breakpoints.enable_entry_breakpoint(&self.debugee)?;
                    // no need to update expl context cause next stop been soon, on entry point
                }
                StopReason::NoSuchProcess(_) => {
                    return Err(ProcessNotStarted);
                }
                StopReason::Breakpoint(pid, current_pc) => {
                    self.ecx_switch_thread(pid)?;

                    if let Some(bp) = self.breakpoints.get_enabled(current_pc) {
                        match bp.r#type() {
                            BrkptType::EntryPoint => {
                                print_warns!(
                                    self.breakpoints.enable_all_breakpoints(&self.debugee)
                                );
                                print_warns!(self.watchpoints.refresh(&self.debugee));

                                // rendezvous already available at this point
                                let brk = self.debugee.rendezvous().r_brk();
                                self.breakpoints.add_and_enable(Breakpoint::new_linker_map(
                                    brk,
                                    self.process.pid(),
                                ))?;

                                // check oracles is ready
                                let oracles = self.oracles.clone();
                                self.oracles = oracles.into_iter().map(|(key, (oracle, _))| {
                                    let ready = oracle.ready_for_install(self);
                                    if !ready {
                                        debug!(target: "oracle", "oracle `{}` is disabled", oracle.name());
                                    }

                                    (key, (oracle, ready))
                                }).collect();

                                let oracles = self.oracles.clone();
                                let ready_oracles = oracles.into_values().filter(|(_, a)| *a);
                                for (oracle, _) in ready_oracles {
                                    let spy_points = oracle.spy_points();
                                    for request in spy_points {
                                        weak_error!(self.set_transparent_breakpoint(request));
                                    }
                                }

                                // Auto-traps: stop the debuggee one last
                                // time before a panic unwinds away the
                                // stack, and again just before the
                                // process exits — that's the user's
                                // chance to inspect locals + backtrace
                                // before the world goes away. Symbols
                                // that aren't present in this binary
                                // (e.g. `_exit` in a no_std build) get
                                // skipped silently.
                                //
                                // Opt-out via `DebuggerBuilder::with_auto_traps(false)`:
                                // some scenarios (notably the test
                                // suite's "run to completion" tests)
                                // need the inferior to actually exit
                                // rather than stop at `process::exit`.
                                if self.auto_traps {
                                    self.install_auto_traps();
                                }

                                // ignore possible signals and watchpoints
                                while self.step_over_breakpoint()?.is_some() {}
                                continue;
                            }
                            BrkptType::LinkerMapFn => {
                                // ignore possible signals and watchpoints
                                while self.step_over_breakpoint()?.is_some() {}
                                print_warns!(self.refresh_deferred());
                                continue;
                            }
                            BrkptType::UserDefined => {
                                let pc = current_pc.into_global(&self.debugee)?;
                                let dwarf = self.debugee.debug_info(self.ecx().location().pc)?;
                                let place = weak_error!(dwarf.find_place_from_pc(pc)).flatten();
                                let func = weak_error!(dwarf.find_function_by_pc(pc))
                                    .flatten()
                                    .map(|(_, info)| info);
                                let tracee_ctl = self.debugee.tracee_ctl();
                                let tracee_in_focus = tracee_ctl
                                    .tracee(self.ecx().pid_on_focus())
                                    .map(|t| t.number);
                                let inline_chain = current_pc
                                    .into_global(&self.debugee)
                                    .ok()
                                    .and_then(|gpc| {
                                        self.debugee
                                            .debug_info(current_pc)
                                            .ok()
                                            .map(|d| d.find_inline_chain(gpc))
                                    })
                                    .unwrap_or_default();
                                self.hooks
                                    .on_breakpoint_with_chain(
                                        current_pc,
                                        bp.number(),
                                        place,
                                        func,
                                        tracee_in_focus,
                                        &inline_chain,
                                    )
                                    .map_err(Hook)?;
                                break event;
                            }
                            BrkptType::WatchpointCompanion(_) => {
                                unreachable!("should not coming from tracer directly");
                            }
                            BrkptType::Temporary | BrkptType::TemporaryAsync => {
                                break event;
                            }
                            BrkptType::Transparent(callback) => {
                                callback.clone()(self);

                                match self.step_over_breakpoint()? {
                                    Some(StopReason::SignalStop(pid, sign)) => {
                                        self.hooks.on_signal(sign);
                                        return Ok(StopReason::SignalStop(pid, sign));
                                    }
                                    Some(StopReason::Watchpoint(pid, addr, ty)) => {
                                        self.execute_on_watchpoint_hook(pid, addr, &ty)?;
                                        return Ok(StopReason::Watchpoint(pid, current_pc, ty));
                                    }
                                    _ => continue,
                                }
                            }
                        }
                    }
                }
                StopReason::SignalStop(pid, sign) => {
                    if !self.debugee.is_in_progress() {
                        continue;
                    }

                    self.ecx_switch_thread(pid)?;
                    self.hooks.on_signal(sign);
                    break event;
                }
                StopReason::Watchpoint(pid, current_pc, ref ty) => {
                    self.ecx_switch_thread(pid)?;
                    self.execute_on_watchpoint_hook(pid, current_pc, ty)?;
                    break event;
                }
            }
        };

        Ok(stop_reason)
    }

    /// Darwin-only: clear our Mach exception subscription, reply to any
    /// pending Mach exception (so the inferior advances past whatever
    /// trapped it), and detach the ptrace half that `PT_ATTACHEXC` put
    /// us in. Without this, a subsequent `kill(SIGKILL)` is queued in
    /// the kernel's signal layer but never delivered because the
    /// inferior is still ptrace-stopped on its last exception. Used by
    /// both `restart_debugee` and `Drop` to make `kill(SIGKILL)` actually
    /// land. Idempotent and tolerant of a vanished inferior (ESRCH).
    #[cfg(not(target_os = "linux"))]
    fn darwin_release_inferior_for_kill(&self) {
        use crate::debugger::darwin_mach::ExceptionPort;
        use mach2::exception_types::{EXC_MASK_BAD_ACCESS, EXC_MASK_BREAKPOINT, EXC_MASK_SOFTWARE};
        use mach2::kern_return::KERN_FAILURE;
        use mach2::port::MACH_PORT_NULL;
        use mach2::thread_status::THREAD_STATE_NONE;
        let pid = self.debugee.tracee_ctl().proc_pid();
        if let Ok(task) = darwin_mach::task_for_pid(pid) {
            let mask = EXC_MASK_BREAKPOINT | EXC_MASK_SOFTWARE | EXC_MASK_BAD_ACCESS;
            // SAFETY: task is a valid task port; MACH_PORT_NULL clears
            // the subscription so post-teardown BRK / SEGV falls
            // through to the BSD default handler.
            unsafe {
                mach2::task::task_set_exception_ports(
                    task,
                    mask,
                    MACH_PORT_NULL,
                    0,
                    THREAD_STATE_NONE,
                );
            }
        }
        if let Some(state) = self.debugee.tracer().darwin_state()
            && let Some((remote, id, _retcode)) = state.take_pending_reply()
        {
            let _ = ExceptionPort::reply(remote, id, KERN_FAILURE);
        }
        // SAFETY: ptrace(PT_DETACH, pid, 0, 0) — addr ignored, data is
        // the signal to inject (0 = none). ESRCH if the inferior died
        // first; we don't care.
        unsafe {
            libc::ptrace(libc::PT_DETACH, pid.as_raw(), std::ptr::null_mut(), 0);
        }
        if let Ok(task) = darwin_mach::task_for_pid(pid) {
            let _ = darwin_mach::task_resume(task);
        }
    }

    /// Restart debugee by recreating debugee process, save all user-defined breakpoints.
    /// Return when new debugee stopped or ends.
    ///
    /// **! change exploration context**
    pub fn restart_debugee(&mut self) -> Result<Pid, Error> {
        match self.debugee.execution_status() {
            ExecutionStatus::Unload => {
                // all breakpoints and watchpoints already disabled by default
            }
            ExecutionStatus::InProgress => {
                print_warns!(
                    self.watchpoints.clear_local_disable_global(
                        self.debugee.tracee_ctl(),
                        &mut self.breakpoints
                    )
                );
                print_warns!(self.breakpoints.disable_all_breakpoints(&self.debugee)?);
            }
            ExecutionStatus::Exited => {
                // all breakpoints and watchpoints
                // already disabled by [`StopReason::DebugeeExit`] handler
            }
        }

        if !self.debugee.is_exited() {
            let proc_pid = self.process.pid();
            // Darwin: with PT_ATTACHEXC active, the inferior is
            // ptrace-stopped on its last Mach exception. SIGKILL would
            // queue but not deliver until ptrace is released, so do
            // the same teardown dance as Drop before kill.
            #[cfg(not(target_os = "linux"))]
            self.darwin_release_inferior_for_kill();
            signal::kill(proc_pid, SIGKILL).map_err(|e| Syscall("kill", e))?;
            _ = self
                .debugee
                .tracer_mut()
                .resume(TraceContext::new(&[], &self.watchpoints));
            // Reap the now-dead inferior so its pid frees up before
            // we ask the kernel to spawn the next one. On linux this
            // is implicit via the ptrace state machine; on darwin the
            // process otherwise lingers as a zombie until the polling
            // `assert_no_proc!` times out. Poll with `WNOHANG` so we
            // don't block forever if the kernel never delivers the
            // terminal event (mirrors the `Drop` teardown).
            #[cfg(not(target_os = "linux"))]
            {
                use nix::sys::wait::WaitPidFlag;
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
                while std::time::Instant::now() < deadline {
                    match waitpid(proc_pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Ok(WaitStatus::Signaled(_, _, _))
                        | Ok(WaitStatus::Exited(_, _))
                        | Err(_) => break,
                        Ok(_) => {
                            let _ = signal::kill(proc_pid, Signal::SIGCONT);
                            let _ = signal::kill(proc_pid, Signal::SIGKILL);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                    }
                }
            }
        }

        self.process = self.process.install()?;

        let new_debugee = self.debugee.extend(self.process.pid());
        _ = mem::replace(&mut self.debugee, new_debugee);

        // breakpoints will be enabled later, when StopReason::DebugeeStart state is reached
        self.breakpoints.update_pid(self.process.pid());

        self.hooks.on_process_install(self.process.pid(), None);
        self.expl_context = ExplorationContext::new_non_running(self.process.pid());
        self.continue_execution()?;
        Ok(self.process.pid())
    }

    fn start_debugee_inner(&mut self, force: bool, dry_start: bool) -> Result<(), Error> {
        if dry_start {
            if (self.debugee.is_in_progress() || self.debugee.is_exited()) && !force {
                return Err(Error::AlreadyRun);
            }
            return Ok(());
        }

        match self.debugee.execution_status() {
            ExecutionStatus::Unload => {
                self.continue_execution()?;
            }
            ExecutionStatus::InProgress | ExecutionStatus::Exited if force => {
                self.restart_debugee()?;
            }
            ExecutionStatus::InProgress | ExecutionStatus::Exited => return Err(Error::AlreadyRun),
        };

        Ok(())
    }

    /// Start and execute debugee.
    /// Return when debugee stopped or ends.
    ///
    /// # Errors
    ///
    /// Return error if debugee already run or execution fails.
    pub fn start_debugee(&mut self) -> Result<(), Error> {
        self.start_debugee_inner(false, false)
    }

    /// Start and execute debugee, returning a structured stop reason.
    ///
    /// This API is primarily intended for protocol adapters (e.g. DAP), where the UI needs
    /// a machine-readable reason why the debugee stopped.
    pub fn start_debugee_with_reason(&mut self) -> Result<StopReason, Error> {
        // Reuse existing validation logic and then return the underlying stop reason.
        match self.debugee.execution_status() {
            ExecutionStatus::Unload => self.continue_execution(),
            ExecutionStatus::InProgress | ExecutionStatus::Exited => Err(Error::AlreadyRun),
        }
    }

    /// Start and execute debugee. Restart if debugee already started.
    /// Return when debugee stopped or ends.
    pub fn start_debugee_force(&mut self) -> Result<(), Error> {
        self.start_debugee_inner(true, false)
    }

    /// Start and execute debugee (restart if already started), returning a structured stop reason.
    pub fn start_debugee_force_with_reason(&mut self) -> Result<StopReason, Error> {
        match self.debugee.execution_status() {
            ExecutionStatus::Unload => self.continue_execution(),
            ExecutionStatus::InProgress | ExecutionStatus::Exited => {
                self.restart_debugee()?;
                // restart_debugee itself continues execution until the next stop.
                // If it returns successfully, we are already stopped; map this to a synthetic reason.
                Ok(StopReason::DebugeeStart)
            }
        }
    }

    /// Dry start debugee. Return immediately.
    ///
    /// # Errors
    ///
    /// Return error if debugee already runs.
    pub fn dry_start_debugee(&mut self) -> Result<(), Error> {
        self.start_debugee_inner(false, true)
    }

    /// Continue debugee execution.
    pub fn continue_debugee(&mut self) -> Result<(), Error> {
        disable_when_not_stared!(self);
        self.continue_execution()?;
        Ok(())
    }

    /// Continue debugee execution and return a structured stop reason.
    pub fn continue_debugee_with_reason(&mut self) -> Result<StopReason, Error> {
        disable_when_not_stared!(self);
        self.continue_execution()
    }

    /// Interrupt (pause) execution of the whole debugee process.
    ///
    /// This is used by non-interactive frontends (e.g. DAP) to implement the `pause` request.
    pub fn pause_debugee(&mut self) -> Result<(), Error> {
        let active_bps = self.breakpoints.active_breakpoints();
        self.debugee
            .pause(TraceContext::new(&active_bps, &self.watchpoints))
    }

    /// Return list of symbols matching regular expression.
    ///
    /// # Arguments
    ///
    /// * `regex`: regular expression
    pub fn get_symbols(&'_ self, regex: &str) -> Result<Vec<Symbol<'_>>, Error> {
        let regex = Regex::new(regex)?;

        Ok(self
            .debugee
            .debug_info_all()
            .iter()
            .flat_map(|dwarf| dwarf.find_symbols(&regex))
            .collect())
    }

    /// Return in focus frame information.
    pub fn frame_info(&self) -> Result<FrameInfo, Error> {
        disable_when_not_stared!(self);
        self.debugee.frame_info(self.ecx())
    }

    /// Set new frame into focus.
    ///
    /// # Arguments
    ///
    /// * `num`: frame number in backtrace
    pub fn set_frame_into_focus(&mut self, num: u32) -> Result<u32, Error> {
        disable_when_not_stared!(self);
        let ecx = self.ecx();
        let backtrace = self.debugee.unwind(ecx.pid_on_focus())?;
        let frame = backtrace.get(num as usize).ok_or(FrameNotFound(num))?;
        self.expl_context = ExplorationContext::new(
            Location {
                pc: frame.ip,
                global_pc: frame.ip.into_global(&self.debugee)?,
                pid: ecx.pid_on_focus(),
            },
            num,
        );
        Ok(num)
    }

    /// Execute `on_step` callback with current exploration context
    fn execute_on_step_hook(&self) -> Result<(), Error> {
        let ecx = self.ecx();
        let pc = ecx.location().pc;
        let global_pc = ecx.location().global_pc;
        let dwarf = self.debugee.debug_info(pc)?;
        let place = weak_error!(dwarf.find_place_from_pc(global_pc)).flatten();
        let func = weak_error!(dwarf.find_function_by_pc(global_pc))
            .flatten()
            .map(|(_, info)| info);
        let inline_chain = dwarf.find_inline_chain(global_pc);
        let tracee_ctl = self.debugee.tracee_ctl();
        let thread_in_focus = tracee_ctl.tracee(ecx.pid_on_focus()).map(|t| t.number);

        self.hooks
            .on_step_with_chain(pc, place, func, thread_in_focus, &inline_chain)
            .map_err(Hook)
    }

    /// Execute `on_async_step` callback with current exploration context
    fn execute_on_async_step_hook(&self, task_id: u64, task_completed: bool) -> Result<(), Error> {
        let ecx = self.ecx();
        let pc = ecx.location().pc;
        let global_pc = ecx.location().global_pc;
        let dwarf = self.debugee.debug_info(pc)?;
        let place = weak_error!(dwarf.find_place_from_pc(global_pc)).flatten();
        let func = weak_error!(dwarf.find_function_by_pc(global_pc))
            .flatten()
            .map(|(_, info)| info);
        self.hooks
            .on_async_step(pc, place, func, task_id, task_completed)
            .map_err(Hook)
    }

    /// Do a single step (until debugee reaches a different source line).
    ///
    /// **! change exploration context**
    pub fn step_into(&mut self) -> Result<(), Error> {
        disable_when_not_stared!(self);
        self.ecx_restore_frame()?;

        match self.step_in()? {
            StepResult::Done => self.execute_on_step_hook(),
            StepResult::SignalInterrupt { signal, quiet } if !quiet => {
                self.hooks.on_signal(signal);
                Ok(())
            }
            StepResult::WatchpointInterrupt {
                pid,
                addr,
                ref ty,
                quiet,
            } if !quiet => self.execute_on_watchpoint_hook(pid, addr, ty),
            _ => Ok(()),
        }
    }

    /// Move in focus thread to the next instruction.
    ///
    /// **! change exploration context**
    pub fn stepi(&mut self) -> Result<(), Error> {
        disable_when_not_stared!(self);
        self.ecx_restore_frame()?;

        match self.single_step_instruction()? {
            Some(StopReason::SignalStop(_, sign)) => {
                self.hooks.on_signal(sign);
                Ok(())
            }
            Some(StopReason::Watchpoint(pid, addr, ref ty)) => {
                self.execute_on_watchpoint_hook(pid, addr, ty)
            }
            _ => self.execute_on_step_hook(),
        }
    }

    /// Return list of currently running debugee threads.
    pub fn thread_state(&self) -> Result<Vec<ThreadSnapshot>, Error> {
        disable_when_not_stared!(self);
        self.debugee.thread_state(self.ecx())
    }

    /// Return IDs of currently attached debugee threads without unwinding them.
    pub fn thread_tids(&self) -> Result<Vec<Pid>, Error> {
        disable_when_not_stared!(self);
        Ok(self
            .debugee
            .tracee_ctl()
            .snapshot()
            .into_iter()
            .map(|tracee| tracee.pid)
            .collect())
    }

    /// Sets the thread into focus.
    ///
    /// # Arguments
    ///
    /// * `num`: thread number
    pub fn set_thread_into_focus(&mut self, num: u32) -> Result<Tracee, Error> {
        disable_when_not_stared!(self);
        let tracee = self.debugee.get_tracee_by_num(num)?;
        self.ecx_switch_thread(tracee.pid)?;
        Ok(tracee)
    }

    /// Return stack trace.
    ///
    /// # Arguments
    ///
    /// * `pid`: thread id
    pub fn backtrace(&self, pid: Pid) -> Result<Backtrace, Error> {
        disable_when_not_stared!(self);
        self.debugee.unwind(pid)
    }

    /// Read N bytes from a debugee process.
    ///
    /// # Arguments
    ///
    /// * `addr`: address in debugee address space where reads
    /// * `read_n`: read byte count
    pub fn read_memory(&self, addr: usize, read_n: usize) -> Result<Vec<u8>, Error> {
        disable_when_not_stared!(self);
        read_memory_by_pid(self.debugee.tracee_ctl().proc_pid(), addr, read_n).map_err(Ptrace)
    }

    /// Write sizeof(uintptr_t) bytes in debugee address space.
    /// Note that little endian byte order will be used when writing.
    ///
    /// # Arguments
    ///
    /// * `addr`: address to write
    /// * `value`: value to write
    #[cfg(target_os = "linux")]
    pub fn write_memory(&self, addr: uintptr_t, value: uintptr_t) -> Result<(), Error> {
        disable_when_not_stared!(self);
        unsafe {
            sys::ptrace::write(
                self.debugee.tracee_ctl().proc_pid(),
                addr as *mut c_void,
                value as *mut c_void,
            )
            .map_err(Ptrace)
        }
    }

    /// Darwin path: `mach_vm_write` framed by `mach_vm_protect` so
    /// read-only pages (typically `r-x` for code) are temporarily
    /// writable. The `value` is `usize`-sized — the caller composes
    /// breakpoint opcodes / restored bytes into a usize first, same
    /// shape as the linux `PTRACE_POKEDATA` path above.
    #[cfg(target_os = "macos")]
    pub fn write_memory(&self, addr: uintptr_t, value: uintptr_t) -> Result<(), Error> {
        disable_when_not_stared!(self);
        let task = darwin_mach::task_for_pid(self.debugee.tracee_ctl().proc_pid())?;
        darwin_mach::vm_write_word(task, addr, value)
    }

    /// Move to higher stack frame.
    pub fn step_out(&mut self) -> Result<(), Error> {
        disable_when_not_stared!(self);
        self.ecx_restore_frame()?;
        self.step_out_frame()?;
        self.execute_on_step_hook()
    }

    /// Do debugee step (over subroutine calls to).
    pub fn step_over(&mut self) -> Result<(), Error> {
        disable_when_not_stared!(self);
        self.ecx_restore_frame()?;
        match self.step_over_any()? {
            StepResult::Done => self.execute_on_step_hook(),
            StepResult::SignalInterrupt { signal, quiet } if !quiet => {
                self.hooks.on_signal(signal);
                Ok(())
            }
            StepResult::WatchpointInterrupt {
                pid,
                addr,
                ref ty,
                quiet,
            } if !quiet => self.execute_on_watchpoint_hook(pid, addr, ty),
            _ => Ok(()),
        }
    }

    /// Reads all local variables from current function in current thread.
    pub fn read_local_variables(&self) -> Result<Vec<QueryResult<'_>>, Error> {
        disable_when_not_stared!(self);

        let executor = variable::execute::DqeExecutor::new(self);
        let eval_result = executor.query(&Dqe::Variable(Selector::Any))?;
        Ok(eval_result)
    }

    /// Reads any variable from the current thread, uses a select expression to filter variables
    /// and fetch their properties (such as structure fields or array elements).
    ///
    /// # Arguments
    ///
    /// * `select_expr`: data query expression
    pub fn read_variable(&self, select_expr: Dqe) -> Result<Vec<QueryResult<'_>>, Error> {
        disable_when_not_stared!(self);
        let executor = variable::execute::DqeExecutor::new(self);
        let eval_result = executor.query(&select_expr)?;
        Ok(eval_result)
    }

    ///  Reads any variable from the current thread, uses a select expression to filter variables
    /// and return their names.
    ///
    /// # Arguments
    ///
    /// * `select_expr`: data query expression
    pub fn read_variable_names(&self, select_expr: Dqe) -> Result<Vec<String>, Error> {
        disable_when_not_stared!(self);
        let executor = variable::execute::DqeExecutor::new(self);
        executor.query_names(&select_expr)
    }

    /// Reads any argument from the current function, uses a select expression to filter variables
    /// and fetch their properties (such as structure fields or array elements).
    ///
    /// # Arguments
    ///
    /// * `select_expr`: data query expression
    pub fn read_argument(&self, select_expr: Dqe) -> Result<Vec<QueryResult<'_>>, Error> {
        disable_when_not_stared!(self);
        let executor = variable::execute::DqeExecutor::new(self);
        let eval_result = executor.query_arguments(&select_expr)?;
        Ok(eval_result)
    }

    /// Reads any argument from the current function, uses a select expression to filter arguments
    /// and return their names.
    ///
    /// # Arguments
    ///
    /// * `select_expr`: data query expression
    pub fn read_argument_names(&self, select_expr: Dqe) -> Result<Vec<String>, Error> {
        disable_when_not_stared!(self);
        let executor = variable::execute::DqeExecutor::new(self);
        executor.query_arguments_names(&select_expr)
    }

    /// Return following register value.
    ///
    /// # Arguments
    ///
    /// * `register_name`: target-architecture register name
    ///   (e.g. `rip` on x86_64, `pc` on aarch64)
    pub fn get_register_value(&self, register_name: &str) -> Result<u64, Error> {
        disable_when_not_stared!(self);

        let r = Register::from_str(register_name)
            .map_err(|_| RegisterNameNotFound(register_name.into()))?;
        Ok(RegisterMap::current(self.ecx().pid_on_focus())?.value(r))
    }

    /// Return registers dump for on focus thread at instruction defined by pc.
    ///
    /// # Arguments
    ///
    /// * `pc`: program counter value
    pub fn current_thread_registers_at_pc(
        &self,
        pc: RelocatedAddress,
    ) -> Result<DwarfRegisterMap, Error> {
        disable_when_not_stared!(self);
        let unwinder = DwarfUnwinder::new(&self.debugee);
        let location = Location {
            pc,
            global_pc: pc.into_global(&self.debugee)?,
            pid: self.ecx().pid_on_focus(),
        };
        Ok(unwinder
            // there is no chance to determine frame number,
            // cause pc may have owned by code outside backtrace,
            // so set frame num to 0 is ok
            .context_for(&ExplorationContext::new(location, 0))?
            .ok_or(UnwindNoContext)?
            .registers())
    }

    /// Set new register value.
    ///
    /// # Arguments
    ///
    /// * `register_name`: target-architecture register name
    ///   (e.g. `rip` on x86_64, `pc` on aarch64)
    /// * `val`: 8-byte value
    pub fn set_register_value(&self, register_name: &str, val: u64) -> Result<(), Error> {
        disable_when_not_stared!(self);

        let in_focus_pid = self.ecx().pid_on_focus();
        let mut map = RegisterMap::current(in_focus_pid)?;
        map.update(
            Register::try_from(register_name)
                .map_err(|_| RegisterNameNotFound(register_name.into()))?,
            val,
        );
        map.persist(in_focus_pid)
    }

    /// Architecture-agnostic program-counter setter. Prefer this over
    /// `set_register_value("rip", _)` from cross-arch call sites (DAP
    /// `goto` / `restartFrame`, internal stepping helpers): the
    /// register is named `rip` on x86_64 but `pc` on aarch64.
    pub fn set_pc(&self, val: u64) -> Result<(), Error> {
        disable_when_not_stared!(self);

        let in_focus_pid = self.ecx().pid_on_focus();
        let mut map = RegisterMap::current(in_focus_pid)?;
        map.set_pc(val);
        map.persist(in_focus_pid)
    }

    /// Return the function-entry runtime address (`fn_start_ip` in
    /// FrameSpan terminology) of the function that contains `addr`,
    /// or `None` if `addr` lies outside any function we have DWARF
    /// for. Used by `bs/applyPatch` to decide whether the just-
    /// applied patch landed in the same function the focused thread
    /// is currently paused inside, which is the trigger for an
    /// auto-restart-frame.
    pub fn function_start_ip_at(&self, addr: usize) -> Option<RelocatedAddress> {
        let reloc = RelocatedAddress::from(addr);
        let global = reloc.into_global(&self.debugee).ok()?;
        let dwarf = self.debugee.debug_info(reloc).ok()?;
        let (die_ref, _) = dwarf.find_function_by_pc(global).ok().flatten()?;
        let prolog = die_ref.prolog_start_place().ok()?;
        prolog
            .address
            .relocate_to_segment_by_pc(&self.debugee, reloc)
            .ok()
    }

    /// Install a hidden transparent breakpoint at the entry of the
    /// function containing `user_bp_addr`, if one isn't already
    /// armed. When the snap-bp fires (on every call to that function),
    /// it captures the inferior's writable memory + registers into
    /// the EnC checkpoint store, so a later `restart_top_frame` for
    /// a function with inner CALLs can route through Tier-2
    /// restoration rather than the DWARF-only path.
    ///
    /// Best-effort: failures (no enclosing function found in DWARF,
    /// transparent bp install errored, address can't be resolved)
    /// log and silently return — the user's bp at `user_bp_addr` is
    /// independent and the safety gate in `restart_top_frame` will
    /// refuse cleanly if it can't find a snapshot.
    ///
    /// Idempotent per function: the second user bp in the same
    /// function reuses the first snap-bp via
    /// [`enc_checkpoint::EncCheckpointStore::is_armed`].
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn try_arm_enc_snap_bp_for_user_bp(&mut self, user_bp_addr: RelocatedAddress) {
        let Some(fn_start) = self.function_start_ip_at(usize::from(user_bp_addr)) else {
            log::debug!(
                target: "enc_checkpoint",
                "no enclosing function for user bp at 0x{:x}; skipping snap-bp arm",
                usize::from(user_bp_addr),
            );
            return;
        };
        let fn_start_u64 = fn_start.as_u64();
        if self.enc_checkpoints.is_armed(fn_start_u64) {
            return;
        }

        // The callback captures the function-entry address as a
        // plain `u64`. It runs on the supervisor thread inside the
        // transparent-bp dispatch path (see `BrkptType::Transparent`
        // handling in this module) and gets `&mut Debugger` —
        // enough to reach `enc_checkpoints` directly without any
        // RefCell dance.
        let cb_fn_start = fn_start_u64;
        let request =
            CreateTransparentBreakpointRequest::address(fn_start, move |dbg: &mut Debugger| {
                let pid = dbg.ecx().pid_on_focus();
                let regions = dbg.enc_checkpoints.capture_at(cb_fn_start, pid);
                log::trace!(
                    target: "enc_checkpoint",
                    "snap-bp fired at fn_start=0x{cb_fn_start:x}; captured {regions} regions",
                );
            });
        match self.set_transparent_breakpoint(request) {
            Ok(()) => {
                self.enc_checkpoints.mark_armed(fn_start_u64);
                log::debug!(
                    target: "enc_checkpoint",
                    "armed snap-bp at fn_start=0x{fn_start_u64:x} (triggered by user bp at 0x{:x})",
                    usize::from(user_bp_addr),
                );
            }
            Err(err) => {
                log::warn!(
                    target: "enc_checkpoint",
                    "failed to arm snap-bp at fn_start=0x{fn_start_u64:x}: {err}; \
                     EnC restart for this function will refuse on inner CALLs",
                );
            }
        }
    }

    /// "Drop and re-enter" the top frame at `fn_start` with full
    /// state restoration: PC ← `fn_start`, SP ← function-entry SP
    /// (CFA computed from DWARF), and every callee-saved register
    /// reset to the value it held at function entry (recovered via
    /// the same DWARF unwind rules `backtrace` uses). On aarch64,
    /// LR is set to the original return address so that a future
    /// RET out of the function still returns to the caller correctly.
    ///
    /// Why all this matters: the naive form (just `set_pc`) re-runs
    /// the prologue, which pushes another `fp/lr` pair, growing the
    /// stack by one frame per restart and clobbering the saved-
    /// register slots so a later step-out would land somewhere
    /// nonsense. Resetting SP + LR + the callee-saved set makes the
    /// prologue write into the SAME slots the original entry's
    /// prologue wrote into — net effect is a clean re-entry as if
    /// the function had been called fresh from the caller.
    ///
    /// **Caller-saved registers (x0..x18 on aarch64) are NOT
    /// restored** — we don't have entry-time arg values without an
    /// explicit snapshot, which is a Phase-2-snapshot-args item. If
    /// the function modified its args you'll re-enter with the
    /// modified ones; use Set Variable to fix manually if needed.
    pub fn restart_top_frame(&self, pid: Pid, fn_start: u64) -> Result<(), Error> {
        disable_when_not_stared!(self);

        // EnC restart safety gate. The DWARF-only restart path
        // restores the System-V int-arg registers and the callee-
        // saved set from the unwound frame-1 view, plus the stack
        // pointer from frame 0's CFA. That covers the *named*
        // state DWARF describes, but not:
        //   * caller-saved registers (RAX, RCX, RDX, RSI, R8..R11
        //     and XMM0..7) that the function body happens to read
        //     before writing,
        //   * unnamed stack slots that hold iterator state
        //     (`Iter::ptr/end`), drop flags, or temporary spills
        //     for trait-object dispatch,
        //   * floats passed in XMM registers (our parameter
        //     restoration skips non-integer locations).
        // Functions that only touch their named locals — leaves
        // and simple non-leaves doing arithmetic over their
        // arguments — restart safely. Functions whose body makes
        // outbound calls (`compute` calling `Iterator::sum`,
        // `slice::iter`, `precondition_check`, etc.) reliably
        // don't, because those callees inherit state we couldn't
        // reconstruct. The user-visible failure is plausible-
        // looking garbage in the function's return value — worse
        // than a clean refusal because nothing flags that the
        // numbers are lies.
        //
        // Tier-2 fast path: if we have a fn-entry snapshot for this
        // function (captured by the snap-bp armed in
        // `try_arm_enc_snap_bp_for_user_bp`), restore writable memory
        // + registers from it and we're done. This handles all the
        // cases the DWARF-only path can't — caller-saved registers,
        // iterator state in unnamed stack slots, float args in XMM,
        // mid-body heap mutations — because the snapshot was taken
        // before any of that ran.
        //
        // The snapshot's registers already encode the function-entry
        // PC (== fn_start, the snap-bp address). We `set_pc(fn_start)`
        // explicitly anyway to make the contract obvious to a future
        // reader and to defend against the unlikely case where the
        // snapshot was taken at a slightly different address.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(snapshot) = self.enc_checkpoints.peek(fn_start) {
            let report = platform_checkpoint::restore(pid, &snapshot.writable)
                .map_err(|e| Error::Hook(e))?;
            if report.skipped > 0 {
                log::warn!(
                    target: "enc_checkpoint",
                    "restore for fn_start=0x{fn_start:x}: {} regions skipped (of {} total)",
                    report.skipped, report.written + report.skipped,
                );
            }
            let mut regs = snapshot.registers.clone();
            regs.set_pc(fn_start);
            regs.persist(pid)?;
            log::debug!(
                target: "enc_checkpoint",
                "restart_top_frame: Tier-2 restore from snapshot for fn_start=0x{fn_start:x}",
            );
            return Ok(());
        }

        // No snapshot. Inner-call safety gate — refuse the cases the
        // DWARF-only path can't reconstruct. Override with
        // `DebuggerBuilder::with_force_restart(true)` when you know
        // the function is restart-safe.
        if !self.force_restart {
            let asm = self.disasm()?;
            let (inner_calls, first_call) = count_inner_calls(&asm.instructions);
            if inner_calls > 0 {
                let function = asm
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("<at 0x{fn_start:x}>"));
                let first_call_offset = first_call
                    .map(|a| format!("first at 0x{:x}", u64::from(a)))
                    .unwrap_or_else(|| "address unknown".to_string());
                let plural = if inner_calls == 1 { "" } else { "s" };
                return Err(Error::RestartRefusedInnerCalls {
                    function,
                    inner_calls,
                    plural,
                    first_call_offset,
                });
            }
        }

        // Compute the caller's register state by unwinding one
        // frame. After this call `unwound` holds:
        //   SP = CFA of frame 0 = function-entry SP
        //   PC = caller's resume address = return address LR had at fn entry
        //   X29, X19..X28, etc = values restored per DWARF register rules
        let raw = RegisterMap::current(pid)?;
        let mut unwound = DwarfRegisterMap::from(raw.clone());
        crate::debugger::debugee::dwarf::unwind::restore_registers_at_frame(
            &self.debugee,
            pid,
            &mut unwound,
            1,
        )?;

        // Helper: read a value from the unwound map by Register
        // (architecture-typed), via DWARF's numeric register id.
        // Kept named (rather than `_`-prefixed) so the cfg-gated
        // architectures that *do* use it below find it.
        #[allow(unused_variables)]
        let read_unwound = |reg: Register| -> Option<u64> {
            let dwarf_reg = reg.dwarf_register()?;
            unwound.value(dwarf_reg).ok()
        };

        let mut map = raw;

        #[cfg(target_arch = "aarch64")]
        {
            if let Some(sp) = read_unwound(Register::SP) {
                map.set_sp(sp);
            }
            // Return-address column → x30. On aarch64 the CIE's
            // `return_address_register` is conventionally x30, so the
            // unwound x30 holds the value lr had at fn entry.
            if let Some(ra) = read_unwound(Register::RA) {
                map.update(Register::X30, ra);
            }
            // Callee-saved set: x19..x28 plus x29 (frame pointer).
            for reg in [
                Register::X19,
                Register::X20,
                Register::X21,
                Register::X22,
                Register::X23,
                Register::X24,
                Register::X25,
                Register::X26,
                Register::X27,
                Register::X28,
                Register::X29,
            ] {
                if let Some(v) = read_unwound(reg) {
                    map.update(reg, v);
                }
            }
        }

        // x86_64 path: System-V passes the return address on the
        // stack — CALL pushes the resume PC and decrements RSP by
        // 8 before transferring control. To recreate fn-entry
        // state from the unwound frame-1 values:
        //
        //   * `unwound.value(SP)` at frame 1 = caller's SP at its
        //     call site = the CFA of frame 0 in DWARF terms. On
        //     x86_64 this is "RSP *before* CALL pushed the
        //     return addr" — i.e. the function-entry SP plus 8.
        //   * `unwound.value(RA)` at frame 1 = address right
        //     after CALL in the caller = the return PC that
        //     CALL pushed onto the stack.
        //
        // So fn-entry RSP = unwound_SP - 8, and the byte at that
        // slot needs to hold unwound_RA. Plus the System-V
        // callee-saved set (RBX, RBP, R12..R15) gets restored
        // from the unwinder so the patched function starts with
        // the same live state the original did.
        //
        // When the unwinder can't recover SP/RA — possible if
        // frame 0 sits in a no-DWARF leaf (signal trampoline,
        // hand-rolled asm) — fall through to set_pc only. The
        // RET at function exit will then pop garbage; the EnC
        // flow's auto-resume usually catches the user's pre-
        // patch bp before that matters.
        // x86_64 path: use frame 0's CFA (already computed by
        // the FDE for the current PC) to derive fn-entry SP,
        // then read the *actual* saved return address from
        // inferior memory at that slot. The previous spike tried
        // to use the unwinder's frame-1 RA, which is wrong for
        // leaf functions (no prologue → no FDE rows → unwinder
        // propagates a stale register through some unrelated
        // function's first row). Reading [fn_entry_RSP] direct
        // is correct regardless: at entry to ANY function the
        // saved return PC sits at the current RSP, before the
        // prologue runs.
        //
        // The DWARF CFA at the current PC encodes "where the
        // caller's RSP was just before the CALL", so:
        //   fn_entry_RSP = CFA - 8     (CALL pushed 8 bytes)
        //   [fn_entry_RSP] = the byte CALL pushed (saved return PC)
        //
        // For leaf functions (compute), CFA = current_RSP + 8,
        // so fn_entry_RSP = current_RSP — set_sp is a no-op.
        // For functions with a prologue, CFA includes the
        // prologue's allocation, so fn_entry_RSP = current_RSP
        // + allocation — set_sp moves RSP back up to entry.
        //
        // The byte at [fn_entry_RSP] is the actual return PC;
        // it's already there (CALL wrote it). We don't need to
        // re-write — just leave it and let the patched function's
        // RET pop it.
        #[cfg(target_arch = "x86_64")]
        {
            // Re-evaluate frame 0 in the current ecx so we can
            // read its CFA directly. This is the same evaluation
            // the unwinder did above, but we use the CFA only —
            // not the propagated registers.
            let frame_0_cfa = self.frame_info().ok().map(|info| u64::from(info.cfa));
            if let Some(cfa) = frame_0_cfa {
                let fn_entry_rsp = cfa.wrapping_sub(8);
                map.set_sp(fn_entry_rsp);
                // The byte at [fn_entry_rsp] is whatever CALL
                // pushed; the RET at the end of the patched
                // function will pop it and resume in the caller.
                // No write needed — it's already correct.
            }

            // Argument restoration. The patched function will re-
            // run its prologue, which reads input args from the
            // System-V int-arg registers (RDI, RSI, RDX, RCX, R8,
            // R9 — first 6 integer/pointer args). If those
            // registers have been clobbered between fn entry and
            // the pause point, the restart would compute on
            // garbage. We read each parameter's *current* value
            // via its DWARF location list — which, at the current
            // PC, resolves to the spill slot the prologue wrote
            // to — and copy that back into the input register.
            //
            // The spill slots live in the function's local frame,
            // BELOW the post-set_sp RSP (the prologue pushed RSP
            // down before spilling), so they survive our SP reset
            // intact. Tested with a non-leaf compute: paused at
            // the multiply, EDI clobbered to a loop counter; the
            // restore reads the spilled u32 and restarts compute
            // with the original input.
            //
            // V1 limits: integer/pointer args only (no floats →
            // XMM regs, no args by-value larger than 8 bytes).
            // Args 7+ live on the stack pre-call and aren't
            // touched here — the prologue reads them direct from
            // [rbp+offset] which is still correct after our SP
            // reset. Failures are silent — a missing location
            // list or unsupported type leaves the corresponding
            // register at whatever it currently holds.
            const SYSV_INT_ARG_REGS: [Register; 6] = [
                Register::Rdi,
                Register::Rsi,
                Register::Rdx,
                Register::Rcx,
                Register::R8,
                Register::R9,
            ];
            if let Ok(dwarf) = self.debugee.debug_info(self.ecx().location().pc) {
                let global_pc = self.ecx().location().global_pc;
                if let Ok(Some((func_die, _))) = dwarf.find_function_by_pc(global_pc) {
                    let params = func_die.parameters();
                    for (i, param) in params.iter().enumerate() {
                        if i >= SYSV_INT_ARG_REGS.len() {
                            break;
                        }
                        let Some(ty) = param.r#type() else { continue };
                        let Some(obj) = param.read_value(self.ecx(), &self.debugee, &ty) else {
                            continue;
                        };
                        if obj.raw_data.is_empty() || obj.raw_data.len() > 8 {
                            continue;
                        }
                        let mut buf = [0u8; 8];
                        buf[..obj.raw_data.len()].copy_from_slice(&obj.raw_data);
                        let value = u64::from_le_bytes(buf);
                        map.update(SYSV_INT_ARG_REGS[i], value);
                    }
                }
            }

            // Callee-saved restoration. The patched function's
            // prologue will save these registers (push rbp, save
            // r12-r15 etc.) — they need to hold the CALLER's
            // values at fn entry, not whatever the current body
            // has clobbered them to. The DWARF unwinder for
            // frame 0 already knows where each callee-saved was
            // spilled by the prologue (via the FDE RegisterRule
            // columns); we read those back from the stack and
            // restore. Unlike the broken RA propagation, the
            // saved-callee-register slots ARE in compute's own
            // frame and the FDE rule reads them direct from
            // [CFA + offset], so this is correct for both leaf
            // and non-leaf cases (a leaf simply has no saved
            // registers to read — the loop just does nothing).
            for reg in [
                Register::Rbx,
                Register::Rbp,
                Register::R12,
                Register::R13,
                Register::R14,
                Register::R15,
            ] {
                if let Some(v) = read_unwound(reg) {
                    map.update(reg, v);
                }
            }
        }

        map.set_pc(fn_start);
        map.persist(pid)?;
        Ok(())
    }

    /// Return list of known files income from dwarf parser.
    pub fn known_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.debugee
            .debug_info_all()
            .into_iter()
            .filter_map(|dwarf| dwarf.known_files().ok())
            .flatten()
    }

    /// Return a list of shared libraries.
    pub fn shared_libs(&self) -> Vec<RegionInfo> {
        self.debugee.dump_mapped_regions()
    }

    /// Return a list of disassembled instruction for a function in focus.
    pub fn disasm(&self) -> Result<FunctionAssembly, Error> {
        disable_when_not_stared!(self);
        self.debugee
            .disasm(self.ecx(), &self.breakpoints.active_breakpoints())
    }

    /// Resolve function name and source place for a global address.
    pub fn resolve_function_at_pc(
        &self,
        pc: GlobalAddress,
    ) -> Result<Option<(String, Option<PlaceDescriptorOwned>)>, Error> {
        disable_when_not_stared!(self);
        for dwarf in self.debugee.debug_info_all() {
            if let Ok(Some((_func, info))) = dwarf.find_function_by_pc(pc) {
                let name = info
                    .full_name()
                    .or_else(|| info.name.clone())
                    .or_else(|| info.linkage_name.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                let place = dwarf.find_place_from_pc(pc)?.map(|p| p.to_owned());
                return Ok(Some((name, place)));
            }
        }
        Ok(None)
    }

    /// Return all breakpoint-capable places for a file line range.
    pub fn breakpoint_places_for_file_range(
        &self,
        file_tpl: &str,
        start_line: u64,
        end_line: u64,
    ) -> Result<Vec<PlaceDescriptorOwned>, Error> {
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        let mut out = Vec::new();
        for dwarf in self.debugee.debug_info_all() {
            if !dwarf.has_debug_info() {
                continue;
            }
            let places = dwarf.find_places_in_line_range(file_tpl, start_line, end_line)?;
            out.extend(places.into_iter().map(|p| p.to_owned()));
        }
        Ok(out)
    }

    /// Return two place descriptors, at the start and at the end of the current function.
    pub fn current_function_range(&self) -> Result<FunctionRange<'_>, Error> {
        disable_when_not_stared!(self);
        self.debugee.function_range(self.ecx())
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        if self.process.is_external() {
            _ = self.breakpoints.disable_all_breakpoints(&self.debugee);
            // drain all watchpoints before terminating the process
            self.watchpoints
                .clear_all(self.debugee.tracee_ctl(), &mut self.breakpoints);

            let current_tids: Vec<Pid> = self
                .debugee
                .tracee_ctl()
                .tracee_iter()
                .map(|t| t.pid)
                .collect();

            if !current_tids.is_empty() {
                #[cfg(target_os = "linux")]
                {
                    current_tids.iter().for_each(|tid| {
                        sys::ptrace::detach(*tid, None).expect("detach debugee");
                    });

                    signal::kill(self.debugee.tracee_ctl().proc_pid(), Signal::SIGCONT)
                        .expect("kill debugee");
                }
                #[cfg(not(target_os = "linux"))]
                {
                    if let Ok(task) =
                        darwin_mach::task_for_pid(self.debugee.tracee_ctl().proc_pid())
                    {
                        let _ = darwin_mach::task_resume(task);
                    }
                }
            }

            return;
        }

        match self.debugee.execution_status() {
            ExecutionStatus::Unload => {
                signal::kill(self.debugee.tracee_ctl().proc_pid(), Signal::SIGKILL)
                    .expect("kill debugee");
                waitpid(self.debugee.tracee_ctl().proc_pid(), None).expect("waiting child");
            }
            ExecutionStatus::InProgress => {
                // ignore all possible errors on breakpoints disabling
                _ = self.breakpoints.disable_all_breakpoints(&self.debugee);
                // drain all watchpoints before terminating the process
                self.watchpoints
                    .clear_all(self.debugee.tracee_ctl(), &mut self.breakpoints);

                let current_tids: Vec<Pid> = self
                    .debugee
                    .tracee_ctl()
                    .tracee_iter()
                    .map(|t| t.pid)
                    .collect();

                #[cfg(target_os = "linux")]
                {
                    // todo currently ok only if all threads in group stop
                    // continue all threads with SIGSTOP
                    let prepare_stopped: Vec<_> = current_tids
                        .into_iter()
                        .filter(|&tid| sys::ptrace::cont(tid, Signal::SIGSTOP).is_ok())
                        .collect();
                    let stopped: Vec<_> = prepare_stopped
                        .into_iter()
                        .filter(|&tid| waitpid(tid, None).is_ok())
                        .collect();
                    // detach ptrace
                    stopped.into_iter().for_each(|tid| {
                        sys::ptrace::detach(tid, None).expect("detach tracee");
                    });
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = current_tids; // captured for symmetry; unused on darwin
                    self.darwin_release_inferior_for_kill();
                }
                // kill debugee process. On darwin, the inferior
                // may have already exited cleanly (passing through
                // user BPs and running to completion) by the time
                // we get here — the engine surfaces BPs and
                // inspect commands without halting forever, so the
                // inferior naturally finishes. Tolerate ESRCH from
                // kill and Exited from waitpid.
                let kill_pid = self.debugee.tracee_ctl().proc_pid();
                let _ = signal::kill(kill_pid, Signal::SIGKILL);
                // Drain wait events. On darwin, KERN_FAILURE'ing a
                // pending Mach BRK during teardown can cause the BSD
                // default to surface as a signal-*stop* (SIGTRAP)
                // before our SIGKILL lands; treat any non-terminal
                // status as "release-and-retry" (SIGCONT + SIGKILL
                // clears the stop and finishes the kill).
                //
                // Use `WNOHANG` with a short poll deadline rather
                // than a blocking `waitpid` — if the kernel never
                // delivers the terminal event (process gone on
                // another path, signal queued behind a Mach state,
                // …) we still need to give up rather than hang the
                // whole test runner.
                use nix::sys::wait::WaitPidFlag;
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
                let mut wait_result = WaitStatus::StillAlive;
                while std::time::Instant::now() < deadline {
                    let wp = waitpid(kill_pid, Some(WaitPidFlag::WNOHANG));
                    match wp {
                        Ok(WaitStatus::StillAlive) => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Ok(w @ (WaitStatus::Signaled(_, _, _) | WaitStatus::Exited(_, _))) => {
                            wait_result = w;
                            break;
                        }
                        Ok(_) => {
                            // Stopped / Continued / PtraceEvent — release and retry
                            let _ = signal::kill(kill_pid, Signal::SIGCONT);
                            let _ = signal::kill(kill_pid, Signal::SIGKILL);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(_) => {
                            // ECHILD or similar — process is gone (or never was a child)
                            break;
                        }
                    }
                }

                // On darwin the inferior may die with a variety of
                // signals during teardown:
                //   * SIGKILL — our explicit kill landed first.
                //   * SIGTRAP — an in-flight BRK exception fell
                //     through to the BSD default handler when the
                //     Mach exception port was torn down.
                //   * Any user signal that was queued in the ptrace
                //     stop at teardown time (e.g. a "transparent"
                //     SIGINT we consumed in Mach but ptrace had
                //     already queued for the BSD layer) — when we
                //     release ptrace via PT_KILL the queued signal
                //     is delivered, and its default action
                //     (terminate) wins the race against our
                //     SIGKILL.
                // All of these still mean "the inferior is gone",
                // which is the only thing the surrounding test
                // suite cares about.
                // Drop cleanup is best-effort. If `waitpid` never
                // returns a terminal status within the deadline (the
                // process is wedged in uninterruptible sleep, the
                // kernel is being slow to deliver our SIGKILL, …)
                // we just log and move on — the surrounding `Drop`
                // path is on a panic-unwinding stack, and a panic
                // here turns into a non-unwinding abort that takes
                // out the whole test process and masks every later
                // test's result. assert_no_proc!() in the test will
                // surface "still exists" as the primary failure
                // instead.
                if !matches!(
                    wait_result,
                    WaitStatus::Signaled(_, _, _) | WaitStatus::Exited(_, _)
                ) {
                    log::warn!(
                        target: "debugger",
                        "kill_pid={kill_pid} did not reach a terminal wait \
                         status within deadline (last seen {wait_result:?}); \
                         giving up — the OS will reap the inferior."
                    );
                }
            }
            ExecutionStatus::Exited => {}
        }
    }
}

/// Scan a function's disassembly and count outbound CALL/BL
/// instructions in its body. Returns `(count, first_address)` where
/// `first_address` is the location of the first such instruction —
/// used by `restart_top_frame` to surface a precise diagnostic.
///
/// Mnemonic recognition is capstone-output-shape sensitive:
///
/// * x86_64 (AT&T syntax, our default): `callq` for near-direct,
///   `callq *…` for near-indirect, occasionally `calll` / `callw`.
///   All start with `call`.
/// * aarch64: `bl` (branch-with-link to immediate) and `blr`
///   (branch-with-link to register). `b` / `br` are tail calls
///   that don't push a return address — restart-safe by
///   definition, so we don't count them.
///
/// We deliberately do *not* try to follow the calls, classify them
/// as intrinsic vs. user, or filter "obviously safe" tail calls of
/// `core::panic` etc. The point of the gate is correctness under
/// uncertainty; refusing the long tail of edge cases is the
/// expected behaviour until Tier-2 fn-entry checkpoints land.
fn count_inner_calls(
    instructions: &[debugee::disasm::Instruction],
) -> (usize, Option<GlobalAddress>) {
    let mut count = 0usize;
    let mut first: Option<GlobalAddress> = None;
    for instr in instructions {
        let Some(mn) = instr.mnemonic.as_deref() else {
            continue;
        };
        if is_call_mnemonic(mn) {
            count += 1;
            if first.is_none() {
                first = Some(instr.address);
            }
        }
    }
    (count, first)
}

/// True if this capstone mnemonic represents an outbound call that
/// pushes a return address (and therefore changes program state in
/// a way that DWARF restart can't reconstruct).
#[cfg(target_arch = "x86_64")]
fn is_call_mnemonic(mn: &str) -> bool {
    // capstone may emit "call", "callq", "calll", "callw" depending
    // on operand size and syntax. Match the common prefix to cover
    // them in one rule.
    let lower = mn.trim().to_ascii_lowercase();
    lower.starts_with("call")
}

#[cfg(target_arch = "aarch64")]
fn is_call_mnemonic(mn: &str) -> bool {
    let lower = mn.trim().to_ascii_lowercase();
    lower == "bl" || lower == "blr"
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn is_call_mnemonic(_mn: &str) -> bool {
    // On unsupported archs we don't run restart_top_frame anyway;
    // returning false keeps the safety gate from firing spuriously
    // if the cfg gates above ever shift.
    false
}

#[cfg(test)]
mod restart_safety_tests {
    use super::*;
    use crate::debugger::address::GlobalAddress;
    use crate::debugger::debugee::disasm::Instruction;

    fn mk_instr(addr: u64, mn: &str) -> Instruction {
        Instruction {
            address: GlobalAddress::from(addr),
            mnemonic: Some(mn.to_string()),
            operands: None,
        }
    }

    #[test]
    fn empty_body_has_no_inner_calls() {
        let (n, first) = count_inner_calls(&[]);
        assert_eq!(n, 0);
        assert!(first.is_none());
    }

    #[test]
    fn leaf_arithmetic_has_no_inner_calls() {
        let body = [
            mk_instr(0x1000, "mov"),
            mk_instr(0x1003, "add"),
            mk_instr(0x1006, "ret"),
        ];
        let (n, first) = count_inner_calls(&body);
        assert_eq!(n, 0);
        assert!(first.is_none());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_callq_is_recognised() {
        let body = [
            mk_instr(0x1000, "mov"),
            mk_instr(0x1003, "callq"),
            mk_instr(0x1008, "ret"),
        ];
        let (n, first) = count_inner_calls(&body);
        assert_eq!(n, 1);
        assert_eq!(first.map(u64::from), Some(0x1003));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_multiple_call_variants_all_count() {
        let body = [
            mk_instr(0x1000, "call"),
            mk_instr(0x1005, "callq"),
            mk_instr(0x100a, "calll"),
            mk_instr(0x100f, "callw"),
        ];
        let (n, first) = count_inner_calls(&body);
        assert_eq!(n, 4);
        assert_eq!(first.map(u64::from), Some(0x1000));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_jmp_is_not_a_call() {
        // jmp is a tail-call jump that doesn't push a return
        // address — restart-safe, so it must not trip the gate.
        let body = [
            mk_instr(0x1000, "mov"),
            mk_instr(0x1003, "jmp"),
            mk_instr(0x1008, "jne"),
            mk_instr(0x100c, "jz"),
        ];
        let (n, _) = count_inner_calls(&body);
        assert_eq!(n, 0);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_bl_and_blr_count() {
        let body = [
            mk_instr(0x1000, "mov"),
            mk_instr(0x1004, "bl"),
            mk_instr(0x1008, "blr"),
            mk_instr(0x100c, "ret"),
        ];
        let (n, first) = count_inner_calls(&body);
        assert_eq!(n, 2);
        assert_eq!(first.map(u64::from), Some(0x1004));
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_b_and_br_do_not_count() {
        // Plain branches are tail-calls that don't push LR —
        // restart-safe, must not count.
        let body = [
            mk_instr(0x1000, "b"),
            mk_instr(0x1004, "br"),
            mk_instr(0x1008, "b.ne"),
            mk_instr(0x100c, "ret"),
        ];
        let (n, _) = count_inner_calls(&body);
        assert_eq!(n, 0);
    }

    #[test]
    fn missing_mnemonics_are_ignored() {
        let body = [Instruction {
            address: GlobalAddress::from(0x1000_u64),
            mnemonic: None,
            operands: None,
        }];
        let (n, _) = count_inner_calls(&body);
        assert_eq!(n, 0);
    }
}

/// Read N bytes from `PID` process.
#[cfg(target_os = "linux")]
pub fn read_memory_by_pid(pid: Pid, addr: usize, read_n: usize) -> Result<Vec<u8>, nix::Error> {
    let mut read_reminder = read_n as isize;
    let mut result = Vec::with_capacity(read_n);

    let single_read_size = mem::size_of::<c_long>();

    let mut addr = addr as *mut c_long;
    while read_reminder > 0 {
        let value = sys::ptrace::read(pid, addr as *mut c_void)?;
        result.extend(value.to_ne_bytes().into_iter().take(read_reminder as usize));

        read_reminder -= single_read_size as isize;
        addr = unsafe { addr.offset(1) };
    }

    debug_assert!(result.len() == read_n);

    Ok(result)
}

/// Darwin path: `mach_vm_read_overwrite` reads N bytes in a single
/// kernel round-trip (no PTRACE_PEEKDATA-style word loop). We map
/// any Mach error to a coarse `nix::Error::EFAULT` so callers don't
/// have to know about Mach error codes.
#[cfg(target_os = "macos")]
pub fn read_memory_by_pid(pid: Pid, addr: usize, read_n: usize) -> Result<Vec<u8>, nix::Error> {
    // Log the rich MachError before collapsing to EFAULT — the
    // signature returns nix::Error so we can't propagate the kr
    // upstream; logging keeps the diagnostic recoverable from
    // `--log` output. Use `task_for_pid_or_proc` so synthetic
    // per-thread Pids (worker threads tracked by
    // `Tracer::reconcile_threads`) fall back to the inferior
    // process's task port — memory is process-scoped, the kernel
    // rejects `task_for_pid` on synthetic ids.
    let task = darwin_mach::task_for_pid_or_proc(pid).map_err(|e| {
        log::error!(target: "darwin_mach", "read_memory_by_pid task_for_pid({pid}): {e}");
        nix::errno::Errno::EFAULT
    })?;
    darwin_mach::vm_read_n(task, addr, read_n).map_err(|e| {
        log::error!(
            target: "darwin_mach",
            "read_memory_by_pid vm_read_n(addr={addr:#x}, n={read_n}): {e}"
        );
        nix::errno::Errno::EFAULT
    })
}

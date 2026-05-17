// SPDX-License-Identifier: MIT
use crate::debugger::Debugger;
use crate::debugger::address::{Address, RelocatedAddress};
use crate::debugger::debugee::Debugee;
use crate::debugger::debugee::dwarf::DebugInformation;
use crate::debugger::debugee::dwarf::unit::PlaceDescriptorOwned;
use crate::debugger::error::Error;
use crate::debugger::error::Error::{NoDebugInformation, NoSuitablePlace, PlaceNotFound};
use log::debug;
#[cfg(target_os = "linux")]
use nix::libc::c_void;
#[cfg(target_os = "linux")]
use nix::sys;
use nix::unistd::Pid;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

enum BrkptsToAddRequest {
    Init(Vec<Breakpoint>),
    Uninit(Vec<UninitBreakpoint>),
}

/// Parameters for construct a transparent breakpoint.
pub enum CreateTransparentBreakpointRequest {
    Line(String, u64, Rc<dyn Fn(&mut Debugger)>),
    Function(String, Rc<dyn Fn(&mut Debugger)>),
    /// Install at a specific relocated address. Used by the EnC
    /// snap-bp arming hook in [`crate::debugger::enc_checkpoint`] —
    /// the address comes from `function_start_ip_at(user_bp_addr)`,
    /// not from a source-level search, so the Function/Line variants
    /// would have to re-resolve a name the hook had already resolved.
    Address(RelocatedAddress, Rc<dyn Fn(&mut Debugger)>),
}

impl CreateTransparentBreakpointRequest {
    /// Create request for transparent breakpoint at function in source code.
    ///
    /// # Arguments
    ///
    /// * `f`: function search template
    /// * `cb`: callback that invoked when breakpoint is heat
    pub fn function(f: impl ToString, cb: impl Fn(&mut Debugger) + 'static) -> Self {
        Self::Function(f.to_string(), Rc::new(cb))
    }

    /// Create request for transparent breakpoint at file:line in source code.
    ///
    /// # Arguments
    ///
    /// * `file`: file search template
    /// * `line`: source code line
    /// * `cb`: callback that invoked when breakpoint is heat
    pub fn line(file: impl ToString, line: u64, cb: impl Fn(&mut Debugger) + 'static) -> Self {
        Self::Line(file.to_string(), line, Rc::new(cb))
    }

    /// Create request for transparent breakpoint at a specific relocated address.
    ///
    /// # Arguments
    ///
    /// * `addr`: relocated address in the running process
    /// * `cb`: callback that invoked when breakpoint is heat
    pub fn address(addr: RelocatedAddress, cb: impl Fn(&mut Debugger) + 'static) -> Self {
        Self::Address(addr, Rc::new(cb))
    }

    /// Return underline callback.
    fn callback(&self) -> Rc<dyn Fn(&mut Debugger)> {
        match self {
            CreateTransparentBreakpointRequest::Line(_, _, cb) => cb.clone(),
            CreateTransparentBreakpointRequest::Function(_, cb) => cb.clone(),
            CreateTransparentBreakpointRequest::Address(_, cb) => cb.clone(),
        }
    }
}

impl Debugger {
    /// Create and enable breakpoint at debugee address space
    ///
    /// # Arguments
    ///
    /// * `addr`: address where debugee must be stopped
    ///
    /// # Errors
    ///
    /// Return [`Error::PlaceNotFound`] if no place found for address,
    /// return [`Error::NoDebugInformation`] if errors occur while fetching debug information.
    pub fn set_breakpoint_at_addr(
        &mut self,
        addr: RelocatedAddress,
    ) -> Result<BreakpointView<'_>, Error> {
        if self.debugee.is_in_progress() {
            // Arm an EnC snap-bp at the enclosing function's entry
            // BEFORE the user bp goes in. Best-effort: any failure
            // (no enclosing fn found, transparent bp install errored)
            // leaves the user bp install path untouched. The snap-bp
            // is what lets `restart_top_frame` use Tier-2 writable-
            // state restoration for functions with inner CALLs.
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.try_arm_enc_snap_bp_for_user_bp(addr);

            let dwarf = self
                .debugee
                .debug_info(addr)
                .map_err(|_| NoDebugInformation("current place"))?;
            let global_addr = addr.into_global(&self.debugee)?;

            let place = dwarf
                .find_place_from_pc(global_addr)?
                .map(|p| p.to_owned())
                .ok_or(PlaceNotFound(global_addr))?;

            return self.breakpoints.add_and_enable(Breakpoint::new(
                dwarf.pathname(),
                addr,
                self.process.pid(),
                Some(place),
            ));
        }

        Ok(self.breakpoints.add_uninit(UninitBreakpoint::new(
            None::<PathBuf>,
            Address::Relocated(addr),
            self.process.pid(),
            None,
        )))
    }

    /// Disable and remove a breakpoint by it address.
    ///
    /// # Arguments
    ///
    /// * `addr`: breakpoint address
    pub fn remove_breakpoint(
        &mut self,
        addr: Address,
    ) -> Result<Option<BreakpointView<'_>>, Error> {
        self.breakpoints.remove_by_addr(addr)
    }

    /// Disable and remove a breakpoint by it number.
    ///
    /// # Arguments
    ///
    /// * `number`: breakpoint number
    pub fn remove_breakpoint_by_number(
        &mut self,
        number: u32,
    ) -> Result<Option<BreakpointView<'_>>, Error> {
        self.breakpoints.remove_by_num(number)
    }

    fn create_breakpoint_at_places(
        &self,
        places: Vec<(&DebugInformation, Vec<PlaceDescriptorOwned>)>,
    ) -> Result<BrkptsToAddRequest, Error> {
        let brkpts_to_add = if self.debugee.is_in_progress() {
            let mut to_add = Vec::new();
            for (dwarf, places) in places {
                for place in places {
                    let addr = place.address.relocate_to_segment(&self.debugee, dwarf)?;
                    to_add.push(Breakpoint::new(
                        dwarf.pathname(),
                        addr,
                        self.process.pid(),
                        Some(place),
                    ));
                }
            }
            BrkptsToAddRequest::Init(to_add)
        } else {
            let mut to_add = Vec::new();
            for (dwarf, places) in places {
                for place in places {
                    to_add.push(UninitBreakpoint::new(
                        Some(dwarf.pathname()),
                        Address::Global(place.address),
                        self.process.pid(),
                        Some(place),
                    ));
                }
            }
            BrkptsToAddRequest::Uninit(to_add)
        };
        Ok(brkpts_to_add)
    }

    fn add_breakpoints(
        &mut self,
        brkpts_to_add: BrkptsToAddRequest,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let result: Vec<_> = match brkpts_to_add {
            BrkptsToAddRequest::Init(init_brkpts) => {
                let mut result_addrs = Vec::with_capacity(init_brkpts.len());
                for brkpt in init_brkpts {
                    let addr = brkpt.addr;
                    self.breakpoints.add_and_enable(brkpt)?;
                    result_addrs.push(addr);
                }
                // EnC snap-bp arming for each newly added bp.
                // Best-effort; see set_breakpoint_at_addr.
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                for addr in &result_addrs {
                    self.try_arm_enc_snap_bp_for_user_bp(*addr);
                }
                result_addrs
                    .iter()
                    .map(|addr| {
                        BreakpointView::from(
                            self.breakpoints
                                .get_enabled(*addr)
                                .expect("breakpoint must exists"),
                        )
                    })
                    .collect()
            }
            BrkptsToAddRequest::Uninit(uninit_brkpts) => {
                let mut result_addrs = Vec::with_capacity(uninit_brkpts.len());
                for brkpt in uninit_brkpts {
                    let addr = self.breakpoints.add_uninit(brkpt).addr;
                    result_addrs.push(addr);
                }
                result_addrs
                    .iter()
                    .map(|addr| {
                        BreakpointView::from(
                            self.breakpoints
                                .get_disabled(*addr)
                                .expect("breakpoint must exists"),
                        )
                    })
                    .collect()
            }
        };

        Ok(result)
    }

    fn addresses_for_breakpoints_at_places(
        &self,
        places: &[(&DebugInformation, Vec<PlaceDescriptorOwned>)],
    ) -> Result<impl Iterator<Item = Address> + use<>, Error> {
        let mut init_addresses_to_remove: Vec<Address> = vec![];
        if self.debugee.is_in_progress() {
            for (dwarf, places) in places.iter() {
                for place in places {
                    let addr = place.address.relocate_to_segment(&self.debugee, dwarf)?;
                    init_addresses_to_remove.push(Address::Relocated(addr));
                }
            }
        };

        let uninit_addresses_to_remove: Vec<_> = places
            .iter()
            .flat_map(|(_, places)| places.iter().map(|place| Address::Global(place.address)))
            .collect();

        Ok(init_addresses_to_remove
            .into_iter()
            .chain(uninit_addresses_to_remove))
    }

    /// Remove breakpoint from instruction.
    ///
    /// # Arguments
    ///
    /// * `addresses`: address of instruction where breakpoint may be found.
    pub fn remove_breakpoints_at_addresses(
        &mut self,
        addresses: impl Iterator<Item = Address>,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let mut result = vec![];
        for to_rem in addresses {
            if let Some(view) = self.breakpoints.remove_by_addr(to_rem)? {
                result.push(view)
            }
        }
        Ok(result)
    }

    fn search_functions(
        &self,
        tpl: &str,
    ) -> Result<Vec<(&DebugInformation, Vec<PlaceDescriptorOwned>)>, Error> {
        let dwarfs = self.debugee.debug_info_all();

        dwarfs
            .iter()
            .filter(|dwarf| dwarf.has_debug_info() && dwarf.tpl_in_pub_names(tpl) != Some(false))
            .map(|&dwarf| {
                let places = dwarf.search_places_for_fn_tpl(tpl)?;
                Ok((dwarf, places))
            })
            .collect()
    }

    /// Create and enable breakpoint at debugee address space on the following function start.
    ///
    /// # Arguments
    ///
    /// * `template`: template for searchin functions where debugee must be stopped
    ///
    /// # Errors
    ///
    /// Return [`Error::PlaceNotFound`] if function not found,
    /// return [`Error::NoDebugInformation`] if errors occur while fetching debug information.
    pub fn set_breakpoint_at_fn(
        &mut self,
        template: &str,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let places = self.search_functions(template)?;
        if places.iter().all(|(_, places)| places.is_empty()) {
            return Err(NoSuitablePlace);
        }

        let brkpts = self.create_breakpoint_at_places(places)?;
        self.add_breakpoints(brkpts)
    }

    /// Disable and remove breakpoint from function start.
    ///
    /// # Arguments
    ///
    /// * `template`: template for searchin functions where breakpoints must be deleted
    pub fn remove_breakpoint_at_fn(
        &mut self,
        template: &str,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let places = self.search_functions(template)?;
        let addresses = self.addresses_for_breakpoints_at_places(&places)?;
        self.remove_breakpoints_at_addresses(addresses)
    }

    fn search_lines_in_file(
        &self,
        debug_info: &DebugInformation,
        fine_tpl: &str,
        line: u64,
    ) -> Result<Vec<PlaceDescriptorOwned>, Error> {
        let places = debug_info.find_closest_place(fine_tpl, line)?;
        Ok(places.into_iter().map(|p| p.to_owned()).collect())
    }

    fn search_lines(
        &self,
        fine_tpl: &str,
        line: u64,
    ) -> Result<Vec<(&DebugInformation, Vec<PlaceDescriptorOwned>)>, Error> {
        let dwarfs = self.debugee.debug_info_all();

        dwarfs
            .iter()
            .filter(|dwarf| dwarf.has_debug_info())
            .map(|&dwarf| {
                let places = self.search_lines_in_file(dwarf, fine_tpl, line)?;
                Ok((dwarf, places))
            })
            .collect()
    }

    /// Create and enable breakpoint at the following file and line number.
    ///
    /// # Arguments
    ///
    /// * `fine_name`: file name (ex: "main.rs")
    /// * `line`: line number
    ///
    /// # Errors
    ///
    /// Return [`Error::PlaceNotFound`] if line or file not exist,
    /// return [`Error::NoDebugInformation`] if errors occur while fetching debug information.
    pub fn set_breakpoint_at_line(
        &mut self,
        fine_path_tpl: &str,
        line: u64,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let places = self.search_lines(fine_path_tpl, line)?;
        if places.iter().all(|(_, places)| places.is_empty()) {
            return Err(NoSuitablePlace);
        }

        let brkpts = self.create_breakpoint_at_places(places)?;
        self.add_breakpoints(brkpts)
    }

    /// Same as `set_breakpoint_at_line` but also returns the line-
    /// resolution diagnostics so callers (e.g. the structured
    /// `break.set` front-end) can surface what choices the chooser
    /// made when the source line maps to multiple addresses across
    /// inlined / monomorphized copies.
    pub fn set_breakpoint_at_line_with_diagnostics(
        &mut self,
        fine_path_tpl: &str,
        line: u64,
    ) -> Result<
        (
            Vec<BreakpointView<'_>>,
            Vec<crate::debugger::debugee::dwarf::LineDiagnostics>,
        ),
        Error,
    > {
        let dwarfs = self.debugee.debug_info_all();
        let mut per_dwarf_places: Vec<(&DebugInformation, Vec<PlaceDescriptorOwned>)> = vec![];
        let mut diagnostics: Vec<crate::debugger::debugee::dwarf::LineDiagnostics> = vec![];

        for dwarf in dwarfs.iter().filter(|d| d.has_debug_info()) {
            let (places, diag) = dwarf.find_closest_place_with_diagnostics(fine_path_tpl, line)?;
            if !diag.candidates.is_empty() {
                diagnostics.push(diag);
            }
            let owned: Vec<PlaceDescriptorOwned> =
                places.into_iter().map(|p| p.to_owned()).collect();
            per_dwarf_places.push((*dwarf, owned));
        }

        if per_dwarf_places.iter().all(|(_, p)| p.is_empty()) {
            return Err(NoSuitablePlace);
        }

        let brkpts = self.create_breakpoint_at_places(per_dwarf_places)?;
        let views = self.add_breakpoints(brkpts)?;
        Ok((views, diagnostics))
    }

    /// Disable and remove breakpoint at the following file and line number.
    ///
    /// # Arguments
    ///
    /// * `fine_name`: file name (ex: "main.rs")
    /// * `line`: line number
    pub fn remove_breakpoint_at_line(
        &mut self,
        fine_name_tpl: &str,
        line: u64,
    ) -> Result<Vec<BreakpointView<'_>>, Error> {
        let places = self.search_lines(fine_name_tpl, line)?;
        let addresses = self.addresses_for_breakpoints_at_places(&places)?;
        self.remove_breakpoints_at_addresses(addresses)
    }

    /// Create and enable transparent breakpoint.
    ///
    /// # Arguments
    ///
    /// * `request`: transparent breakpoint parameters
    pub fn set_transparent_breakpoint(
        &mut self,
        request: CreateTransparentBreakpointRequest,
    ) -> Result<(), Error> {
        // transparent breakpoint currently may be set only at main object file instructions
        let debug_info = self.debugee.program_debug_info()?;

        let callback = request.callback();

        // The Address variant short-circuits the source-place
        // search — the caller already knows exactly which IP to
        // arm at. Used by the EnC snap-bp hook in
        // `crate::debugger::enc_checkpoint`, which resolves the
        // address via `function_start_ip_at(user_bp_addr)` before
        // calling in.
        if let CreateTransparentBreakpointRequest::Address(addr, _) = &request {
            let brkpt = Breakpoint::new_transparent(
                debug_info.pathname(),
                *addr,
                self.process.pid(),
                callback,
            );
            self.breakpoints.add_and_enable(brkpt)?;
            return Ok(());
        }

        let places: Vec<_> = match &request {
            CreateTransparentBreakpointRequest::Line(file, line, _) => {
                self.search_lines_in_file(debug_info, file, *line)?
            }
            CreateTransparentBreakpointRequest::Function(tpl, _) => {
                if debug_info.has_debug_info() && debug_info.tpl_in_pub_names(tpl) != Some(false) {
                    debug_info.search_places_for_fn_tpl(tpl)?
                } else {
                    vec![]
                }
            }
            CreateTransparentBreakpointRequest::Address(_, _) => {
                unreachable!("address variant short-circuits above")
            }
        };

        if places.is_empty() {
            return Err(NoSuitablePlace);
        }

        let breakpoints: Vec<_> = places
            .into_iter()
            .flat_map(|place| {
                let addr = place
                    .address
                    .relocate_to_segment(&self.debugee, debug_info)
                    .ok()?;
                Some(Breakpoint::new_transparent(
                    debug_info.pathname(),
                    addr,
                    self.process.pid(),
                    callback.clone(),
                ))
            })
            .collect();

        for brkpt in breakpoints {
            self.breakpoints.add_and_enable(brkpt)?;
        }

        Ok(())
    }

    /// Return list of breakpoints.
    pub fn breakpoints_snapshot(&self) -> Vec<BreakpointView<'_>> {
        self.breakpoints.snapshot()
    }

    /// Add new deferred breakpoint by address in debugee address space.
    pub fn add_deferred_at_addr(&mut self, addr: RelocatedAddress) {
        self.breakpoints
            .deferred_breakpoints
            .push(DeferredBreakpoint::at_address(addr));
    }

    /// Add new deferred breakpoint by function name.
    pub fn add_deferred_at_function(&mut self, function: &str) {
        self.breakpoints
            .deferred_breakpoints
            .push(DeferredBreakpoint::at_function(function));
    }

    /// Add new deferred breakpoint by file and line.
    pub fn add_deferred_at_line(&mut self, file: &str, line: u64) {
        self.breakpoints
            .deferred_breakpoints
            .push(DeferredBreakpoint::at_line(file, line));
    }

    /// Install auto-trap breakpoints — one at the panic-runtime entry,
    /// one at libc's exit handler — so the user gets a final chance to
    /// inspect locals / take a backtrace before the world goes away.
    ///
    /// Both traps are best-effort: a symbol the linker stripped or a
    /// runtime that doesn't use the canonical names (no_std, custom
    /// libc, …) is skipped silently. They install as ordinary
    /// breakpoints so the user can `break.remove` them by number if
    /// they're in the way.
    ///
    /// Returns `(panic_traps_set, exit_traps_set)`.
    pub fn install_auto_traps(&mut self) -> (usize, usize) {
        // Candidate symbols — broad on purpose; the same Rust binary
        // may carry any subset depending on toolchain version, panic
        // strategy, and whether the unwind runtime is statically
        // linked. We try them all, deduplicating in practice because
        // `set_breakpoint_at_fn` only fires once per resolved place.
        const PANIC_FNS: &[&str] = &[
            // Rust 1.83+ panic handler (the runtime hook entry).
            "std::panicking::rust_panic_with_hook",
            // Older Rust handler.
            "std::panicking::begin_panic_handler",
            // Formatted-arg panic entry from core::panicking.
            "core::panicking::panic_fmt",
            // The catch-all `_Unwind_RaiseException` rust-shim.
            "rust_panic",
        ];
        const EXIT_FNS: &[&str] = &[
            // Rust-side explicit exit.
            "std::process::exit",
            // libc's exit (runs atexit handlers, then _exit).
            "exit",
            // glibc-internal exit entry, sometimes the only one visible.
            "__GI_exit",
        ];

        let mut panic_traps = 0usize;
        for name in PANIC_FNS {
            if self.set_breakpoint_at_fn(name).is_ok() {
                panic_traps += 1;
                debug!(target: "auto-trap", "installed panic trap at `{name}`");
            }
        }
        let mut exit_traps = 0usize;
        for name in EXIT_FNS {
            if self.set_breakpoint_at_fn(name).is_ok() {
                exit_traps += 1;
                debug!(target: "auto-trap", "installed exit trap at `{name}`");
            }
        }

        // Libc's `exit` lives in `libc.so.6` and is reachable via the
        // dynamic-linker symbol table, but it usually carries no DWARF
        // — `set_breakpoint_at_fn` won't find it. Walk every loaded
        // module's symbol table directly. We set the breakpoint at
        // the raw address (no `place` info) — when the trap fires the
        // bt walks up into the caller's DWARF as usual, so the user
        // still sees a useful stack.
        const LIBC_EXIT_FNS: &[&str] = &["exit", "_exit", "__GI_exit", "__libc_exit"];
        let modules: Vec<_> = self
            .debugee
            .debug_info_all()
            .into_iter()
            .map(|d| (d.pathname().to_path_buf(), d as *const _ as usize))
            .collect();
        let _ = modules; // we only re-fetch fresh refs in the loop body
        // Iterate by name to avoid hanging onto borrows of `self.debugee`
        // across `self.breakpoints.add_and_enable`.
        let info_count = self.debugee.debug_info_all().len();
        for idx in 0..info_count {
            for name in LIBC_EXIT_FNS {
                let (runtime, pathname) = {
                    let infos = self.debugee.debug_info_all();
                    let Some(info) = infos.get(idx) else { continue };
                    let Some(global) = info.symbol_address(name) else {
                        continue;
                    };
                    let Ok(runtime) = global.relocate_to_segment(&self.debugee, info) else {
                        continue;
                    };
                    (runtime, info.pathname().to_path_buf())
                };
                let bp = Breakpoint::new(pathname, runtime, self.process.pid(), None);
                if self.breakpoints.add_and_enable(bp).is_ok() {
                    exit_traps += 1;
                    debug!(target: "auto-trap", "installed libc exit trap at `{name}` @ {runtime}");
                    break; // first match in this module wins
                }
            }
        }
        (panic_traps, exit_traps)
    }

    /// Refresh deferred breakpoints. Trying to set breakpoint if success - remove
    /// breakpoint from a deferred list.
    pub fn refresh_deferred(&mut self) -> Vec<Error> {
        let mut errors = vec![];

        let mut deferred_brkpts = mem::take(&mut self.breakpoints.deferred_breakpoints);
        deferred_brkpts.retain(|brkpt| {
            let mb_error = match &brkpt {
                DeferredBreakpoint::Address(addr) => self.set_breakpoint_at_addr(*addr).err(),
                DeferredBreakpoint::Line(file, line) => {
                    self.set_breakpoint_at_line(file, *line).err()
                }
                DeferredBreakpoint::Function(function) => self.set_breakpoint_at_fn(function).err(),
            };

            match mb_error {
                None => false,
                Some(NoSuitablePlace) => true,
                Some(err) => {
                    errors.push(err);
                    true
                }
            }
        });
        self.breakpoints.deferred_breakpoints = deferred_brkpts;

        errors
    }
}

#[derive(Clone)]
pub enum BrkptType {
    /// Breakpoint to program entry point
    EntryPoint,
    /// User defined breakpoint
    UserDefined,
    /// This breakpoint created as a companion to the watchpoint.
    /// Stops the program when the watchpoint expression leaves a scope where it is valid.
    /// Contains linked watchpoint numbers.
    WatchpointCompanion(Vec<u32>),
    /// Auxiliary breakpoints, using, for example, in step-over implementation.
    Temporary,
    /// Same is temporary breakpoints, using in async steps implementation.
    TemporaryAsync,
    /// Breakpoint at linker internal function that will always be called when the linker
    /// begins to map in a library or unmap it, and again when the mapping change is complete.
    LinkerMapFn,
    /// Transparent breakpoints are transparent for debugger user and using it by inner mechanisms
    /// like oracles.
    Transparent(Rc<dyn Fn(&mut Debugger)>),
}

impl Debug for BrkptType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BrkptType::EntryPoint => f.write_str("entry-point"),
            BrkptType::UserDefined => f.write_str("user-defined"),
            BrkptType::Temporary => f.write_str("temporary"),
            BrkptType::LinkerMapFn => f.write_str("linker-map"),
            BrkptType::Transparent(_) => f.write_str("transparent"),
            BrkptType::WatchpointCompanion(_) => f.write_str("watchpoint-companion"),
            BrkptType::TemporaryAsync => f.write_str("temporary-async"),
        }
    }
}

impl PartialEq for BrkptType {
    fn eq(&self, other: &Self) -> bool {
        match self {
            BrkptType::EntryPoint => {
                matches!(other, BrkptType::EntryPoint)
            }
            BrkptType::UserDefined => {
                matches!(other, BrkptType::UserDefined)
            }
            BrkptType::Temporary => {
                matches!(other, BrkptType::Temporary)
            }
            BrkptType::TemporaryAsync => {
                matches!(other, BrkptType::TemporaryAsync)
            }
            BrkptType::LinkerMapFn => {
                matches!(other, BrkptType::LinkerMapFn)
            }
            BrkptType::Transparent(_) => {
                matches!(other, BrkptType::Transparent(_))
            }
            BrkptType::WatchpointCompanion(nums) => {
                matches!(other, BrkptType::WatchpointCompanion(other_nums) if nums == other_nums)
            }
        }
    }
}

static GLOBAL_BP_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Breakpoint representation.
#[derive(Debug, Clone)]
pub struct Breakpoint {
    pub addr: RelocatedAddress,
    pub pid: Pid,
    /// Breakpoint number, > 0 for user defined breakpoints have a number, 0 for others
    number: u32,
    /// Place information, None if brkpt is a temporary or entry point
    place: Option<PlaceDescriptorOwned>,
    pub saved_data: Cell<u64>,
    enabled: Cell<bool>,
    r#type: BrkptType,
    pub debug_info_file: PathBuf,
}

impl Breakpoint {
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.get()
    }
}

impl Breakpoint {
    /// Software-breakpoint opcode.
    ///
    /// x86_64: `INT3` = `0xCC` (1 byte).
    /// aarch64: `BRK #0` = `0xD4200000` (4 bytes, little-endian).
    #[cfg(target_arch = "x86_64")]
    const BRK_OPCODE: u64 = 0xCC;
    #[cfg(target_arch = "x86_64")]
    const BRK_MASK: u64 = 0xff;

    #[cfg(target_arch = "aarch64")]
    const BRK_OPCODE: u64 = 0xD420_0000;
    #[cfg(target_arch = "aarch64")]
    const BRK_MASK: u64 = 0xFFFF_FFFF;

    /// Adjustment from the signalled PC back to the breakpoint address.
    ///
    /// x86_64 reports PC *after* the 1-byte `INT3` trap, so we rewind by 1.
    /// aarch64 reports PC *at* the 4-byte `BRK #0`, so no rewind is needed.
    #[cfg(target_arch = "x86_64")]
    pub const PC_ADJUST: u64 = 1;
    #[cfg(target_arch = "aarch64")]
    pub const PC_ADJUST: u64 = 0;

    #[inline(always)]
    fn new_inner(
        addr: RelocatedAddress,
        pid: Pid,
        number: u32,
        place: Option<PlaceDescriptorOwned>,
        r#type: BrkptType,
        debug_info_file: PathBuf,
    ) -> Self {
        Self {
            addr,
            number,
            pid,
            place,
            enabled: Default::default(),
            saved_data: Default::default(),
            r#type,
            debug_info_file,
        }
    }

    #[inline(always)]
    pub fn new(
        debug_info_file: impl Into<PathBuf>,
        addr: RelocatedAddress,
        pid: Pid,
        place: Option<PlaceDescriptorOwned>,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            GLOBAL_BP_COUNTER.fetch_add(1, Ordering::Relaxed),
            place,
            BrkptType::UserDefined,
            debug_info_file.into(),
        )
    }

    #[inline(always)]
    pub fn new_entry_point(
        debug_info_file: impl Into<PathBuf>,
        addr: RelocatedAddress,
        pid: Pid,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::EntryPoint,
            debug_info_file.into(),
        )
    }

    #[inline(always)]
    pub fn new_temporary(
        debug_info_file: impl Into<PathBuf>,
        addr: RelocatedAddress,
        pid: Pid,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::Temporary,
            debug_info_file.into(),
        )
    }

    #[inline(always)]
    pub fn new_temporary_async(
        debug_info_file: impl Into<PathBuf>,
        addr: RelocatedAddress,
        pid: Pid,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::TemporaryAsync,
            debug_info_file.into(),
        )
    }

    #[inline(always)]
    pub fn new_linker_map(addr: RelocatedAddress, pid: Pid) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::LinkerMapFn,
            PathBuf::default(),
        )
    }

    #[inline(always)]
    pub fn new_transparent(
        debug_info_file: impl Into<PathBuf>,
        addr: RelocatedAddress,
        pid: Pid,
        callback: Rc<dyn Fn(&mut Debugger)>,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::Transparent(callback),
            debug_info_file.into(),
        )
    }

    #[inline(always)]
    pub(super) fn new_watchpoint_companion(
        registry: &BreakpointRegistry,
        wp_num: u32,
        addr: RelocatedAddress,
        pid: Pid,
    ) -> Self {
        let (wp_nums, brkpt_num) = if let Some(Breakpoint {
            r#type: BrkptType::WatchpointCompanion(nums),
            number,
            ..
        }) = registry.get_enabled(addr)
        {
            // reuse if a companion already exists
            let mut nums = nums.to_vec();
            nums.push(wp_num);
            (nums, *number)
        } else {
            (
                vec![wp_num],
                GLOBAL_BP_COUNTER.fetch_add(1, Ordering::Relaxed),
            )
        };

        Self::new_inner(
            addr,
            pid,
            brkpt_num,
            None,
            BrkptType::WatchpointCompanion(wp_nums),
            PathBuf::default(),
        )
    }

    #[inline(always)]
    pub fn number(&self) -> u32 {
        self.number
    }

    /// Return breakpoint place information.
    ///
    /// # Panics
    ///
    /// Panic if a breakpoint is not a user defined.
    /// It is the caller's responsibility to check that the type is [`BrkptType::UserDefined`].
    pub fn place(&self) -> Option<&PlaceDescriptorOwned> {
        match self.r#type {
            BrkptType::UserDefined => self.place.as_ref(),
            BrkptType::EntryPoint
            | BrkptType::Temporary
            | BrkptType::TemporaryAsync
            | BrkptType::LinkerMapFn
            | BrkptType::WatchpointCompanion(_)
            | BrkptType::Transparent(_) => {
                panic!("only user defined breakpoint has a place attribute")
            }
        }
    }

    pub fn is_entry_point(&self) -> bool {
        self.r#type == BrkptType::EntryPoint
    }

    pub fn is_wp_companion(&self) -> bool {
        matches!(self.r#type, BrkptType::WatchpointCompanion(_))
    }

    #[inline(always)]
    pub fn r#type(&self) -> &BrkptType {
        &self.r#type
    }

    #[inline(always)]
    pub fn is_temporary(&self) -> bool {
        matches!(self.r#type, BrkptType::Temporary)
    }

    #[inline(always)]
    pub fn is_temporary_async(&self) -> bool {
        matches!(self.r#type, BrkptType::TemporaryAsync)
    }

    #[cfg(target_os = "linux")]
    pub fn enable(&self) -> Result<(), Error> {
        let addr = self.addr.as_usize() as *mut c_void;
        let data = sys::ptrace::read(self.pid, addr).map_err(Error::Ptrace)? as u64;
        self.saved_data.set(data & Self::BRK_MASK);
        let data_with_pb = (data & !Self::BRK_MASK) | Self::BRK_OPCODE;
        unsafe {
            sys::ptrace::write(self.pid, addr, data_with_pb as *mut c_void)
                .map_err(Error::Ptrace)?;
        }
        self.enabled.set(true);

        Ok(())
    }

    /// Darwin path: same `BRK_OPCODE` / `BRK_MASK` / `PC_ADJUST`
    /// the linux/aarch64 path uses; only the kernel call shape
    /// changes — we route through `darwin_mach::vm_read_n` /
    /// `vm_write_word` (which itself widens the page to RW with
    /// `mach_vm_protect(VM_PROT_COPY|READ|WRITE)` first, so the
    /// CoW'd text page becomes writable for the duration).
    #[cfg(not(target_os = "linux"))]
    pub fn enable(&self) -> Result<(), Error> {
        use crate::debugger::darwin_mach;
        // `self.pid` may be a synthetic per-thread pid
        // (proc_pid + 1_000_000) when the breakpoint comes from a
        // step-over computed at a stop on a worker thread. Synthetic
        // pids aren't real kernel pids — task_for_pid rejects them
        // with KERN_FAILURE. `task_for_pid_or_proc` falls back to
        // the proc's task port (same task for all threads in the
        // process), which is what we want for memory ops here.
        let task = darwin_mach::task_for_pid_or_proc(self.pid)?;
        let addr = self.addr.as_usize();
        let bytes = darwin_mach::vm_read_n(task, addr, std::mem::size_of::<u64>())?;
        let arr: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .expect("vm_read_n returns exactly 8 bytes");
        let data = u64::from_ne_bytes(arr);
        self.saved_data.set(data & Self::BRK_MASK);
        let data_with_pb = (data & !Self::BRK_MASK) | Self::BRK_OPCODE;
        darwin_mach::vm_write_word(task, addr, data_with_pb as usize)?;
        self.enabled.set(true);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn disable(&self) -> Result<(), Error> {
        let addr = self.addr.as_usize() as *mut c_void;
        let data = sys::ptrace::read(self.pid, addr).map_err(Error::Ptrace)? as u64;
        let restored: u64 = (data & !Self::BRK_MASK) | self.saved_data.get();
        unsafe {
            sys::ptrace::write(self.pid, addr, restored as *mut c_void).map_err(Error::Ptrace)?;
        }
        self.enabled.set(false);

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn disable(&self) -> Result<(), Error> {
        use crate::debugger::darwin_mach;
        // `self.pid` may be a synthetic per-thread pid
        // (proc_pid + 1_000_000) when the breakpoint comes from a
        // step-over computed at a stop on a worker thread. Synthetic
        // pids aren't real kernel pids — task_for_pid rejects them
        // with KERN_FAILURE. `task_for_pid_or_proc` falls back to
        // the proc's task port (same task for all threads in the
        // process), which is what we want for memory ops here.
        let task = darwin_mach::task_for_pid_or_proc(self.pid)?;
        let addr = self.addr.as_usize();
        let bytes = darwin_mach::vm_read_n(task, addr, std::mem::size_of::<u64>())?;
        let arr: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .expect("vm_read_n returns exactly 8 bytes");
        let data = u64::from_ne_bytes(arr);
        let restored: u64 = (data & !Self::BRK_MASK) | self.saved_data.get();
        darwin_mach::vm_write_word(task, addr, restored as usize)?;
        self.enabled.set(false);
        Ok(())
    }
}

/// User defined breakpoint template,
/// may create if debugee program not running and
/// there is no, and there is no way to determine the relocated address.
#[derive(Debug, Clone)]
pub struct UninitBreakpoint {
    addr: Address,
    pid: Pid,
    number: u32,
    place: Option<PlaceDescriptorOwned>,
    r#type: BrkptType,
    debug_info_file: Option<PathBuf>,
}

impl UninitBreakpoint {
    fn new_inner(
        addr: Address,
        pid: Pid,
        number: u32,
        place: Option<PlaceDescriptorOwned>,
        r#type: BrkptType,
        debug_info_file: Option<PathBuf>,
    ) -> Self {
        Self {
            addr,
            pid,
            number,
            place,
            r#type,
            debug_info_file,
        }
    }

    pub fn new(
        debug_info_file: Option<impl Into<PathBuf>>,
        addr: Address,
        pid: Pid,
        place: Option<PlaceDescriptorOwned>,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            GLOBAL_BP_COUNTER.fetch_add(1, Ordering::Relaxed),
            place,
            BrkptType::UserDefined,
            debug_info_file.map(|path| path.into()),
        )
    }

    pub fn new_inherited(addr: Address, brkpt: Breakpoint) -> Self {
        Self::new_inner(
            addr,
            brkpt.pid,
            brkpt.number,
            brkpt.place,
            BrkptType::UserDefined,
            Some(brkpt.debug_info_file),
        )
    }

    pub fn new_entry_point(
        debug_info_file: Option<impl Into<PathBuf>>,
        addr: Address,
        pid: Pid,
    ) -> Self {
        Self::new_inner(
            addr,
            pid,
            0,
            None,
            BrkptType::EntryPoint,
            debug_info_file.map(|path| path.into()),
        )
    }

    /// Return a breakpoint created from template.
    ///
    /// # Panics
    ///
    /// Method will panic if calling with unloaded debugee.
    pub fn try_into_brkpt(self, debugee: &Debugee) -> Result<Breakpoint, Error> {
        debug_assert!(
            self.r#type == BrkptType::EntryPoint || self.r#type == BrkptType::UserDefined
        );

        let (global_addr, rel_addr) = match self.addr {
            Address::Relocated(addr) => (addr.into_global(debugee)?, Some(addr)),
            Address::Global(addr) => (addr, None),
        };

        let dwarf = match self.debug_info_file {
            None if self.r#type == BrkptType::EntryPoint => debugee.program_debug_info().ok(),
            None => rel_addr.and_then(|addr| debugee.debug_info(addr).ok()),
            Some(path) => debugee.debug_info_from_file(&path).ok(),
        }
        .ok_or(NoDebugInformation("breakpoint"))?;

        let place = if self.r#type == BrkptType::UserDefined {
            if self.place.is_some() {
                self.place
            } else {
                Some(
                    dwarf
                        .find_place_from_pc(global_addr)?
                        .ok_or(PlaceNotFound(global_addr))?
                        .to_owned(),
                )
            }
        } else {
            None
        };

        Ok(Breakpoint::new_inner(
            global_addr.relocate_to_segment(debugee, dwarf)?,
            self.pid,
            self.number,
            place,
            self.r#type,
            dwarf.pathname().into(),
        ))
    }
}

pub struct BreakpointView<'a> {
    pub addr: Address,
    pub number: u32,
    pub place: Option<Cow<'a, PlaceDescriptorOwned>>,
}

impl From<Breakpoint> for BreakpointView<'_> {
    fn from(brkpt: Breakpoint) -> Self {
        Self {
            addr: Address::Relocated(brkpt.addr),
            number: brkpt.number,
            place: brkpt.place.map(Cow::Owned),
        }
    }
}

impl<'a> From<&'a Breakpoint> for BreakpointView<'a> {
    fn from(brkpt: &'a Breakpoint) -> Self {
        Self {
            addr: Address::Relocated(brkpt.addr),
            number: brkpt.number,
            place: brkpt.place.as_ref().map(Cow::Borrowed),
        }
    }
}

impl From<UninitBreakpoint> for BreakpointView<'_> {
    fn from(brkpt: UninitBreakpoint) -> Self {
        Self {
            addr: brkpt.addr,
            number: brkpt.number,
            place: brkpt.place.map(Cow::Owned),
        }
    }
}

impl<'a> From<&'a UninitBreakpoint> for BreakpointView<'a> {
    fn from(brkpt: &'a UninitBreakpoint) -> Self {
        Self {
            addr: brkpt.addr,
            number: brkpt.number,
            place: brkpt.place.as_ref().map(Cow::Borrowed),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BreakpointViewOwned {
    pub addr: Address,
    pub number: u32,
    pub place: Option<PlaceDescriptorOwned>,
}

impl BreakpointView<'_> {
    pub fn to_owned(&self) -> BreakpointViewOwned {
        BreakpointViewOwned {
            addr: self.addr,
            number: self.number,
            place: self.place.clone().map(|p| p.into_owned()),
        }
    }
}

/// User breakpoint deferred until a shared library with target place will be loaded.
pub enum DeferredBreakpoint {
    Address(RelocatedAddress),
    Line(String, u64),
    Function(String),
}

impl DeferredBreakpoint {
    pub fn at_address(addr: RelocatedAddress) -> DeferredBreakpoint {
        DeferredBreakpoint::Address(addr)
    }

    pub fn at_line(file: &str, line: u64) -> DeferredBreakpoint {
        DeferredBreakpoint::Line(file.to_string(), line)
    }

    pub fn at_function(function: &str) -> DeferredBreakpoint {
        DeferredBreakpoint::Function(function.to_string())
    }
}

/// Container for application breakpoints.
/// Supports active breakpoints and uninit breakpoints.
#[derive(Default)]
pub struct BreakpointRegistry {
    /// Active breakpoint list.
    breakpoints: HashMap<RelocatedAddress, Breakpoint>,
    /// Non-active breakpoint list.
    disabled_breakpoints: HashMap<Address, UninitBreakpoint>,
    /// List of deferred breakpoints, refresh all time when shared library loading.
    deferred_breakpoints: Vec<DeferredBreakpoint>,
}

impl BreakpointRegistry {
    /// Add a new breakpoint to registry and enable it.
    pub fn add_and_enable(&mut self, brkpt: Breakpoint) -> Result<BreakpointView<'_>, Error> {
        if let Some(existed) = self.breakpoints.get(&brkpt.addr) {
            existed.disable()?;
        }
        brkpt.enable()?;

        let addr = brkpt.addr;
        self.breakpoints.insert(addr, brkpt);
        Ok((&self.breakpoints[&addr]).into())
    }

    pub fn get_enabled(&self, addr: RelocatedAddress) -> Option<&Breakpoint> {
        self.breakpoints.get(&addr)
    }

    pub fn get_disabled(&self, addr: Address) -> Option<&UninitBreakpoint> {
        self.disabled_breakpoints.get(&addr)
    }

    /// Add uninit breakpoint, this means that breakpoint will be created later.
    pub fn add_uninit(&mut self, brkpt: UninitBreakpoint) -> BreakpointView<'_> {
        let addr = brkpt.addr;
        self.disabled_breakpoints.insert(addr, brkpt);
        (&self.disabled_breakpoints[&addr]).into()
    }

    /// Decrease watchpoint reference counter for companion breakpoint.
    /// Remove breakpoint if there are no more references to watchpoints.
    ///
    /// # Arguments
    ///
    /// * `num`: companion breakpoint number
    /// * `target_wp_num`: watchpoint number
    ///
    /// # Panics
    ///
    /// Panic if breakpoint is not a companion type.
    pub fn decrease_companion_rc(&mut self, num: u32, target_wp_num: u32) -> Result<(), Error> {
        let Some(companion) = self
            .breakpoints
            .values_mut()
            .find(|brkpt| brkpt.number == num)
        else {
            return Ok(());
        };

        let BrkptType::WatchpointCompanion(wps) = companion.r#type() else {
            panic!("not a watchpoint companion");
        };
        debug_assert!(wps.contains(&target_wp_num));

        if wps.len() == 1 && wps[0] == target_wp_num {
            self.remove_by_num(num)?;
        } else {
            let new_wps: Vec<_> = wps
                .iter()
                .filter(|&&wp_num| wp_num != target_wp_num)
                .copied()
                .collect();
            companion.r#type = BrkptType::WatchpointCompanion(new_wps);
        };

        Ok(())
    }

    /// Remove breakpoint or uninit breakpoint from registry.
    pub fn remove_by_addr(
        &mut self,
        addr: Address,
    ) -> Result<Option<BreakpointView<'static>>, Error> {
        if let Some(brkpt) = self.disabled_breakpoints.remove(&addr) {
            return Ok(Some(brkpt.into()));
        }
        if let Address::Relocated(addr) = addr
            && let Some(brkpt) = self.breakpoints.remove(&addr)
        {
            if brkpt.is_enabled() {
                brkpt.disable()?;
            }
            return Ok(Some(brkpt.into()));
        }
        Ok(None)
    }

    /// Remove enabled breakpoint from registry by it number.
    pub fn remove_by_num(&mut self, number: u32) -> Result<Option<BreakpointView<'static>>, Error> {
        if let Some(addr) = self.disabled_breakpoints.iter().find_map(|(addr, brkpt)| {
            if brkpt.number == number {
                return Some(addr);
            }
            None
        }) {
            return self.remove_by_addr(*addr);
        }

        if let Some(addr) = self.breakpoints.iter().find_map(|(addr, brkpt)| {
            if brkpt.number == number {
                return Some(addr);
            }
            None
        }) {
            return self.remove_by_addr(Address::Relocated(*addr));
        }

        Ok(None)
    }

    /// Enable currently disabled breakpoints.
    pub fn enable_all_breakpoints(&mut self, debugee: &Debugee) -> Vec<Error> {
        let mut errors = vec![];
        let mut disabled_breakpoints = mem::take(&mut self.disabled_breakpoints);
        for (_, uninit_brkpt) in disabled_breakpoints.drain() {
            let brkpt = match uninit_brkpt.try_into_brkpt(debugee) {
                Ok(b) => b,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };

            if let Err(e) = self.add_and_enable(brkpt) {
                errors.push(e);
            }
        }
        errors
    }

    /// Enable entry point breakpoint if it disabled.
    pub fn enable_entry_breakpoint(&mut self, debugee: &Debugee) -> Result<(), Error> {
        let Some((&key, _)) = self
            .disabled_breakpoints
            .iter()
            .find(|(_, brkpt)| brkpt.r#type == BrkptType::EntryPoint)
        else {
            return Ok(());
        };

        let uninit_entry_point_brkpt = self.disabled_breakpoints.remove(&key).unwrap();

        let brkpt = uninit_entry_point_brkpt.try_into_brkpt(debugee)?;
        self.add_and_enable(brkpt)?;

        Ok(())
    }

    /// Disable currently enabled breakpoints.
    pub fn disable_all_breakpoints(&mut self, debugee: &Debugee) -> Result<Vec<Error>, Error> {
        let mut errors = vec![];
        let mut breakpoints = std::mem::take(&mut self.breakpoints);
        for (_, brkpt) in breakpoints.drain() {
            if let Err(e) = brkpt.disable() {
                errors.push(e);
            }

            // Only compute the global address for breakpoints we
            // actually re-save below — `LinkerMapFn` BPs sit in
            // dyld/ld-linux pages whose mapping the registry may
            // not track (notably on darwin, where dsymutil never
            // covers system libraries), so converting them would
            // fail with `MappingOffsetNotFound` and abort the whole
            // restart. We're throwing those away anyway.
            match brkpt.r#type {
                BrkptType::EntryPoint => {
                    let addr = Address::Global(brkpt.addr.into_global(debugee)?);
                    self.add_uninit(UninitBreakpoint::new_entry_point(
                        Some(brkpt.debug_info_file),
                        addr,
                        brkpt.pid,
                    ));
                }
                BrkptType::UserDefined => {
                    let addr = Address::Global(brkpt.addr.into_global(debugee)?);
                    self.add_uninit(UninitBreakpoint::new_inherited(addr, brkpt));
                }
                BrkptType::Temporary
                | BrkptType::TemporaryAsync
                | BrkptType::LinkerMapFn
                | BrkptType::Transparent(_)
                | BrkptType::WatchpointCompanion(_) => {}
            }
        }
        Ok(errors)
    }

    /// Update pid of all breakpoints.
    pub fn update_pid(&mut self, new_pid: Pid) {
        self.breakpoints
            .iter_mut()
            .for_each(|(_, brkpt)| brkpt.pid = new_pid);
        self.disabled_breakpoints
            .iter_mut()
            .for_each(|(_, brkpt)| brkpt.pid = new_pid);
    }

    /// Return vector of currently enabled breakpoints.
    pub fn active_breakpoints(&self) -> Vec<&Breakpoint> {
        self.breakpoints.values().collect()
    }

    /// Return view for all user-defined breakpoints.
    pub fn snapshot(&self) -> Vec<BreakpointView<'_>> {
        let active_bps = self
            .breakpoints
            .values()
            .filter(|&bp| bp.r#type() == &BrkptType::UserDefined)
            .map(BreakpointView::from);
        let disabled_brkpts = self
            .disabled_breakpoints
            .values()
            .filter(|&bp| bp.r#type == BrkptType::UserDefined)
            .map(BreakpointView::from);

        let mut snap = active_bps.chain(disabled_brkpts).collect::<Vec<_>>();
        snap.sort_by(|a, b| a.number.cmp(&b.number));

        snap
    }
}

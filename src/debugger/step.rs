// SPDX-License-Identifier: MIT
use crate::debugger::address::{Address, GlobalAddress, RelocatedAddress};
use crate::debugger::breakpoint::Breakpoint;
use crate::debugger::debugee::dwarf::unit::PlaceDescriptorOwned;
use crate::debugger::debugee::tracer::{StopReason, TraceContext, WatchpointHitType};
use crate::debugger::error::Error;
use crate::debugger::error::Error::{NoFunctionRanges, ProcessExit};
use crate::debugger::{Debugger, ExplorationContext};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use std::path::Path;

/// Whether a stopped frame is the user's own code or library/runtime
/// code. Drives "Step-In, skip libraries" (just-my-code stepping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Source file resolves to the user's own crate(s).
    UserCode,
    /// Cargo dep, std/core, the toolchain, or a frame with no line info.
    Library,
}

/// Path fragments that mark a DWARF source/decl path as library or
/// toolchain code rather than the user's own crate. Matched as
/// substrings. Covers cargo deps (`/registry/`), the cargo home
/// (`/.cargo/`), rustup toolchains (`/.rustup/`, `/toolchains/`), and
/// the rust std/compiler sources (`/rustc/`). The rustup layout already
/// nests the sysroot under `/.rustup/toolchains/<tc>/…`, so the
/// `rustc --print sysroot` prefix mentioned in the design is subsumed
/// here; v0 deliberately avoids shelling out, for determinism.
const LIBRARY_PATH_FRAGMENTS: &[&str] = &[
    "/registry/",
    "/.cargo/",
    "/.rustup/",
    "/toolchains/",
    "/rustc/",
];

/// Classify a DWARF source path as user code or library code. A `None`
/// path — a frame with no line info (PLT stub, stripped/FFI frame) — is
/// `Library`: there's nothing there for the user to read.
///
/// ```
/// use std::path::Path;
/// use bugstalker::debugger::{classify_source_path, FrameKind};
///
/// assert_eq!(
///     classify_source_path(Some(Path::new("/home/me/proj/src/main.rs"))),
///     FrameKind::UserCode,
/// );
/// assert_eq!(
///     classify_source_path(Some(Path::new(
///         "/home/me/.cargo/registry/src/index.crates.io-x/serde-1/src/lib.rs"
///     ))),
///     FrameKind::Library,
/// );
/// assert_eq!(classify_source_path(None), FrameKind::Library);
/// ```
pub fn classify_source_path(path: Option<&Path>) -> FrameKind {
    let Some(path) = path else {
        return FrameKind::Library;
    };
    let s = path.to_string_lossy();
    if LIBRARY_PATH_FRAGMENTS.iter().any(|frag| s.contains(frag)) {
        FrameKind::Library
    } else {
        FrameKind::UserCode
    }
}

/// Which frames a Step-In is allowed to stop in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepIntoMode {
    /// Descend into whatever the current line calls, library or not
    /// (classic Step-In).
    AnyFrame,
    /// Step transparently through library/runtime frames and stop at
    /// the next line of user code ("just my code").
    SkipLibraries,
}

/// Whether a stepping error is recoverable during a "skip libraries"
/// walk — i.e. it means "stepped into code we can't introspect" (a
/// stripped dylib with no DWARF, or a frame the unwinder can't read)
/// rather than a genuine fault. Such a step should degrade to climbing
/// back out to user code, not abort the whole step.
fn is_recoverable_step_error(e: &Error) -> bool {
    matches!(e, Error::NoDebugInformation(_) | Error::DwarfParsing(_))
}

/// Backstop on the number of line-steps the [`StepIntoMode::SkipLibraries`]
/// walk will take before giving up and stopping wherever it landed. A
/// normal library call resolves in a handful of steps; this only fires
/// on pathological runaways (and is logged when it does), so the
/// debugger never hangs.
const SKIP_LIB_STEP_BUDGET: usize = 4096;

/// Result of a step, if [`SignalInterrupt`] or [`WatchpointInterrupt`] then
/// a step process interrupted and the user should know about it.
/// If `quiet` set to `true` then no hooks should occur.
pub(super) enum StepResult {
    Done,
    SignalInterrupt {
        signal: Signal,
        quiet: bool,
    },
    WatchpointInterrupt {
        pid: Pid,
        addr: RelocatedAddress,
        ty: WatchpointHitType,
        quiet: bool,
    },
}

impl StepResult {
    fn signal_interrupt_quiet(signal: Signal) -> Self {
        Self::SignalInterrupt {
            signal,
            quiet: true,
        }
    }

    fn signal_interrupt(signal: Signal) -> Self {
        Self::SignalInterrupt {
            signal,
            quiet: false,
        }
    }

    fn wp_interrupt_quite(pid: Pid, addr: RelocatedAddress, ty: WatchpointHitType) -> Self {
        Self::WatchpointInterrupt {
            pid,
            addr,
            ty,
            quiet: true,
        }
    }

    fn wp_interrupt(pid: Pid, addr: RelocatedAddress, ty: WatchpointHitType) -> Self {
        Self::WatchpointInterrupt {
            pid,
            addr,
            ty,
            quiet: false,
        }
    }
}

impl Debugger {
    /// Do a single step (until debugee reaches a different source line).
    ///
    /// Returns [`StepResult::SignalInterrupt`] if the step is interrupted by a signal
    /// or [`StepResult::Done`] if a step is done.
    ///
    /// **! change exploration context**
    pub(super) fn step_in(&mut self) -> Result<StepResult, Error> {
        enum PlaceOrStop {
            Place(PlaceDescriptorOwned),
            Signal(Signal),
            Watchpoint(Pid, RelocatedAddress, WatchpointHitType),
        }

        // make an instruction step but ignoring functions prolog
        // initial function must exist (do instruction steps until it's not)
        // returns stop place or signal if a step is undone
        fn step_over_prolog(debugger: &mut Debugger) -> Result<PlaceOrStop, Error> {
            macro_rules! prolog_single_step {
                ($debugger: expr) => {
                    match $debugger.single_step_instruction()? {
                        Some(StopReason::SignalStop(_, sign)) => {
                            return Ok(PlaceOrStop::Signal(sign));
                        }
                        Some(StopReason::Watchpoint(pid, addr, ty)) => {
                            return Ok(PlaceOrStop::Watchpoint(pid, addr, ty));
                        }
                        _ => {}
                    }
                };
            }

            loop {
                // initial step
                prolog_single_step!(debugger);
                let ecx = debugger.ecx();
                let mut location = ecx.location();
                // determine current function, if no debug information for function - step until function found
                let func = loop {
                    let dwarf = debugger.debugee.debug_info(location.pc)?;
                    // step's stop only if there is debug information for PC and current function can be determined
                    if let Ok(Some((func, _))) = dwarf.find_function_by_pc(location.global_pc) {
                        break func;
                    }
                    prolog_single_step!(debugger);
                    let ecx = debugger.ecx();
                    location = ecx.location();
                };

                let prolog = func.prolog()?;
                // if PC in prolog range - step until function body is reached
                while debugger.ecx().location().global_pc.in_range(&prolog) {
                    prolog_single_step!(debugger);
                }

                let location = debugger.ecx().location();
                if let Some(place) = debugger
                    .debugee
                    .debug_info(location.pc)?
                    .find_exact_place_from_pc(location.global_pc)?
                {
                    return Ok(PlaceOrStop::Place(place.to_owned()));
                }
            }
        }

        let mut location = self.ecx().location();

        let start_place = loop {
            let dwarf = &self.debugee.debug_info(location.pc)?;
            if let Ok(Some(place)) = dwarf.find_place_from_pc(location.global_pc) {
                break place;
            }
            match self.single_step_instruction()? {
                Some(StopReason::SignalStop(_, sign)) => {
                    return Ok(StepResult::signal_interrupt(sign));
                }
                Some(StopReason::Watchpoint(pid, addr, ty)) => {
                    return Ok(StepResult::wp_interrupt(pid, addr, ty));
                }
                _ => {}
            }
            location = self.ecx().location();
        };

        let sp_file = start_place.file.to_path_buf();
        let sp_line = start_place.line_number;
        let start_cfa = self
            .debugee
            .debug_info(location.pc)?
            .get_cfa(&self.debugee, &ExplorationContext::new(location, 0))?;

        loop {
            let next_place = match step_over_prolog(self)? {
                PlaceOrStop::Place(place) => place,
                PlaceOrStop::Signal(signal) => return Ok(StepResult::signal_interrupt(signal)),
                PlaceOrStop::Watchpoint(pid, addr, dr) => {
                    return Ok(StepResult::wp_interrupt(pid, addr, dr));
                }
            };
            if !next_place.is_stmt {
                continue;
            }
            let in_same_place = sp_file == next_place.file && sp_line == next_place.line_number;
            let location = self.ecx().location();
            let next_cfa = self
                .debugee
                .debug_info(location.pc)?
                .get_cfa(&self.debugee, &ExplorationContext::new(location, 0))?;

            // step is done if:
            // 1) we may step at same place in code but in another stack frame
            // 2) we step at another place in code (file + line)
            if start_cfa != next_cfa || !in_same_place {
                break;
            }
        }

        self.ecx_update_location()?;
        Ok(StepResult::Done)
    }

    /// Move debugee to next instruction, step over breakpoint if needed.
    /// May return a [`StopReason::SignalStop`] if the step didn't happen cause signal.
    ///
    /// **! change exploration context**
    pub(super) fn single_step_instruction(&mut self) -> Result<Option<StopReason>, Error> {
        let loc = self.ecx().location();
        let mb_reason = if self.breakpoints.get_enabled(loc.pc).is_some() {
            self.step_over_breakpoint()?
        } else {
            self.debugee.bump_single_step_trap();
            let maybe_reason = self.debugee.tracer_mut().single_step(
                TraceContext::new(&self.breakpoints.active_breakpoints(), &self.watchpoints),
                loc.pid,
            )?;
            self.ecx_update_location()?;
            maybe_reason
        };
        Ok(mb_reason)
    }

    /// If current on focus thread is stopped at a breakpoint, then it takes a step through this point.
    ///
    /// May return a [`StopReason::SignalStop`] or [`StopReason::Watchpoint`]
    /// if the step didn't happen cause signal or watchpoint is hit.
    ///
    /// **! change exploration context**
    pub(super) fn step_over_breakpoint(&mut self) -> Result<Option<StopReason>, Error> {
        // cannot use debugee::Location mapping offset may be not init yet
        let tracee = self.debugee.get_tracee_ensure(self.ecx().pid_on_focus());
        let mb_brkpt = self.breakpoints.get_enabled(tracee.pc()?);
        let tracee_pid = tracee.pid;
        if let Some(brkpt) = mb_brkpt
            && brkpt.is_enabled()
        {
            brkpt.disable()?;
            self.debugee.bump_single_step_trap();
            let maybe_reason = self.debugee.tracer_mut().single_step(
                TraceContext::new(&self.breakpoints.active_breakpoints(), &self.watchpoints),
                tracee_pid,
            )?;
            brkpt.enable()?;
            self.ecx_update_location()?;
            return Ok(maybe_reason);
        }
        Ok(None)
    }

    /// Move to higher stack frame.
    ///
    /// **! change exploration context**
    pub(super) fn step_out_frame(&mut self) -> Result<(), Error> {
        let ecx = self.ecx();
        let location = ecx.location();
        let debug_info = self.debugee.debug_info(location.pc)?;

        if let Some(ret_addr) = self.debugee.return_addr(ecx.pid_on_focus())? {
            let brkpt_is_set = self.breakpoints.get_enabled(ret_addr).is_some();
            if brkpt_is_set {
                self.continue_execution()?;
            } else {
                let brkpt =
                    Breakpoint::new_temporary(debug_info.pathname(), ret_addr, location.pid);
                self.breakpoints.add_and_enable(brkpt)?;
                self.continue_execution()?;
                self.remove_breakpoint(Address::Relocated(ret_addr))?;
            }
        }

        if self.debugee.is_exited() {
            // todo add exit code here
            return Err(ProcessExit(0));
        }

        self.ecx_update_location()?;
        Ok(())
    }

    /// Do debugee step (over subroutine calls too).
    /// Returns [`StepResult::SignalInterrupt`] if the step is interrupted by a signal
    /// or [`StepResult::Done`] if step done.
    ///
    /// **! change exploration context**
    pub(super) fn step_over_any(&mut self) -> Result<StepResult, Error> {
        let ecx = self.ecx();
        let mut current_location = ecx.location();

        // determine current function, if no debug information for function - step until function found
        let (func, info) = loop {
            let dwarf = &self.debugee.debug_info(current_location.pc)?;
            // step's stop only if there is debug information for PC and current function can be determined
            if let Ok(Some((func, info))) = dwarf.find_function_by_pc(current_location.global_pc) {
                break (func, info);
            }
            match self.single_step_instruction()? {
                Some(StopReason::SignalStop(_, sign)) => {
                    return Ok(StepResult::signal_interrupt(sign));
                }
                Some(StopReason::Watchpoint(pid, addr, ty)) => {
                    return Ok(StepResult::wp_interrupt(pid, addr, ty));
                }
                _ => {}
            }
            current_location = self.ecx().location();
        };
        let fn_file = info.decl_file_line.map(|fl| fl.0);

        let prolog = func.prolog()?;
        let epilog_begin = func.epilog_begin()?;
        let dwarf = &self.debugee.debug_info(current_location.pc)?;
        let inline_ranges = func.inline_ranges();

        let mut step_over_breakpoints = vec![];
        let mut to_delete = vec![];

        let fn_full_name = info.full_name();
        for range in func.ranges() {
            let mut place = func
                .unit()
                .find_place_by_pc(GlobalAddress::from(range.begin))
                .ok_or_else(|| NoFunctionRanges(fn_full_name.clone()))?;

            while place.address.in_range(&range) {
                if Some(place.file_idx) != fn_file {
                    match place.next() {
                        None => break,
                        Some(n) => place = n,
                    }
                    continue;
                }

                // skip places in function prolog
                if place.address.in_range(&prolog) {
                    match place.next() {
                        None => break,
                        Some(n) => place = n,
                    }
                    continue;
                }

                // skip places in function epilog
                if let Some(eb) = epilog_begin.as_ref()
                    && place.address > eb.address
                {
                    match place.next() {
                        None => break,
                        Some(n) => place = n,
                    }
                    continue;
                }

                // Guard against a step landing inside an inlined
                // function body — but only the *interior* of the
                // body. The first PC of an inline range is the call
                // site itself; that's a legitimate step boundary
                // and we want a BP there. dsymutil on darwin tends
                // to emit inline ranges starting *at* the call-site
                // PC (rustc on linux often emits them starting one
                // instruction higher), so the naive
                // `addr >= begin && addr < end` test wrongly
                // excludes the call site on darwin and a step over
                // a `for` loop body that contains an inlined call
                // (`Iterator::next`, `BTreeMap::insert`, …) jumps
                // straight past the loop. Only skip when strictly
                // inside the body.
                let in_inline_interior = inline_ranges.iter().any(|r| {
                    u64::from(place.address) > r.begin && u64::from(place.address) < r.end
                });

                if !in_inline_interior && place.is_stmt {
                    let load_addr = place
                        .address
                        .relocate_to_segment_by_pc(&self.debugee, current_location.pc)?;
                    if self.breakpoints.get_enabled(load_addr).is_none() {
                        step_over_breakpoints.push(load_addr);
                        to_delete.push(load_addr);
                    }
                }

                match place.next() {
                    None => break,
                    Some(n) => place = n,
                }
            }
        }

        step_over_breakpoints
            .into_iter()
            .try_for_each(|load_addr| {
                self.breakpoints
                    .add_and_enable(Breakpoint::new_temporary(
                        dwarf.pathname(),
                        load_addr,
                        current_location.pid,
                    ))
                    .map(|_| ())
            })?;

        let return_addr = self.debugee.return_addr(current_location.pid)?;
        if let Some(ret_addr) = return_addr
            && self.breakpoints.get_enabled(ret_addr).is_none()
        {
            self.breakpoints.add_and_enable(Breakpoint::new_temporary(
                dwarf.pathname(),
                ret_addr,
                current_location.pid,
            ))?;
            to_delete.push(ret_addr);
        }

        let stop_reason = self.continue_execution()?;

        to_delete
            .into_iter()
            .try_for_each(|addr| self.remove_breakpoint(Address::Relocated(addr)).map(|_| ()))?;

        // hooks already called at [`Self::continue_execution`], so use `quite` opt
        match stop_reason {
            StopReason::SignalStop(_, sign) => {
                return Ok(StepResult::signal_interrupt_quiet(sign));
            }
            StopReason::Watchpoint(pid, addr, ty) => {
                return Ok(StepResult::wp_interrupt_quite(pid, addr, ty));
            }
            _ => {}
        }

        // if a step is taken outside and new location pc not equals to place pc,
        // then we stopped at the place of the previous function call,
        // and got into an assignment operation or similar in this case do a single step
        let new_location = self.ecx().location();
        if Some(new_location.pc) == return_addr {
            let place = self
                .debugee
                .debug_info(new_location.pc)?
                .find_place_from_pc(new_location.global_pc)?
                .ok_or_else(|| NoFunctionRanges(fn_full_name))?;
            if place.address != new_location.global_pc {
                match self.step_in()? {
                    StepResult::SignalInterrupt { signal, .. } => {
                        return Ok(StepResult::signal_interrupt(signal));
                    }
                    StepResult::WatchpointInterrupt { pid, addr, ty, .. } => {
                        return Ok(StepResult::wp_interrupt(pid, addr, ty));
                    }
                    _ => {}
                }
            }
        }

        if self.debugee.is_exited() {
            // todo add exit code here
            return Err(ProcessExit(0));
        }

        self.ecx_update_location()?;
        Ok(StepResult::Done)
    }

    /// Classify the frame the focus thread is currently stopped in as
    /// user code or library code, by resolving the PC to a source path.
    /// A PC with no debug info / no function is [`FrameKind::Library`]
    /// (nothing to show there).
    ///
    /// Classification uses the *real* frame function's own defining file
    /// (its first range's place), **not** `find_place_from_pc` — the PC
    /// may sit on an inlined library call (`iter().map(…)`,
    /// `BTreeMap::insert`, …) whose innermost attribution is core/alloc
    /// even though the executing frame is the user's function. We want to
    /// classify the frame, so that stepping over an inlined library call
    /// inside a user line still stops on that user line.
    ///
    /// **! does not change exploration context**
    pub(super) fn current_frame_kind(&self) -> FrameKind {
        let loc = self.ecx().location();
        let Ok(dwarf) = self.debugee.debug_info(loc.pc) else {
            return FrameKind::Library;
        };
        if let Ok(Some((func, _))) = dwarf.find_function_by_pc(loc.global_pc)
            && let Some(range) = func.ranges().first()
            && let Some(place) = func
                .unit()
                .find_place_by_pc(GlobalAddress::from(range.begin))
        {
            return classify_source_path(Some(place.file));
        }
        // No function / no place — a PLT stub, stripped or FFI frame.
        FrameKind::Library
    }

    /// Step-In that steps *through* library frames and stops at the next
    /// line of user code ("just my code").
    ///
    /// Walk: repeatedly [`Self::step_in`]; classify where we land.
    /// - **User code at a new source position** → stop. Covers both
    ///   stepping *into* a user function the line called (`helper(x)`)
    ///   and *advancing* to the next user line (an all-library line).
    /// - **Library frame** → skip it: climb back out with
    ///   [`Self::step_out_frame`] to the user caller. Then, crucially,
    ///   only stop if that returned us to a *new* source line; if we're
    ///   still on the *same* line we keep walking, because the rest of
    ///   the line may hold another call — and the next one might be
    ///   **your** code (e.g. `helper(&v)` does a `Vec`→`&[T]` deref, a
    ///   library call, *then* calls the user `helper`). Stepping over the
    ///   whole line here would wrongly skip `helper`.
    ///
    /// So the engine steps *over* library calls but *into* user calls, in
    /// execution order, and never strands you on the start line doing
    /// nothing.
    ///
    /// MVP gap: a library that invokes a *user closure* (e.g.
    /// `iter().map(user_fn)`) is stepped over, not stopped in — the
    /// callback runs to completion inside the `step_out_frame` that skips
    /// the iterator. The phase-4 engine closes this; see
    /// `doc/plans/phase-12-step-into-just-my-code.md`.
    ///
    /// **! change exploration context**
    pub(super) fn step_in_skip_libraries(&mut self) -> Result<StepResult, Error> {
        // The source line we started on. A step has made user-visible
        // progress once we're in user code at a *different* `(file, line)`.
        let start_place = self.current_source_place();

        for _ in 0..SKIP_LIB_STEP_BUDGET {
            // A `step_in` can fail *inside* foreign code we can't
            // introspect — a stripped system dylib (no DWARF →
            // `NoDebugInformation`) or a frame the unwinder can't read
            // (`DwarfParsing`). That's not a real failure: treat it like
            // landing in library and climb back out to the user frame.
            let entered_foreign = match self.step_in() {
                Ok(StepResult::Done) => false,
                Ok(other) => return Ok(other),
                Err(e) if is_recoverable_step_error(&e) => {
                    // Resync the exploration context to the real PC the
                    // failed step left us at before we try to climb.
                    let _ = self.ecx_update_location();
                    true
                }
                Err(e) => return Err(e),
            };
            if self.debugee.is_exited() {
                return Err(ProcessExit(0));
            }

            if entered_foreign || self.current_frame_kind() == FrameKind::Library {
                // Skip this library call: climb back out to user code.
                let mut reached_user = false;
                for _ in 0..SKIP_LIB_STEP_BUDGET {
                    let before = self.ecx().location().pc;
                    // `step_out_frame` can itself hit an un-unwindable
                    // frame; tolerate that and stop climbing.
                    if self.step_out_frame().is_err() {
                        break;
                    }
                    if self.debugee.is_exited() {
                        return Err(ProcessExit(0));
                    }
                    if self.current_frame_kind() == FrameKind::UserCode {
                        reached_user = true;
                        break;
                    }
                    // No return address to unwind to (top of stack): we've
                    // run out of frames to climb. Stop trying.
                    if self.ecx().location().pc == before {
                        break;
                    }
                }
                if !reached_user {
                    // No user frame left to return to — we've stepped past
                    // the end of all user code (typically off the end of
                    // `main` into the C runtime). A step here should behave
                    // like run-to-completion: continue to the next
                    // breakpoint or program exit, rather than stranding the
                    // user in unreadable runtime or erroring on it.
                    return self.continue_to_stop();
                }
            }

            // Now in a user *frame*. Stop only when the displayed source
            // line is itself user code (not an inlined library line such
            // as a `Box`/`Vec` method spliced into a user function — the
            // frame is the user's, but the editor would show `boxed.rs`)
            // *and* it differs from where we began. Otherwise — inlined
            // library line, or still on the start line — keep walking.
            let place = self.current_source_place();
            let place_is_user = matches!(
                classify_source_path(place.as_ref().map(|(f, _)| f.as_path())),
                FrameKind::UserCode
            );
            if place_is_user && place != start_place {
                return Ok(StepResult::Done);
            }
        }

        log::warn!(
            "step-into (skip libraries): step budget ({SKIP_LIB_STEP_BUDGET}) \
             exhausted, stopping in place"
        );
        Ok(StepResult::Done)
    }

    /// Run the debuggee until the next stop (breakpoint / watchpoint /
    /// signal) or exit, mapping the outcome to a [`StepResult`]. Used as
    /// the graceful fallback when a "skip libraries" step runs off the
    /// end of user code: there is nothing left to step *to*, so the step
    /// degrades into a continue (this is what GDB/LLDB do when you step
    /// off the end of `main`). Hooks already fire inside
    /// `continue_execution`, so the signal/watchpoint variants are quiet.
    fn continue_to_stop(&mut self) -> Result<StepResult, Error> {
        let stop = self.continue_execution()?;
        if self.debugee.is_exited() {
            return Err(ProcessExit(0));
        }
        match stop {
            StopReason::DebugeeExit(code) => Err(ProcessExit(code)),
            StopReason::SignalStop(_, sign) => Ok(StepResult::signal_interrupt_quiet(sign)),
            StopReason::Watchpoint(pid, addr, ty) => {
                Ok(StepResult::wp_interrupt_quite(pid, addr, ty))
            }
            _ => Ok(StepResult::Done),
        }
    }

    /// `(file, line)` of the source place the focus thread is currently
    /// stopped at, or `None` if the PC has no place. Used to detect a
    /// source-line change across a step.
    pub(super) fn current_source_place(&self) -> Option<(std::path::PathBuf, u64)> {
        let loc = self.ecx().location();
        let dwarf = self.debugee.debug_info(loc.pc).ok()?;
        let place = dwarf.find_place_from_pc(loc.global_pc).ok().flatten()?;
        Some((place.file.to_path_buf(), place.line_number))
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameKind, classify_source_path};
    use std::path::Path;

    fn classify(p: &str) -> FrameKind {
        classify_source_path(Some(Path::new(p)))
    }

    #[test]
    fn user_crate_paths_are_user_code() {
        assert_eq!(classify("/home/me/proj/src/main.rs"), FrameKind::UserCode);
        assert_eq!(
            classify("/Users/me/git/app/crates/core/src/lib.rs"),
            FrameKind::UserCode
        );
        // A workspace member literally named "registry" must not be
        // mistaken for the cargo registry — the fragment is `/registry/`
        // mid-path, not a bare component the user might choose.
        assert_eq!(
            classify("/home/me/registry-cli/src/main.rs"),
            FrameKind::UserCode
        );
    }

    #[test]
    fn cargo_dep_paths_are_library() {
        assert_eq!(
            classify("/home/me/.cargo/registry/src/index.crates.io-abc/serde-1.0/src/lib.rs"),
            FrameKind::Library
        );
        // `/registry/` alone (e.g. a vendored deps dir) is enough.
        assert_eq!(
            classify("/opt/vendor/registry/foo-2.0/src/lib.rs"),
            FrameKind::Library
        );
    }

    #[test]
    fn toolchain_and_std_paths_are_library() {
        assert_eq!(
            classify(
                "/home/me/.rustup/toolchains/stable-aarch64/lib/rustlib/src/rust/library/core/src/iter/mod.rs"
            ),
            FrameKind::Library
        );
        // rustc-embedded std source prefix (`/rustc/<hash>/library/...`).
        assert_eq!(
            classify("/rustc/abc123/library/alloc/src/vec/mod.rs"),
            FrameKind::Library
        );
    }

    #[test]
    fn no_line_info_is_library() {
        // PLT stubs / stripped / FFI frames carry no place — nothing to
        // show, so treat as library and step through.
        assert_eq!(classify_source_path(None), FrameKind::Library);
    }
}

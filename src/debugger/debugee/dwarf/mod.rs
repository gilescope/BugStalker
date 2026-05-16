// SPDX-License-Identifier: MIT
pub mod eval;
mod loader;
mod location;
mod symbol;
pub mod r#type;
pub mod unit;
pub mod unwind;
mod utils;

pub use self::unwind::DwarfUnwinder;

use crate::debugger::ExplorationContext;
use crate::debugger::address::{GlobalAddress, RelocatedAddress};
use crate::debugger::context::gcx;
use crate::debugger::debugee::dwarf::eval::AddressKind;
use crate::debugger::debugee::dwarf::symbol::{SymbolTab, TlsSymbolTab};
use crate::debugger::debugee::dwarf::unit::die::{DerefContext, Die};
use crate::debugger::debugee::dwarf::unit::die_ref::{FatDieRef, Function, Variable};
use crate::debugger::debugee::dwarf::unit::{
    BsUnit, DwarfUnitParser, FunctionInfo, PlaceDescriptorOwned,
};
use crate::debugger::debugee::dwarf::utils::PathSearchIndex;
use crate::debugger::debugee::{Debugee, Location};
use crate::debugger::error::Error;
use crate::debugger::error::Error::{DebugIDFormat, UnitNotFound};
use crate::debugger::register::{DwarfRegisterMap, RegisterMap};
use crate::{muted_error, resolve_unit_call, version_switch, weak_error};
use gimli::CfaRule::RegisterAndOffset;
use gimli::{
    BaseAddresses, CfaRule, DebugAddr, DebugFrame, DebugInfoOffset, DebugPubTypes, Dwarf, EhFrame,
    LocationLists, Range, Reader, RunTimeEndian, Section, UnitOffset, UnwindContext, UnwindSection,
    UnwindTableRow,
};
use indexmap::IndexMap;
use log::debug;
use memmap2::Mmap;
use object::{Object, ObjectSection};
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::ops::Add;
use std::path::{Path, PathBuf};
use std::{fs, path};
pub use symbol::Symbol;
use trie_rs::Trie;
use unit::PlaceDescriptor;
use walkdir::WalkDir;

pub type EndianArcSlice = gimli::EndianArcSlice<gimli::RunTimeEndian>;

pub struct DebugInformation<R: gimli::Reader = EndianArcSlice> {
    file: PathBuf,
    inner: Dwarf<R>,
    eh_frame: EhFrame<R>,
    debug_frame: Option<DebugFrame<R>>,
    bases: BaseAddresses,
    units: Option<Vec<BsUnit>>,
    symbol_table: Option<SymbolTab>,
    tls_symbol_tab: Option<TlsSymbolTab>,
    pub_names: Option<Trie<u8>>,
    pub_types: HashMap<String, (DebugInfoOffset, UnitOffset)>,
    /// Index for fast search files by full path or part of file path. Contains unit index and
    /// indexes of lines in [`Unit::lines`] vector that belongs to a file, indexes are ordered by
    /// line number, column number and address.
    files_index: PathSearchIndex<(usize, Vec<usize>)>,
    /// Canonical PC → function-frame walker, courtesy of the
    /// `addr2line` crate. Built lazily on first use because not every
    /// load needs it. Same author / repo as gimli, kept in lockstep
    /// with the pinned gimli version; we reuse it rather than
    /// reimplementing inline-frame chain walking. See
    /// `find_inline_chain`.
    ///
    /// Wrapped in `Mutex` because `addr2line::Context` is `!Sync`:
    /// the surrounding `DebugInformation` must stay `Sync` to keep
    /// the existing rayon `par_iter` chains working. Lock contention
    /// is a non-issue — chain lookups happen on user-visible events
    /// (breakpoint hits, step responses), not in hot loops.
    addr2_ctx: once_cell::sync::OnceCell<std::sync::Mutex<addr2line::Context<EndianArcSlice>>>,
    /// macOS Mach-O `__unwind_info` section bytes. Empty on Linux /
    /// ELF or when the binary has no compact-unwind section. Parsed
    /// lazily on each query (zero-copy parser; the per-query cost is
    /// just header reads). See `compact_cfa_at`.
    compact_unwind_bytes: Option<std::sync::Arc<Vec<u8>>>,
}

impl Clone for DebugInformation {
    fn clone(&self) -> Self {
        Self {
            file: self.file.clone(),
            inner: Dwarf {
                debug_abbrev: self.inner.debug_abbrev.clone(),
                debug_addr: self.inner.debug_addr.clone(),
                debug_aranges: self.inner.debug_aranges.clone(),
                debug_info: self.inner.debug_info.clone(),
                debug_line: self.inner.debug_line.clone(),
                debug_line_str: self.inner.debug_line_str.clone(),
                debug_macro: self.inner.debug_macro.clone(),
                debug_macinfo: self.inner.debug_macinfo.clone(),
                debug_names: self.inner.debug_names.clone(),
                debug_str: self.inner.debug_str.clone(),
                debug_str_offsets: self.inner.debug_str_offsets.clone(),
                debug_types: self.inner.debug_types.clone(),
                locations: self.inner.locations.clone(),
                ranges: self.inner.ranges.clone(),
                file_type: self.inner.file_type,
                sup: self.inner.sup.clone(),
                abbreviations_cache: Default::default(),
            },
            eh_frame: self.eh_frame.clone(),
            debug_frame: self.debug_frame.clone(),
            bases: self.bases.clone(),
            units: self
                .units
                .as_ref()
                .map(|units| units.iter().map(|u| u.clone(self.dwarf())).collect()),
            symbol_table: self.symbol_table.clone(),
            tls_symbol_tab: self.tls_symbol_tab.clone(),
            // it is ok cause pub_names currently unused, maybe it will be changed in future
            pub_names: None,
            pub_types: self.pub_types.clone(),
            files_index: self.files_index.clone(),
            // Don't transfer the addr2line context across clones —
            // it'll be rebuilt lazily on first use against the cloned
            // Dwarf sections.
            addr2_ctx: once_cell::sync::OnceCell::new(),
            compact_unwind_bytes: self.compact_unwind_bytes.clone(),
        }
    }
}

/// Using this macro means a promise that debug information exists in context of usage.
#[macro_export]
macro_rules! debug_info_exists {
    ($expr: expr) => {
        $expr.expect("unreachable: debug information must exists")
    };
}

impl DebugInformation {
    /// Return path to executable file with (possible) debug information.
    /// In case of executable contains debug information in separate file this file may not have
    /// a debug information but contains a link to it.
    pub fn pathname(&self) -> &Path {
        self.file.as_path()
    }

    /// The location lists in the .debug_loc and .debug_loclists sections.
    pub fn locations(&self) -> &LocationLists<EndianArcSlice> {
        &self.inner.locations
    }

    /// Return all dwarf units or error if no debug information found.
    fn get_units(&self) -> Result<&[BsUnit], Error> {
        self.units
            .as_deref()
            .ok_or(Error::NoDebugInformation("file"))
    }

    /// Return false if file dont contains a debug information.
    pub fn has_debug_info(&self) -> bool {
        self.units.is_some()
    }

    /// Return unit by its index.
    ///
    /// # Arguments
    ///
    /// * `idx`: unit index
    ///
    /// # Panics
    ///
    /// Panic if unit not found.
    pub fn unit_ensure(&self, idx: usize) -> &BsUnit {
        &debug_info_exists!(self.get_units())[idx]
    }

    /// Return unit count. Return 0 if no debug information exists.
    #[inline(always)]
    pub fn unit_count(&self) -> usize {
        self.units
            .as_ref()
            .map(|units| units.len())
            .unwrap_or_default()
    }

    /// Return `Some(true)` if .debug_pubnames section contains template last part (for example
    /// this may be a function name), `Some(false)` if not contains and `None` if no .debug_pubnames
    /// section in debug information file.
    ///
    /// This function is useful, for example, to determine the presence of a function in a file. The
    /// result is false positive, means that if result is `Some(false)` than function not exists, but
    /// it may exists or not exists if result is `None` (we need analyze die's for determine).
    ///
    /// # Arguments
    ///
    /// * `tpl`: template for object or function name.
    pub fn tpl_in_pub_names(&self, tpl: &str) -> Option<bool> {
        debug_assert!(tpl.split("::").count() > 0);
        let needle = tpl.split("::").last().expect("at least one exists");
        self.pub_names.as_ref().map(|pub_names| {
            // trie-rs 0.4: predictive_search returns an iterator; we
            // only need to know whether any match exists.
            pub_names
                .predictive_search::<Vec<u8>, _>(needle)
                .next()
                .is_some()
        })
    }

    fn evaluate_cfa(
        &self,
        debugee: &Debugee,
        registers: &DwarfRegisterMap,
        utr: &UnwindTableRow<usize>,
        ecx: &ExplorationContext,
    ) -> Result<RelocatedAddress, Error> {
        let rule = utr.cfa();
        match rule {
            RegisterAndOffset { register, offset } => {
                let ra = registers.value(*register)?;
                Ok(RelocatedAddress::from(ra as usize).offset(*offset as isize))
            }
            CfaRule::Expression(expr) => {
                let unit = debug_info_exists!(self.find_unit_by_pc(ecx.location().global_pc))
                    .ok_or(UnitNotFound(ecx.location().global_pc))?;
                let evaluator =
                    resolve_unit_call!(&self.inner, unit, evaluator, debugee, self.dwarf());
                let expr_result = evaluator.evaluate(ecx, expr.get(&self.eh_frame)?)?;

                Ok((expr_result.into_scalar::<usize>(AddressKind::Value)?).into())
            }
        }
    }

    pub fn get_cfa(
        &self,
        debugee: &Debugee,
        ecx: &ExplorationContext,
    ) -> Result<RelocatedAddress, Error> {
        let mut ucx = Box::new(UnwindContext::new());
        let global_pc = ecx.location().global_pc;
        let pid = ecx.pid_on_focus();
        match self.eh_frame.unwind_info_for_address(
            &self.bases,
            &mut ucx,
            global_pc.into(),
            EhFrame::cie_from_offset,
        ) {
            Ok(row) => self.evaluate_cfa(
                debugee,
                &DwarfRegisterMap::from(RegisterMap::current(pid)?),
                row,
                ecx,
            ),
            Err(gimli::Error::NoUnwindInfoForAddress) => {
                // macOS arm64 falls back to compact unwind for the
                // majority of functions. Without this fallback,
                // computing CFA / frame_base for any non-eh_frame-
                // covered function would fail and downstream variable
                // reads would return garbage.
                if let Some(cfa) = self.compact_cfa_at(global_pc, pid)? {
                    return Ok(cfa);
                }
                Err(Error::from(gimli::Error::NoUnwindInfoForAddress))
            }
            Err(e) => Err(Error::from(e)),
        }
    }

    /// Compute the canonical frame address (CFA) at `pc` using
    /// Mach-O `__compact_unwind` data when present. Returns `Ok(None)`
    /// when the binary has no compact-unwind section, when the lookup
    /// misses, or when the encoding asks us to fall through to a
    /// `__eh_frame` FDE (in which case the caller should already have
    /// resolved that path).
    ///
    /// The compact-unwind ARM64 encoding tells us directly how the
    /// frame is laid out at a PC:
    /// * `FrameBased` — standard `[fp, lr]` pair on the stack; CFA is
    ///   `old_fp + 16`.
    /// * `Frameless` — no frame pointer; CFA is `sp + stack_size`.
    /// * `Dwarf { eh_frame_fde }` — defer to the FDE at that offset.
    /// * `Null` / unrecognised — no info available.
    pub fn compact_cfa_at(
        &self,
        pc: GlobalAddress,
        pid: nix::unistd::Pid,
    ) -> Result<Option<RelocatedAddress>, Error> {
        let Some(bytes) = self.compact_unwind_bytes.as_ref() else {
            return Ok(None);
        };
        let info = match macho_unwind_info::UnwindInfo::parse(bytes.as_ref()) {
            Ok(i) => i,
            Err(err) => {
                log::warn!(target: "debugger", "compact unwind parse error: {err}");
                return Ok(None);
            }
        };
        let probe: u64 = pc.into();
        let probe_u32 = match u32::try_from(probe) {
            Ok(v) => v,
            // Compact unwind keys are u32; PCs beyond 4 GiB into the
            // image aren't representable. Fall through to "no info".
            Err(_) => return Ok(None),
        };
        let function = match info.lookup(probe_u32) {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(None),
            Err(err) => {
                log::debug!(
                    target: "debugger",
                    "compact unwind lookup at {probe:#x} failed: {err}"
                );
                return Ok(None);
            }
        };
        use macho_unwind_info::opcodes::OpcodeArm64;
        let opcode = OpcodeArm64::parse(function.opcode);
        let regs = DwarfRegisterMap::from(RegisterMap::current(pid)?);
        // arm64 DWARF register numbers: x0..x30 -> 0..30, SP -> 31,
        // x29 (FP) -> 29. Same numbering gimli uses.
        const FP: gimli::Register = gimli::Register(29);
        const SP: gimli::Register = gimli::Register(31);
        let cfa: u64 = match opcode {
            OpcodeArm64::FrameBased { .. } => {
                let fp = regs.value(FP)?;
                fp.saturating_add(16)
            }
            OpcodeArm64::Frameless {
                stack_size_in_bytes,
            } => {
                let sp = regs.value(SP)?;
                sp.saturating_add(stack_size_in_bytes as u64)
            }
            OpcodeArm64::Dwarf { .. } | OpcodeArm64::Null | OpcodeArm64::UnrecognizedKind(_) => {
                return Ok(None);
            }
        };
        Ok(Some(RelocatedAddress::from(cfa as usize)))
    }

    pub fn debug_addr(&self) -> &DebugAddr<EndianArcSlice> {
        &self.inner.debug_addr
    }

    /// Return a list of all known files.
    pub fn known_files(&self) -> Result<impl Iterator<Item = &PathBuf>, Error> {
        Ok(self.get_units()?.iter().flat_map(|unit| unit.files()))
    }

    /// Searches for a unit by occurrences of PC in its range.
    ///
    /// # Arguments
    ///
    /// * `pc`: program counter value
    ///
    /// returns: `None` if unit not found, error if no debug information found
    ///
    /// **Note on darwin:** dsymutil emits per-CU `DW_AT_low_pc` /
    /// `DW_AT_high_pc` covering the full enclosing range even when
    /// the CU's actual code is non-contiguous (and the *between*
    /// addresses belong to other CUs). The naive "first match" walk
    /// then picks the CU with the widest claim — typically a
    /// generic instantiation CU whose range engulfs unrelated code
    /// — and the line lookup ends up in some other source file
    /// (e.g. `alloc/sync.rs:2226`). To stay correct we collect all
    /// candidate CUs and prefer the one whose own line table has
    /// an entry exactly at `pc`; that's a unit which actually
    /// generated this instruction, not just one that happens to
    /// span it. Falls back to the tightest range otherwise.
    fn find_unit_by_pc(&self, pc: GlobalAddress) -> Result<Option<&BsUnit>, Error> {
        let pc_u = u64::from(pc);
        let mut candidates: Vec<&BsUnit> = Vec::new();
        for unit in self.get_units()?.iter() {
            let in_range = match unit.ranges().binary_search_by_key(&pc_u, |r| r.begin) {
                Ok(_) => true,
                Err(pos) => unit.ranges()[..pos]
                    .iter()
                    .rev()
                    .any(|range| pc.in_range(range)),
            };
            if in_range {
                candidates.push(unit);
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        if candidates.len() == 1 {
            return Ok(Some(candidates[0]));
        }
        // Prefer a unit whose own line program has an exact entry at pc.
        if let Some(exact) = candidates
            .iter()
            .find(|u| u.find_exact_place_by_pc(pc).is_some())
        {
            return Ok(Some(*exact));
        }
        // Otherwise pick the unit with the tightest enclosing range —
        // the smallest `(end - begin)` containing `pc`. dsymutil's
        // CU-spanning ranges lose this contest to a real per-function
        // range every time.
        let mut best: Option<(&BsUnit, u64)> = None;
        for unit in &candidates {
            let mut tightest: Option<u64> = None;
            for range in unit.ranges() {
                if pc_u >= range.begin && pc_u < range.end {
                    let span = range.end - range.begin;
                    if tightest.is_none_or(|t| span < t) {
                        tightest = Some(span);
                    }
                }
            }
            if let Some(span) = tightest
                && best.is_none_or(|(_, b)| span < b)
            {
                best = Some((unit, span));
            }
        }
        Ok(best.map(|(u, _)| u).or_else(|| candidates.first().copied()))
    }

    /// Returns best matched place by program counter global address.
    pub fn find_place_from_pc(
        &self,
        pc: GlobalAddress,
    ) -> Result<Option<PlaceDescriptor<'_>>, Error> {
        let mb_unit = self.find_unit_by_pc(pc)?;
        Ok(mb_unit.and_then(|u| u.find_place_by_pc(pc)))
    }

    /// Returns first place with line address equals to program counter global address.
    pub fn find_exact_place_from_pc(
        &self,
        pc: GlobalAddress,
    ) -> Result<Option<PlaceDescriptor<'_>>, Error> {
        let mb_unit = self.find_unit_by_pc(pc)?;
        Ok(mb_unit.and_then(|u| u.find_exact_place_by_pc(pc)))
    }

    /// Lazy accessor for the addr2line context built over our gimli
    /// `Dwarf`. addr2line owns the canonical PC → function-and-inline-
    /// chain walker; reimplementing it would be silly. Built once per
    /// `DebugInformation` and cached.
    fn addr2line_ctx(&self) -> &std::sync::Mutex<addr2line::Context<EndianArcSlice>> {
        self.addr2_ctx.get_or_init(|| {
            // `gimli::Dwarf` is not `Clone`; rebuild the wrapper by
            // cloning each Arc-wrapped section (cheap). Same shape
            // the manual `Clone for DebugInformation` impl uses
            // above — keep them in lockstep.
            let dwarf = clone_dwarf(&self.inner);
            let ctx = addr2line::Context::from_dwarf(dwarf).unwrap_or_else(|err| {
                log::warn!(
                    target: "dwarf-loader",
                    "addr2line::Context::from_dwarf failed for {:?}: {err}; \
                     inline-frame chain will be unavailable",
                    self.file,
                );
                // Return an addr2line context over an empty Dwarf
                // so callers get empty chains instead of crashes.
                addr2line::Context::from_dwarf(empty_dwarf()).expect("empty Dwarf always builds")
            });
            std::sync::Mutex::new(ctx)
        })
    }

    /// Return the inline-call chain at `pc`, innermost first.
    ///
    /// At a PC inside inlined code, addr2line returns:
    /// * frame `[0]` — the innermost `DW_TAG_inlined_subroutine` if
    ///   any, otherwise the concrete enclosing `DW_TAG_subprogram`,
    /// * frame `[1..n-1]` — successive outer inlined frames,
    /// * frame `[n-1]` — the concrete enclosing subprogram.
    ///
    /// This is the algorithm LLDB / llvm-symbolizer / `cargo flamegraph`
    /// all use; see the prior-art memo in the v0.4.x release notes.
    /// Returns an empty vec when no debug info covers the PC.
    pub fn find_inline_chain(&self, pc: GlobalAddress) -> Vec<InlineFrame> {
        let mutex = self.addr2line_ctx();
        let ctx = match mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let probe = u64::from(pc);
        let frame_iter = match ctx.find_frames(probe).skip_all_loads() {
            Ok(it) => it,
            Err(err) => {
                log::debug!(
                    target: "debugger",
                    "addr2line find_frames({probe:#x}) failed: {err}"
                );
                return vec![];
            }
        };
        let mut frames = vec![];
        let mut iter = frame_iter;
        loop {
            match iter.next() {
                Ok(Some(frame)) => {
                    let function = frame
                        .function
                        .as_ref()
                        .and_then(|f| f.demangle().ok().map(|c| c.into_owned()));
                    let (file, line, column) = match &frame.location {
                        Some(loc) => (
                            loc.file.map(|s| s.to_string()),
                            loc.line.map(|l| l as u64),
                            loc.column.map(|c| c as u64),
                        ),
                        None => (None, None, None),
                    };
                    frames.push(InlineFrame {
                        function,
                        file,
                        line,
                        column,
                    });
                }
                Ok(None) => break,
                Err(err) => {
                    log::debug!(
                        target: "debugger",
                        "addr2line frame iter at {probe:#x}: {err}"
                    );
                    break;
                }
            }
        }
        frames
    }

    /// Return a function inside which the given instruction is located.
    ///
    /// # Arguments
    ///
    /// * `pc`: instruction global address.
    pub fn find_function_by_pc(
        &'_ self,
        pc: GlobalAddress,
    ) -> Result<Option<(FatDieRef<'_, Function>, &'_ FunctionInfo)>, Error> {
        let mb_unit = self.find_unit_by_pc(pc)?;
        Ok(mb_unit.and_then(|unit| {
            let pc = u64::from(pc);
            let die_ranges = resolve_unit_call!(self.dwarf(), unit, fn_ranges);
            let find_pos = match die_ranges.binary_search_by_key(&pc, |dr| dr.range.begin) {
                Ok(pos) => {
                    let mut idx = pos + 1;
                    while idx < die_ranges.len() && die_ranges[idx].range.begin == pc {
                        idx += 1;
                    }
                    idx
                }
                Err(pos) => pos,
            };

            die_ranges[..find_pos].iter().rev().find_map(|dr| {
                let mb_fn_info = resolve_unit_call!(&self.inner, unit, fn_info, dr.die_off);

                if let Some(fn_info) = mb_fn_info
                    && dr.range.begin <= pc
                    && pc < dr.range.end
                {
                    return Some((FatDieRef::new_func(self, unit.idx(), dr.die_off), fn_info));
                };
                None
            })
        }))
    }

    /// Return a functions relevant to template.
    ///
    /// # Arguments
    ///
    /// * `template`: search template (full function path or part of this path).
    pub fn search_functions(
        &self,
        template: &str,
    ) -> Result<Vec<(FatDieRef<'_, Function>, &FunctionInfo)>, Error> {
        let units = self.get_units()?;
        let result: Vec<_> = units
            .par_iter()
            .flat_map(|unit| {
                let fn_infos = resolve_unit_call!(self.dwarf(), unit, search_functions, template);
                fn_infos
                    .into_iter()
                    .map(|(offset, info)| (FatDieRef::new_func(self, unit.idx(), offset), info))
                    .collect::<Vec<_>>()
            })
            .collect();

        Ok(result)
    }

    /// Return closest [`PlaceDescriptor`] for given file and line.
    /// Closest means that returns descriptor for target line or, if no descriptor for target line,
    /// place for next line after target.
    ///
    /// # Arguments
    ///
    /// * `file`: file name template (full path or part of a file path)
    /// * `line`: line number
    pub fn find_closest_place(
        &self,
        file_tpl: &str,
        line: u64,
    ) -> Result<Vec<PlaceDescriptor<'_>>, Error> {
        let (places, _diag) = self.find_closest_place_inner(file_tpl, line, false)?;
        Ok(places)
    }

    /// Same as `find_closest_place` but also reports every candidate
    /// it considered and why it was kept or dropped. Used by the
    /// structured `break.set` so agents see the disambiguation.
    pub fn find_closest_place_with_diagnostics(
        &self,
        file_tpl: &str,
        line: u64,
    ) -> Result<(Vec<PlaceDescriptor<'_>>, LineDiagnostics), Error> {
        self.find_closest_place_inner(file_tpl, line, true)
    }

    fn find_closest_place_inner(
        &self,
        file_tpl: &str,
        line: u64,
        record_diagnostics: bool,
    ) -> Result<(Vec<PlaceDescriptor<'_>>, LineDiagnostics), Error> {
        let files = self.files_index.get(file_tpl);

        #[derive(PartialEq, Hash, Eq)]
        struct Key {
            name: Option<String>,
            range: Box<[Range]>,
        }

        let mut unique_subprograms = HashSet::new();
        let mut diagnostics = LineDiagnostics::default();
        // Phase 9 follow-up — `result` holds the canonical matches:
        // line entries whose enclosing subprogram was *declared* in the
        // requested source file. `inline_fallback` holds matches where
        // the line is only present as an inlined / monomorphized copy
        // in some other function (e.g. main.rs:122 inside an inlined
        // chunk of `HashMap::insert`). The two are kept separate so we
        // can prefer canonical entries when both exist, but still fall
        // back to inlined matches when canonical entries are missing
        // (which happens when the line was optimised out of its source
        // function entirely).
        let mut result: Vec<PlaceDescriptor> = vec![];
        let mut inline_fallback: Vec<PlaceDescriptor> = vec![];

        let mut next_statement_line: Option<u64> = None;
        for (unit_idx, file_lines) in &files {
            let unit = self.unit_ensure(*unit_idx);
            for &line_idx in file_lines {
                let line_row = unit.line(line_idx);
                if line_row.is_stmt() && line_row.line >= line {
                    next_statement_line = Some(match next_statement_line {
                        Some(current) => current.min(line_row.line),
                        None => line_row.line,
                    });
                }
            }
        }

        if let Some(needle_line) = next_statement_line {
            for (unit_idx, file_lines) in &files {
                let unit = self.unit_ensure(*unit_idx);

                let mut suitable_places_in_unit = vec![];

                let mut i = 0;
                while i < file_lines.len() {
                    let mut line_idx = file_lines[i];
                    let next_line_row = unit.line(line_idx);

                    if suitable_places_in_unit.is_empty() {
                        // no places found at this point,
                        // try to find the closest place to a target line
                        if next_line_row.line != needle_line || !next_line_row.is_stmt() {
                            i += 1;
                            continue;
                        }

                        // now check that there is no prolog end in neighborhood line rows,
                        // if there is one then take it.
                        // This sets priority of line rows with PE over other
                        // line rows at this line as a breakpoint candidate
                        let mut ahead_idx = i + 1;
                        loop {
                            let Some(&ahead_line_idx) = file_lines.get(ahead_idx) else {
                                break;
                            };

                            let line_row = unit.line(ahead_line_idx);
                            if line_row.line != next_line_row.line || !line_row.is_stmt() {
                                break;
                            }

                            if line_row.prolog_end() {
                                line_idx = ahead_line_idx;
                                i = ahead_idx;
                                break;
                            }
                            ahead_idx += 1;
                        }

                        if let Some(place) = unit.find_place_by_idx(line_idx) {
                            suitable_places_in_unit.push(place);
                        }
                    } else {
                        // At least one line is found,
                        // now try to find lines with the same col and row
                        // as in found place in source code.
                        // This covers a case when compiler
                        // generates multiple representations of a single line, for example, when
                        // source code line in a part of a template function.
                        let line = suitable_places_in_unit[0].line_number;
                        let col = suitable_places_in_unit[0].column_number;
                        let pe = suitable_places_in_unit[0].prolog_end;
                        let eb = suitable_places_in_unit[0].epilog_begin;
                        let es = suitable_places_in_unit[0].end_sequence;

                        if next_line_row.line != line
                            || next_line_row.column != col
                            || next_line_row.prolog_end() != pe
                            || next_line_row.epilog_begin() != eb
                            || next_line_row.end_sequence() != es
                            || !next_line_row.is_stmt()
                        {
                            i += 1;
                            continue;
                        }

                        if let Some(place) = unit.find_place_by_idx(line_idx) {
                            suitable_places_in_unit.push(place);
                        }
                    }

                    i += 1;
                }

                for suitable_place in suitable_places_in_unit {
                    // only one place for a single unique subprogram is allowed
                    // to apply this rule as a filter for all places
                    if let Some((func, info)) = self.find_function_by_pc(suitable_place.address)? {
                        let key = Key {
                            name: info.name.clone(),
                            range: func.ranges(),
                        };
                        if unique_subprograms.contains(&key) {
                            if record_diagnostics {
                                diagnostics.candidates.push(LineCandidate {
                                    address: suitable_place.address,
                                    function: info.full_name(),
                                    decl_file: subprogram_decl_file(func, info),
                                    status: CandidateStatus::DuplicateSubprogram,
                                });
                            }
                            continue;
                        }
                        unique_subprograms.insert(key);

                        let decl_file = subprogram_decl_file(func, info);
                        let decl_file_match = subprogram_decl_file_matches(func, info, file_tpl);
                        // Stronger physical-function check: the line
                        // entry's PC must be in the SAME
                        // compact-unwind function as the DWARF
                        // subprogram's *real* entry (per nm). This
                        // catches the case where DWARF claims a wide
                        // subprogram range that overlaps with foreign
                        // physical functions (LTO / cold-block split
                        // / generic instantiation interleave); the
                        // candidate PC is in main per DWARF but
                        // physically in HashMap::insert, where main's
                        // locals' DWARF expressions don't apply.
                        let physical_match =
                            self.candidate_in_subprogram_range(suitable_place.address, info);
                        let canonical = decl_file_match && physical_match.unwrap_or(true);
                        if record_diagnostics {
                            diagnostics.candidates.push(LineCandidate {
                                address: suitable_place.address,
                                function: info.full_name(),
                                decl_file,
                                status: if canonical {
                                    CandidateStatus::Selected
                                } else {
                                    CandidateStatus::InlineCopy
                                },
                            });
                        }
                        if canonical {
                            result.push(suitable_place);
                        } else {
                            inline_fallback.push(suitable_place);
                        }
                    } else {
                        // No enclosing subprogram — keep it as a canonical
                        // match. Synthetic / orphaned addresses are rare
                        // enough that we don't bucket them as inline-only.
                        if record_diagnostics {
                            diagnostics.candidates.push(LineCandidate {
                                address: suitable_place.address,
                                function: None,
                                decl_file: None,
                                status: CandidateStatus::Selected,
                            });
                        }
                        result.push(suitable_place);
                    }
                }
            }
        }

        // When canonical entries exist, return only those — they are
        // the addresses where the source line was actually compiled.
        // Otherwise fall back to the inline copies so the user still
        // gets *some* breakpoint when the original line got optimised
        // out of its source function (e.g. a `let _ = ...` that the
        // compiler dropped from main but kept inside an inlined
        // callee). The fallback is logged via `log::debug!` so
        // perplexed users can grep for "fell back to inline copies"
        // and understand why their bp landed in a foreign function.
        if result.is_empty() && !inline_fallback.is_empty() {
            log::debug!(
                target: "debugger",
                "find_closest_place({file_tpl}, {line}): no canonical entry, \
                 fell back to {} inline copies; bp will land in foreign \
                 functions where the source line was inlined",
                inline_fallback.len(),
            );
            diagnostics.inline_fallback_used = true;
            // Promote the inline-copy candidates' status: the first one
            // (per unique subprogram) was actually used.
            if record_diagnostics {
                for c in diagnostics.candidates.iter_mut() {
                    if c.status == CandidateStatus::InlineCopy {
                        c.status = CandidateStatus::Selected;
                        // Only the first per subprogram is used;
                        // subsequent ones already bear DuplicateSubprogram
                        // status from the dedup gate above.
                        break;
                    }
                }
            }
            return Ok((inline_fallback, diagnostics));
        }

        Ok((result, diagnostics))
    }

    /// Return all places that correspond to the given file and line range.
    ///
    /// # Arguments
    ///
    /// * `file_tpl`: file name template (full path or part of a file path)
    /// * `start_line`: starting line (inclusive)
    /// * `end_line`: ending line (inclusive)
    pub fn find_places_in_line_range(
        &self,
        file_tpl: &str,
        start_line: u64,
        end_line: u64,
    ) -> Result<Vec<PlaceDescriptor<'_>>, Error> {
        let files = self.files_index.get(file_tpl);
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };

        let mut result = Vec::new();
        let mut seen = HashSet::new();

        for (unit_idx, file_lines) in &files {
            let unit = self.unit_ensure(*unit_idx);
            for &line_idx in file_lines {
                let line_row = unit.line(line_idx);
                if !line_row.is_stmt() {
                    continue;
                }
                let line = line_row.line;
                if line < start_line || line > end_line {
                    continue;
                }

                if let Some(place) = unit.find_place_by_idx(line_idx) {
                    let key = (place.address, place.line_number, place.column_number);
                    if seen.insert(key) {
                        result.push(place);
                    }
                }
            }
        }

        Ok(result)
    }

    /// Search all places for functions that relevant to template.
    /// Note, that result place points to the end of function prolog.
    ///
    /// # Arguments
    ///
    /// * `template`: search template (full function path or part of this path).
    pub fn search_places_for_fn_tpl(
        &self,
        template: &str,
    ) -> Result<Vec<PlaceDescriptorOwned>, Error> {
        Ok(self
            .search_functions(template)?
            .into_iter()
            .filter_map(|(fn_ref, _)| {
                weak_error!(fn_ref.prolog_end_place()).map(|place| place.to_owned())
            })
            .collect())
    }

    pub fn find_symbols(&'_ self, regex: &Regex) -> Vec<Symbol<'_>> {
        self.symbol_table
            .as_ref()
            .map(|table| table.find(regex))
            .unwrap_or_default()
    }

    /// Phase 3 Feature A — exact-address lookup for vtable
    /// resolution. Returns the *mangled* symbol name at `addr` —
    /// the parser's trait-object resolver re-parses it via
    /// `rust-mangle-tree` and walks to `impl_self_type()` to
    /// recover the concrete type behind a `dyn Trait`.
    ///
    /// Returns `None` for stripped binaries, anonymous /
    /// compiler-internal symbols, or when no debug-info is loaded
    /// for the address's module.
    pub fn mangled_symbol_at(&self, addr: u64) -> Option<&str> {
        self.symbol_table.as_ref()?.mangled_at(addr)
    }

    /// Reverse — find the linker-assigned address of a symbol named
    /// `name` (in the form `rust-mangle-tree` would print).
    pub fn symbol_address(&self, name: &str) -> Option<GlobalAddress> {
        self.symbol_table.as_ref()?.address_of(name)
    }

    /// Symbol-table-based "what function contains this PC?". Returns
    /// the `(start, end, mangled_name)` of the linker symbol whose
    /// range covers `pc`. The Mach-O / ELF symbol table is the
    /// linker's view of function boundaries — independent of DWARF
    /// subprogram ranges, which can claim impossibly-wide ranges
    /// after LTO.
    pub fn containing_text_symbol(&self, pc: u64) -> Option<(u64, u64, &str)> {
        self.symbol_table.as_ref()?.containing_text_symbol(pc)
    }

    /// True when `candidate_pc` falls inside the same physical
    /// (linker-visible) function as the subprogram `info`. Returns
    /// `None` when we don't have the data to make the call (no
    /// symbol table, no entry for the linkage name) — caller treats
    /// `None` as "no signal, don't demote".
    ///
    /// The check uses the linker's symbol table (more reliable than
    /// DWARF subprogram ranges, which lie after LTO and far more
    /// densely-populated than `__compact_unwind`): look up the
    /// subprogram's canonical entry by name; find the text-symbol
    /// range that contains it; check whether `candidate_pc` is in
    /// that same range.
    fn candidate_in_subprogram_range(
        &self,
        candidate_pc: GlobalAddress,
        info: &FunctionInfo,
    ) -> Option<bool> {
        // Resolve the subprogram's canonical linker address.
        // `SymbolTab::address_of` keys by the `rust-mangle-tree`
        // demangled form (e.g. `showcase::main`); `full_name`
        // produces that form for us.
        let lookup_name = info.full_name().or_else(|| info.name.clone())?;
        let sym_addr = u64::from(self.symbol_address(&lookup_name)?);
        let (sym_start, sym_end, _sym_name) = self.containing_text_symbol(sym_addr)?;
        let pc = u64::from(candidate_pc);
        Some(pc >= sym_start && pc < sym_end)
    }

    /// Look up the macOS `__compact_unwind` entry whose range covers
    /// `pc`. Returns `(start_address, end_address)` for the function;
    /// `None` if no compact unwind data, no entry, or `pc >= 4 GiB`
    /// from the image base (compact unwind keys are u32).
    ///
    /// This is the linker's view of "what physical function does this
    /// PC belong to" — independent of DWARF subprogram ranges, which
    /// can lie after LTO. See the prior-art notes for the rationale.
    pub fn compact_function_range_at(&self, pc: GlobalAddress) -> Option<(u64, u64)> {
        let bytes = self.compact_unwind_bytes.as_ref()?;
        let info = macho_unwind_info::UnwindInfo::parse(bytes.as_ref()).ok()?;
        let probe = u32::try_from(u64::from(pc)).ok()?;
        let f = info.lookup(probe).ok().flatten()?;
        Some((f.start_address as u64, f.end_address as u64))
    }

    pub fn tls_symbol_offset(&self, mangled_name: &str) -> Option<u64> {
        self.tls_symbol_tab.as_ref()?.get_offset(mangled_name)
    }

    pub fn tls_symbol_offset_stripped(&self, mangled_name: &str) -> Option<u64> {
        self.tls_symbol_tab
            .as_ref()?
            .get_offset_stripped(mangled_name)
    }

    pub fn find_variables(
        &self,
        location: Location,
        name: &str,
    ) -> Result<Vec<FatDieRef<'_, Variable>>, Error> {
        let units = self.get_units()?;

        let mut found = vec![];
        for unit in units {
            let mb_var_locations = resolve_unit_call!(self.dwarf(), unit, locate_var_die, name);

            if let Some(vars) = mb_var_locations {
                vars.iter().for_each(|(_, offset)| {
                    let fref = FatDieRef::new_no_hint(self, unit.idx(), *offset);

                    if let Some(die) = weak_error!(fref.deref())
                        && die.tag() == gimli::DW_TAG_variable
                    {
                        let variable = fref.with_new_hint::<Variable>();
                        if variable.valid_at(location.global_pc) {
                            found.push(variable);
                        }
                    }
                });
            }
        }

        for unit in units {
            let rustc_version = unit.rustc_version().unwrap_or_default();

            let tls_ns_part = version_switch!(
                rustc_version,
                .. (1 . 80) => {
                    // now check tls variables
                    // for rust we expect that tls variable represents in dwarf like
                    // variable with name "__KEY" and namespace like [.., variable_name, __getit]
                    vec![name, "__getit"]
                },
                (1 . 80) .. => {
                    vec![name]
                },
            );
            let tls_ns_part = tls_ns_part.expect("infallible: all rustc versions are covered");

            let mut tls_collector = |(namespaces, offset): &(NamespaceHierarchy, UnitOffset)| {
                if namespaces.contains(&tls_ns_part) {
                    let die_ref: FatDieRef<'_, _> =
                        FatDieRef::new_no_hint(self, unit.idx(), *offset);

                    if let Some(die) = weak_error!(die_ref.deref())
                        && die.tag() == gimli::DW_TAG_variable
                    {
                        found.push(die_ref.with_new_hint::<Variable>());
                    }
                }
            };

            if let Some(vars) = resolve_unit_call!(self.dwarf(), unit, locate_var_die, "__KEY") {
                vars.iter().for_each(&mut tls_collector);
            };
            if let Some(vars) = resolve_unit_call!(self.dwarf(), unit, locate_var_die, "VAL") {
                vars.iter().for_each(&mut tls_collector);
            };
            if let Some(vars) = resolve_unit_call!(
                self.dwarf(),
                unit,
                locate_var_die,
                "__RUST_STD_INTERNAL_VAL"
            ) {
                vars.iter().for_each(&mut tls_collector);
            };
        }

        Ok(found)
    }

    /// Return reference (unit and die offsets) to type die by type name.
    ///
    /// Search from `pub_types` section in priority, but if `pub_types` is empty,
    /// then a unit full scan may be occurred.
    pub fn find_type_die_ref(&self, name: &str) -> Option<(DebugInfoOffset, UnitOffset)> {
        if self.pub_types.is_empty() {
            self.get_units().ok()?.iter().find_map(|u| {
                u.offset().and_then(|u_offset| {
                    let type_ref_in_unit = resolve_unit_call!(&self.inner, u, locate_type, name)?;
                    Some((u_offset, type_ref_in_unit))
                })
            })
        } else {
            self.pub_types.get(name).copied()
        }
    }

    /// Return all suitable references (unit and die offsets) to type dies by type name.
    pub fn find_type_die_ref_all(&self, name: &str) -> Vec<(DebugInfoOffset, UnitOffset)> {
        self.get_units()
            .unwrap_or_default()
            .iter()
            .filter_map(|u| {
                u.offset().and_then(|u_offset| {
                    let type_ref_in_unit = resolve_unit_call!(&self.inner, u, locate_type, name)?;
                    Some((u_offset, type_ref_in_unit))
                })
            })
            .collect()
    }

    /// Return unit found at offset.
    #[inline(always)]
    pub fn find_unit(&self, offset: DebugInfoOffset) -> Option<&BsUnit> {
        let mb_unit = debug_info_exists!(self.get_units())
            .binary_search_by_key(&Some(offset), |u| u.offset());
        match mb_unit {
            Ok(_) | Err(0) => None,
            Err(pos) => Some(self.unit_ensure(pos - 1)),
        }
    }

    pub fn dwarf(&self) -> &Dwarf<EndianArcSlice> {
        &self.inner
    }

    /// Return the maximum and minimum address from the collection of unit ranges.
    pub fn range(&self) -> Option<Range> {
        let units = self.get_units().ok()?;

        // ranges already sorted by begin addr
        let begin = units
            .iter()
            .filter_map(|u| u.ranges().first().map(|r| r.begin))
            .min()?;

        let end = units
            .iter()
            .map(|u| {
                u.ranges().iter().fold(
                    begin,
                    |end, range| if range.end > end { range.end } else { end },
                )
            })
            .max()?;

        Some(Range { begin, end })
    }
}

/// Clone a `gimli::Dwarf<EndianArcSlice>` by reconstructing it with
/// each section's Arc-wrapped bytes cloned. `gimli::Dwarf` doesn't
/// implement `Clone`; this is the same dance the manual
/// `Clone for DebugInformation` impl does.
fn clone_dwarf(d: &Dwarf<EndianArcSlice>) -> Dwarf<EndianArcSlice> {
    Dwarf {
        debug_abbrev: d.debug_abbrev.clone(),
        debug_addr: d.debug_addr.clone(),
        debug_aranges: d.debug_aranges.clone(),
        debug_info: d.debug_info.clone(),
        debug_line: d.debug_line.clone(),
        debug_line_str: d.debug_line_str.clone(),
        debug_macro: d.debug_macro.clone(),
        debug_macinfo: d.debug_macinfo.clone(),
        debug_names: d.debug_names.clone(),
        debug_str: d.debug_str.clone(),
        debug_str_offsets: d.debug_str_offsets.clone(),
        debug_types: d.debug_types.clone(),
        locations: d.locations.clone(),
        ranges: d.ranges.clone(),
        file_type: d.file_type,
        sup: d.sup.clone(),
        abbreviations_cache: Default::default(),
    }
}

/// Build an empty `Dwarf<EndianArcSlice>` for fallback paths.
fn empty_dwarf() -> Dwarf<EndianArcSlice> {
    use gimli::{EndianArcSlice, RunTimeEndian};
    let empty = EndianArcSlice::new(std::sync::Arc::from(&[][..]), RunTimeEndian::Little);
    Dwarf {
        debug_abbrev: gimli::DebugAbbrev::from(empty.clone()),
        debug_addr: gimli::DebugAddr::from(empty.clone()),
        debug_aranges: gimli::DebugAranges::from(empty.clone()),
        debug_info: gimli::DebugInfo::from(empty.clone()),
        debug_line: gimli::DebugLine::from(empty.clone()),
        debug_line_str: gimli::DebugLineStr::from(empty.clone()),
        debug_macro: gimli::DebugMacro::from(empty.clone()),
        debug_macinfo: gimli::DebugMacinfo::from(empty.clone()),
        debug_names: gimli::DebugNames::from(empty.clone()),
        debug_str: gimli::DebugStr::from(empty.clone()),
        debug_str_offsets: gimli::DebugStrOffsets::from(empty.clone()),
        debug_types: gimli::DebugTypes::from(empty.clone()),
        locations: gimli::LocationLists::new(
            gimli::DebugLoc::from(empty.clone()),
            gimli::DebugLocLists::from(empty.clone()),
        ),
        ranges: gimli::RangeLists::new(
            gimli::DebugRanges::from(empty.clone()),
            gimli::DebugRngLists::from(empty.clone()),
        ),
        file_type: gimli::DwarfFileType::Main,
        sup: None,
        abbreviations_cache: Default::default(),
    }
}

/// One frame from `find_inline_chain`. Demangled function name + the
/// source location the call site was inlined from. Frames stack
/// innermost-first: `frames[0]` is the deepest `DW_TAG_inlined_subroutine`
/// (or the concrete subprogram if no inlining), `frames.last()` is
/// always the concrete `DW_TAG_subprogram`.
#[derive(Debug, Clone)]
pub struct InlineFrame {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

/// Outcome of a single line-table candidate after the chooser ran.
/// Surfaced by `set_breakpoint_at_line_with_diagnostics` so callers
/// can show users *which* address was picked when a source line maps
/// to many — typical with heavy monomorphization and inlining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateStatus {
    /// Picked for the breakpoint.
    Selected,
    /// Skipped because another candidate in the same subprogram was
    /// chosen first (dedup by enclosing subprogram).
    DuplicateSubprogram,
    /// Skipped because the enclosing subprogram's `decl_file` didn't
    /// match the user's `file_tpl`. The line entry exists at this
    /// address only because the source line got inlined into a
    /// different function.
    InlineCopy,
}

/// One line-table candidate. Several can collapse to a single
/// breakpoint per unique subprogram; the chooser records what it did
/// here so users see the disambiguation in plain words.
#[derive(Debug, Clone)]
pub struct LineCandidate {
    pub address: GlobalAddress,
    pub function: Option<String>,
    /// Path the enclosing subprogram was declared in. `None` when no
    /// enclosing subprogram could be found (synthetic / orphaned
    /// address).
    pub decl_file: Option<PathBuf>,
    pub status: CandidateStatus,
}

/// Diagnostic record for one `find_closest_place` call.
#[derive(Debug, Clone, Default)]
pub struct LineDiagnostics {
    pub candidates: Vec<LineCandidate>,
    /// `true` when no canonical candidate was found and the chooser
    /// fell back to inline-copy addresses. Callers may want to warn
    /// the user that the breakpoint will land in foreign functions.
    pub inline_fallback_used: bool,
}

/// True when the subprogram `info` was *declared* in a source file
/// matching `file_tpl` — i.e. the function whose ranges enclose the
/// candidate breakpoint address really was written in the file the
/// user asked about. False for subprograms (e.g. monomorphizations of
/// `HashMap::insert`) that merely *inline* code from the requested
/// file: those carry their own decl_file in some other crate.
///
/// Used by `find_closest_place` to prefer canonical line entries over
/// inline-attributed ones in foreign functions. See the diagnosis
/// transcript in the Phase 9 conformance suite — without this filter,
/// `break.set main.rs:117` on the showcase example lands inside
/// `hashbrown::HashMap::insert` and the agent reads garbage for
/// `dyn_ref`.
fn subprogram_decl_file_matches(
    func: FatDieRef<'_, Function>,
    info: &FunctionInfo,
    file_tpl: &str,
) -> bool {
    // No decl_file recorded — conservatively call it canonical so we
    // don't drop genuinely-orphaned places. (Synthetic functions
    // emitted without DW_AT_decl_file fall here.)
    let Some(path) = subprogram_decl_file(func, info) else {
        return true;
    };
    // The template is a suffix match against the path components — the
    // same shape `files_index` uses. Compare component-wise so that
    // "main.rs" matches "/x/y/main.rs" but NOT "/x/y/main.rs.bk".
    let path_str = path.to_string_lossy();
    path_ends_with_components(&path_str, file_tpl)
}

/// Look up the source file in which a subprogram was *declared*, via
/// its `DW_AT_decl_file` index resolved against the subprogram's CU
/// file table. `None` when the DIE carries no decl_file, or when the
/// file index points outside the CU's file table.
fn subprogram_decl_file(func: FatDieRef<'_, Function>, info: &FunctionInfo) -> Option<PathBuf> {
    let (decl_file_idx, _) = info.decl_file_line?;
    let unit = func.unit();
    unit.files().get(decl_file_idx as usize).cloned()
}

/// True when the slash-separated path ends with the slash-separated
/// suffix `tail`, on whole-component boundaries. Mirrors the matching
/// semantics of [`PathSearchIndex::get`] so the filter and the
/// candidate-generation step agree on which files match the template.
fn path_ends_with_components(path: &str, tail: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let tail_parts: Vec<&str> = tail.split('/').filter(|p| !p.is_empty()).collect();
    if tail_parts.len() > path_parts.len() {
        return false;
    }
    path_parts
        .iter()
        .rev()
        .zip(tail_parts.iter().rev())
        .all(|(a, b)| a == b)
}

#[derive(Default)]
pub struct DebugInformationBuilder;

impl DebugInformationBuilder {
    // todo configure this path
    const DEBUG_FILES_DIR: &'static str = "/usr/lib/debug";

    /// Compute the path to the DWARF file inside a `.dSYM` bundle:
    /// `<obj_path>.dSYM/Contents/Resources/DWARF/<basename>`.
    #[cfg(target_os = "macos")]
    fn dsym_inner_dwarf_path(obj_path: &Path) -> Option<PathBuf> {
        let basename = obj_path.file_name()?;
        let mut candidate = obj_path.as_os_str().to_owned();
        candidate.push(".dSYM");
        Some(
            PathBuf::from(candidate)
                .join("Contents")
                .join("Resources")
                .join("DWARF")
                .join(basename),
        )
    }

    /// macOS / `split-debuginfo = "unpacked"` recovery.
    ///
    /// Rust's default macOS layout leaves DWARF inside the per-CU
    /// `.o` files; the linker writes only `N_OSO` stab pointers
    /// into the executable. Cargo doesn't run `dsymutil` for you,
    /// so a fresh `cargo build` gives BugStalker an executable
    /// with neither inline DWARF nor a sidecar `.dSYM` bundle —
    /// every breakpoint goes UNVERIFIED and the user has no clue
    /// why.
    ///
    /// This recovery step runs `dsymutil` on first attach when:
    ///
    /// * the executable has no `__debug_info` section *and*
    /// * either no `.dSYM` bundle exists, or the bundle is older
    ///   than the executable.
    ///
    /// `dsymutil` then walks the OSO stabs itself (the same job
    /// LLDB's stab walker does) and consolidates the `.o` DWARF
    /// into the bundle. Subsequent attaches hit the bundle
    /// directly and skip this branch.
    ///
    /// Failures (binary not writable, dsymutil missing, etc.) are
    /// surfaced as `log::warn` and we fall through; the caller
    /// then degrades to "no debug info" with a clear message
    /// rather than UNVERIFIED-but-silent breakpoints.
    #[cfg(target_os = "macos")]
    fn ensure_dsym_fresh(obj_path: &Path, file: &object::File<'_>) -> Result<(), Error> {
        use std::process::Command;

        // Already has inline DWARF? Nothing to do.
        if file.section_by_name("__debug_info").is_some() {
            return Ok(());
        }

        let bin_mtime = fs::metadata(obj_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        let Some(dsym_path) = Self::dsym_inner_dwarf_path(obj_path) else {
            return Ok(());
        };
        let needs_refresh = match fs::metadata(&dsym_path).and_then(|m| m.modified()) {
            Ok(dsym_mtime) => dsym_mtime < bin_mtime,
            Err(_) => true,
        };
        if !needs_refresh {
            return Ok(());
        }

        debug!(
            target: "dwarf-loader",
            "{obj_path:?}: no inline DWARF and no fresh .dSYM bundle — \
             running `dsymutil` to materialise debug info from the \
             OSO-pointed `.o` files"
        );
        match Command::new("dsymutil").arg(obj_path).status() {
            Ok(s) if s.success() => {
                debug!(target: "dwarf-loader", "dsymutil produced {dsym_path:?}");
            }
            Ok(s) => {
                log::warn!(
                    target: "dwarf-loader",
                    "dsymutil exited with status {s}; debug info will be unavailable. \
                     If the binary lives in a read-only path, copy it locally and re-run \
                     `dsymutil <bin>` by hand."
                );
            }
            Err(e) => {
                log::warn!(
                    target: "dwarf-loader",
                    "could not spawn `dsymutil`: {e}. Install Xcode command-line tools \
                     (`xcode-select --install`) or run `dsymutil <bin>` manually."
                );
            }
        }
        Ok(())
    }

    /// Look for `<obj_path>.dSYM/Contents/Resources/DWARF/<basename>`
    /// — Apple's bundle layout for separate-file DWARF, produced by
    /// `dsymutil`. Cargo's `target/debug/<bin>` does NOT contain
    /// embedded DWARF on macOS by default; the user (or a build
    /// script) has to invoke `dsymutil` to extract debug info from
    /// the `.o` files into the bundle. If no bundle is present, we
    /// return `None` and the caller falls back to using the binary
    /// itself (which will yield "no debug information" rather than
    /// crash).
    #[cfg(target_os = "macos")]
    fn get_dwarf_from_dsym_bundle(
        &self,
        obj_path: &Path,
    ) -> Result<Option<(PathBuf, Mmap)>, Error> {
        let Some(bundle) = Self::dsym_inner_dwarf_path(obj_path) else {
            return Ok(None);
        };
        if !bundle.exists() {
            return Ok(None);
        }
        let file = fs::File::open(&bundle)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Some((bundle, mmap)))
    }

    fn get_dwarf_from_separate_debug_file<'a, 'b, OBJ>(
        &self,
        obj_file: &'a OBJ,
    ) -> Result<Option<(PathBuf, Mmap)>, Error>
    where
        'a: 'b,
        OBJ: Object<'a, 'b>,
    {
        // try build-id
        let debug_id_sect = obj_file.section_by_name(".note.gnu.build-id");
        if let Some(build_id) = debug_id_sect {
            let data = build_id.data()?;
            // skip 16 byte header
            let note = &data[16..];
            if note.len() < 2 {
                return Err(DebugIDFormat);
            }

            let dir = format!("{:02x}", note[0]);
            let file = note[1..]
                .iter()
                .map(|&b| format!("{b:02x}"))
                .collect::<Vec<String>>()
                .join("")
                .add(".debug");

            let path = PathBuf::from(Self::DEBUG_FILES_DIR)
                .join(".build-id")
                .join(dir)
                .join(file);
            let file = fs::File::open(path.as_path())?;
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            return Ok(Some((path, mmap)));
        }

        // try debug link
        let debug_link_sect = obj_file.section_by_name(".gnu_debuglink");
        if let Some(sect) = debug_link_sect {
            let data = sect.data()?;
            let data: Vec<u8> = data.iter().take_while(|&&b| b != 0).copied().collect();
            let debug_link = std::str::from_utf8(&data)?;

            for entry in WalkDir::new(Self::DEBUG_FILES_DIR)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let name = entry.file_name().to_string_lossy();
                if name.contains(debug_link) {
                    let file = fs::File::open(entry.path())?;
                    let mmap = unsafe { memmap2::Mmap::map(&file)? };
                    return Ok(Some((entry.path().to_path_buf(), mmap)));
                }
            }
        }

        Ok(None)
    }

    pub fn build(&self, obj_path: &Path, file: &object::File) -> Result<DebugInformation, Error> {
        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };

        let mut eh_frame = EhFrame::load(|id| -> Result<EndianArcSlice, Error> {
            loader::load_section(id, file, endian)
        })?;
        #[cfg(target_arch = "aarch64")]
        eh_frame.set_vendor(gimli::Vendor::AArch64);
        // Section names differ across container formats:
        //   ELF:    `.text`, `.eh_frame`, `.eh_frame_hdr`, `.got`
        //   Mach-O: `__text`, `__eh_frame`, `__got` (no `.eh_frame_hdr`
        //           on darwin — `.eh_frame_hdr` is a GNU/ELF extension).
        // Try both spellings; whichever the object reports wins.
        let section_addr = |names: &[&str]| -> Option<u64> {
            file.sections().find_map(|section| {
                let n = section.name().ok()?;
                if names.iter().any(|w| *w == n) {
                    Some(section.address())
                } else {
                    None
                }
            })
        };
        // macOS-only: `__unwind_info` (compact unwind) is read in
        // addition to `__eh_frame`. Most Rust functions on macOS arm64
        // live only in compact unwind; without this fallback bs can't
        // compute CFA / frame_base at their PCs and variable reads
        // return garbage. The bytes are stored on the
        // `DebugInformation` and parsed lazily on each query.
        let compact_unwind_bytes: Option<std::sync::Arc<Vec<u8>>> = file
            .sections()
            .find(|s| s.name().ok() == Some("__unwind_info"))
            .and_then(|s| s.data().ok())
            .map(|d| std::sync::Arc::new(d.to_vec()));
        let mut bases = BaseAddresses::default();
        if let Some(got) = section_addr(&[".got", "__got"]) {
            bases = bases.set_got(got);
        }
        if let Some(text) = section_addr(&[".text", "__text"]) {
            bases = bases.set_text(text);
        }
        if let Some(eh) = section_addr(&[".eh_frame", "__eh_frame"]) {
            bases = bases.set_eh_frame(eh);
        }
        if let Some(eh_frame_hdr) = section_addr(&[".eh_frame_hdr"]) {
            bases = bases.set_eh_frame_hdr(eh_frame_hdr);
        }

        // Order of debug-info lookup:
        //   0. (macos) auto-run `dsymutil` if the binary has no
        //      inline DWARF and no fresh dSYM bundle — covers
        //      Rust's default `split-debuginfo = "unpacked"`
        //      where DWARF lives in `.o` files referenced via
        //      Mach-O `N_OSO` stabs.
        //   1. (macos) <obj_path>.dSYM bundle — Apple's separate-file
        //      layout produced by `dsymutil`.
        //   2. (linux) build-id index under /usr/lib/debug/.build-id.
        //   3. (linux) `.gnu_debuglink` walk of /usr/lib/debug.
        //   4. fall back to the binary's embedded DWARF.
        let debug_split_file_data;
        let debug_split_file;

        #[cfg(target_os = "macos")]
        {
            // Best-effort recovery: failure logged via warn but
            // doesn't abort the load — we'd rather degrade to
            // "no debug info" than refuse to attach.
            let _ = Self::ensure_dsym_fresh(obj_path, file);
        }

        #[cfg(target_os = "macos")]
        let dsym = self.get_dwarf_from_dsym_bundle(obj_path).ok().flatten();
        #[cfg(not(target_os = "macos"))]
        let dsym: Option<(PathBuf, Mmap)> = None;

        let debug_info_file = if let Some((path, debug_file)) = dsym {
            debug!(target: "dwarf-loader", "{obj_path:?} has dSYM bundle at {path:?}");
            debug_split_file_data = debug_file;
            debug_split_file = object::File::parse(&*debug_split_file_data)?;
            &debug_split_file
        } else if let Ok(Some((path, debug_file))) = self.get_dwarf_from_separate_debug_file(file) {
            debug!(target: "dwarf-loader", "{obj_path:?} has separate debug information file");
            debug!(target: "dwarf-loader", "load debug information from {path:?}");
            debug_split_file_data = debug_file;
            debug_split_file = object::File::parse(&*debug_split_file_data)?;
            &debug_split_file
        } else {
            debug!(target: "dwarf-loader", "load debug information from {obj_path:?}");
            file
        };

        let dwarf = loader::load_par(debug_info_file, endian)?;
        let debug_frame = if debug_info_file.section_by_name(".debug_frame").is_some() {
            let mut df = DebugFrame::load(|id| -> Result<EndianArcSlice, Error> {
                loader::load_section(id, debug_info_file, endian)
            })?;
            #[cfg(target_arch = "aarch64")]
            df.set_vendor(gimli::Vendor::AArch64);
            Some(df)
        } else {
            None
        };
        // SymbolTab is the *linker's* view of function and global
        // symbol locations. On macOS, the dSYM bundle's nlist table
        // reports DWARF-claimed addresses (which can lie after LTO);
        // the original binary's nlist is the truth. Always source
        // from `file` (the runtime image), not `debug_info_file`
        // (which may be a separate dSYM).
        let symbol_table = SymbolTab::new(file);
        let tls_symbol_tab = TlsSymbolTab::new(debug_info_file);

        // let mb_pub_names_sect = muted_error!(DebugPubNames::load(|id| {
        //     loader::load_section(id, debug_info_file, endian)
        // }));
        // let pub_names = mb_pub_names_sect.and_then(|pub_names_sect| {
        //     let mut names_trie = TrieBuilder::new();
        //     muted_error!(pub_names_sect.items().for_each(|pub_name| {
        //         let name = pub_name.name().to_string_lossy()?;
        //         names_trie.push(name.as_bytes());
        //         Ok(())
        //     }))?;
        //     Some(names_trie.build())
        // });

        // Currently pub_names section is not used
        // because the current function-search algorithm anyway
        // will load all dwarf DIE information after name was found in .debug_pubnames section.
        // Maybe this will be changed in the future, when debugger loads only DIE that points by
        // name from .debug_pubnames section.
        let pub_names = None;

        let mb_pub_types_sect = muted_error!(DebugPubTypes::load(|id| {
            loader::load_section(id, debug_info_file, endian)
        }));
        let pub_types = mb_pub_types_sect.and_then(|pub_types_sect| {
            pub_types_sect
                .items()
                .map(|e| match e {
                    Ok(e) => {
                        let type_name = e.name().to_string_lossy()?.to_string();
                        let unit_offset = e.unit_header_offset();
                        Ok((type_name, (unit_offset, e.die_offset())))
                    }
                    Err(e) => Err(e),
                })
                .collect::<Result<_, _>>()
                .ok()
        });

        let parser = DwarfUnitParser::new(&dwarf);
        let headers = dwarf.units().collect::<Result<Vec<_>, _>>()?;

        if headers.is_empty() {
            // no units means no debug info
            debug!(target: "dwarf-loader", "no debug information for {obj_path:?}");

            return Ok(DebugInformation {
                file: obj_path.to_path_buf(),
                inner: dwarf,
                eh_frame,
                debug_frame,
                bases,
                units: None,
                symbol_table,
                tls_symbol_tab,
                pub_names,
                pub_types: pub_types.unwrap_or_default(),
                files_index: PathSearchIndex::new(""),
                addr2_ctx: once_cell::sync::OnceCell::new(),
                compact_unwind_bytes: None,
            });
        }

        let headers_len = headers.len();
        let mut units = headers
            .into_par_iter()
            .map(|header| -> gimli::Result<BsUnit> {
                let unit = parser.parse(header)?;
                Ok(unit)
            })
            .collect::<gimli::Result<Vec<_>>>()?;
        debug_assert!(units.capacity() == headers_len);

        units.sort_unstable_by_key(|u| u.offset());
        units.iter_mut().enumerate().for_each(|(i, u)| u.set_idx(i));

        let mut files_index = PathSearchIndex::new(path::MAIN_SEPARATOR_STR);
        units.iter().for_each(|unit| {
            unit.file_path_with_lines_pairs()
                .for_each(|(file_path, lines)| {
                    files_index.insert(file_path, (unit.idx(), lines));
                });
        });
        files_index.shrink_to_fit();

        Ok(DebugInformation {
            file: obj_path.to_path_buf(),
            inner: dwarf,
            eh_frame,
            debug_frame,
            bases,
            units: Some(units),
            symbol_table,
            tls_symbol_tab,
            pub_names,
            pub_types: pub_types.unwrap_or_default(),
            files_index,
            addr2_ctx: once_cell::sync::OnceCell::new(),
            compact_unwind_bytes,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespaceHierarchy(Vec<string_interner::DefaultSymbol>);

impl NamespaceHierarchy {
    pub fn new(parts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let inner = parts
            .into_iter()
            .map(|s| gcx().with_interner(|i| i.get_or_intern(s)));
        Self(inner.collect())
    }

    pub fn as_parts(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|s| {
                gcx().with_interner(|i| {
                    i.resolve(*s)
                        .expect("symbol should be resolved")
                        .to_string()
                })
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Create namespace for a selected die.
    ///
    /// # Arguments
    ///
    /// * `dcx`: die dereferencing context
    /// * `die_offset`: offset of root die
    /// * `parent_index`: parent index
    pub fn for_die(
        dcx: DerefContext,
        die_offset: gimli::UnitOffset,
        parent_index: &IndexMap<UnitOffset, UnitOffset>,
    ) -> Self {
        let mut ns_chain = vec![];
        let mut p_idx = parent_index.get(&die_offset).copied();
        let mut next_parent = || -> Option<_> {
            let parent = weak_error!(Die::new(dcx.clone(), p_idx?))?;
            p_idx = parent_index.get(&parent.offset()).copied();
            Some(parent)
        };

        use gimli::DW_TAG_namespace as NS_TAG;
        while let Some((NS_TAG, next_die)) = next_parent().map(|die| (die.tag(), die)) {
            ns_chain.push(next_die.name().unwrap_or_default());
        }
        ns_chain.reverse();

        NamespaceHierarchy::new(ns_chain)
    }

    /// Return `true` if namespace part contains in target namespace, `false` otherwise.
    ///
    /// # Arguments
    ///
    /// * `needle`: searched part of the namespace
    pub fn contains(&self, needle: &[&str]) -> bool {
        let needle_symbols = needle
            .iter()
            .map(|n| gcx().with_interner(|i| i.get_or_intern(n)))
            .collect::<Vec<_>>();
        self.0
            .windows(needle.len())
            .any(|slice| slice == needle_symbols)
    }

    /// Return (namespace, subroutine name) pair from mangled representation.
    ///
    /// # Arguments
    ///
    /// * `linkage_name`: mangled subroutine name
    #[inline(always)]
    pub fn from_mangled(linkage_name: &str) -> (Self, String) {
        // Phase 2 batch H: drive demangling through `rust-mangle-tree`.
        // The crate's `Display` already produces the short form (no
        // trailing per-mono hash) so we don't need a separate `:#`
        // step. On parse error, fall back to the raw mangled string —
        // splitting that on `::` will yield a one-element list, which
        // matches the existing test for unrecognised inputs like
        // `"poll"`.
        let demangled = match rust_mangle_tree::parse(linkage_name) {
            Ok(sym) => sym.to_string(),
            Err(_) => linkage_name.to_string(),
        };
        let mut parts: Vec<_> = demangled.split("::").map(ToString::to_string).collect();
        debug_assert!(!parts.is_empty());
        let fn_name = parts.pop().expect("function name must exists");
        (NamespaceHierarchy::new(parts), fn_name)
    }
}

#[cfg(test)]
mod test {
    use crate::debugger::debugee::dwarf::NamespaceHierarchy;

    #[test]
    fn test_namespace_from_mangled() {
        struct TestCase {
            mangled: &'static str,
            expected_ns: Vec<String>,
            expected_fn: &'static str,
        }

        let test_cases = vec![
            TestCase {
                mangled: "_ZN5tokio7runtime4task3raw7RawTask4poll17h7b89afb116da4cf2E",
                expected_ns: vec![
                    "tokio".to_string(),
                    "runtime".to_string(),
                    "task".to_string(),
                    "raw".to_string(),
                    "RawTask".to_string(),
                ],
                expected_fn: "poll",
            },
            TestCase {
                mangled: "poll",
                expected_ns: vec![],
                expected_fn: "poll",
            },
        ];

        for tc in test_cases {
            let (ns, name) = NamespaceHierarchy::from_mangled(tc.mangled);
            assert_eq!(ns.as_parts(), tc.expected_ns);
            assert_eq!(name, tc.expected_fn);
        }
    }
}

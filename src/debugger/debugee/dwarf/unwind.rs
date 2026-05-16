// SPDX-License-Identifier: MIT
use crate::debugger::address::RelocatedAddress;
use crate::debugger::debugee::dwarf::EndianArcSlice;
use crate::debugger::debugee::dwarf::eval::{AddressKind, ExpressionEvaluator};
use crate::debugger::debugee::{Debugee, Location};
use crate::debugger::error::Error;
use crate::debugger::error::Error::{
    TypeBinaryRepr, UnitNotFound, UnwindNoContext, UnwindTooDeepFrame,
};
use crate::debugger::register::{DwarfRegisterMap, Register, RegisterMap};
use crate::debugger::utils::TryGetOrInsert;
use crate::debugger::{ExplorationContext, PlaceDescriptorOwned};
use crate::{debugger, resolve_unit_call, weak_error};
use gimli::{DebugFrame, EhFrame, FrameDescriptionEntry, RegisterRule, UnwindSection};
use log::warn;
use nix::unistd::Pid;
use std::collections::HashSet;
use std::mem;

/// Strip Pointer Authentication Code (PAC) signature bits from an address.
///
/// On aarch64 CPUs with PAC enabled, the link register (return address) has
/// authentication bits set in the upper bits. These must be cleared before the
/// address can be used for code lookups.
#[cfg(target_arch = "aarch64")]
fn strip_pac(addr: u64) -> u64 {
    // On aarch64, bit 55 distinguishes user-space (0) from kernel-space (1).
    // PAC signs the unused upper bits above the virtual address width.
    // For user-space (48-bit VA): clear bits 48-63 to recover the VA.
    // For kernel-space: sign-extend from bit 55 (set bits 56-63).
    // We only debug user-space processes so the first branch dominates.
    if addr & (1 << 55) == 0 {
        addr & 0x0000_FFFF_FFFF_FFFF
    } else {
        addr | 0xFFFF_0000_0000_0000
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn strip_pac(addr: u64) -> u64 {
    addr
}

/// Unique frame identifier. It is just an address of the first instruction in function.
pub type FrameID = RelocatedAddress;

/// Represents detailed information about single stack frame in the unwind path.
#[derive(Debug, Default, Clone)]
pub struct FrameSpan {
    pub func_name: Option<String>,
    pub fn_start_ip: Option<RelocatedAddress>,
    pub ip: RelocatedAddress,
    pub place: Option<PlaceDescriptorOwned>,
}

impl FrameSpan {
    fn new(debugee: &Debugee, location: Location) -> Result<Self, Error> {
        // PC may sit outside any module BugStalker has DWARF for —
        // typical when a worker thread is mid-syscall in
        // `libsystem_kernel.dylib` (darwin) or in libc (linux). Treat
        // that as an anonymous frame rather than failing the whole
        // unwind: the user still benefits from seeing the call stack
        // *up to* the unknown frame.
        let debug_information = match debugee.debug_info(location.pc) {
            Ok(di) => di,
            Err(Error::NoDebugInformation(_)) => {
                return Ok(FrameSpan {
                    func_name: None,
                    fn_start_ip: None,
                    ip: location.pc,
                    place: None,
                });
            }
            Err(e) => return Err(e),
        };

        let function = debug_information
            .find_function_by_pc(location.global_pc)
            .unwrap_or_default();

        let fn_start_at = function
            .as_ref()
            .and_then(|(die_ref, _)| {
                die_ref.prolog_start_place().ok().map(|prolog| {
                    prolog
                        .address
                        .relocate_to_segment_by_pc(debugee, location.pc)
                })
            })
            .transpose()
            .unwrap_or_default();

        let pc = location.pc.into_global(debugee)?;

        let place = debug_information
            .find_place_from_pc(pc)
            .unwrap_or_default()
            .map(|p| p.to_owned());

        Ok(FrameSpan {
            func_name: function.and_then(|(_, info)| info.full_name()),
            fn_start_ip: fn_start_at,
            ip: location.pc,
            place,
        })
    }

    #[inline(always)]
    pub fn id(&self) -> Option<FrameID> {
        self.fn_start_ip
    }
}

pub type Backtrace = Vec<FrameSpan>;

/// Unwind thread stack and return a backtrace.
///
/// # Arguments
///
/// * `debugee`: debugee instance
/// * `pid`: thread for unwinding
pub fn unwind(debugee: &Debugee, pid: Pid) -> Result<Backtrace, Error> {
    let unwinder = DwarfUnwinder::new(debugee);
    unwinder.unwind(pid)
}

/// Restore registers at chosen frame.
///
/// # Arguments
///
/// * `debugee`: debugee instance
/// * `pid`: thread for unwinding
/// * `registers`: initial registers state at frame 0 (current frame), will be updated with new values
/// * `frame_num`: frame number for which registers is restored
#[allow(unused)]
pub fn restore_registers_at_frame(
    debugee: &Debugee,
    pid: Pid,
    registers: &mut DwarfRegisterMap,
    frame_num: u32,
) -> Result<(), Error> {
    let unwinder = DwarfUnwinder::new(debugee);
    unwinder.restore_registers_at_frame(pid, registers, frame_num)
}

/// Return return address for thread current program counter.
///
/// # Arguments
///
/// * `debugee`: debugee instance
/// * `pid`: thread for unwinding
#[allow(unused)]
pub fn return_addr(debugee: &Debugee, pid: Pid) -> Result<Option<RelocatedAddress>, Error> {
    let unwinder = DwarfUnwinder::new(debugee);
    unwinder.return_address(pid)
}

/// UnwindContext (or ucx) contains information for unwinding single frame.
pub struct UnwindContext<'a> {
    registers: DwarfRegisterMap,
    location: Location,
    fde: FrameDescriptionEntry<EndianArcSlice, usize>,
    debugee: &'a Debugee,
    cfa: RelocatedAddress,
}

impl<'a> UnwindContext<'a> {
    fn new(
        debugee: &'a Debugee,
        registers: DwarfRegisterMap,
        ecx: &ExplorationContext,
    ) -> Result<Option<Self>, Error> {
        let dwarf = &debugee.debug_info(ecx.location().pc)?;
        let mut next_registers = registers.clone();
        let registers_snap = registers;
        let mut ucx = Box::new(gimli::UnwindContext::new());
        let (fde, row) = match dwarf.eh_frame.fde_for_address(
            &dwarf.bases,
            ecx.location().global_pc.into(),
            EhFrame::cie_from_offset,
        ) {
            Ok(fde) => {
                let row = fde.unwind_info_for_address(
                    &dwarf.eh_frame,
                    &dwarf.bases,
                    &mut ucx,
                    ecx.location().global_pc.into(),
                )?;
                (fde, row)
            }
            Err(gimli::Error::NoUnwindInfoForAddress) => {
                let Some(debug_frame) = dwarf.debug_frame.as_ref() else {
                    return Ok(None);
                };
                let fde = match debug_frame.fde_for_address(
                    &dwarf.bases,
                    ecx.location().global_pc.into(),
                    DebugFrame::cie_from_offset,
                ) {
                    Ok(fde) => fde,
                    Err(gimli::Error::NoUnwindInfoForAddress) => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let row = fde.unwind_info_for_address(
                    debug_frame,
                    &dwarf.bases,
                    &mut ucx,
                    ecx.location().global_pc.into(),
                )?;
                (fde, row)
            }
            Err(e) => return Err(e.into()),
        };
        let cfa = dwarf.evaluate_cfa(debugee, &registers_snap, row, ecx)?;

        let mut lazy_evaluator = None;
        let evaluator_init_fn = || -> Result<ExpressionEvaluator, Error> {
            let unit = dwarf
                .find_unit_by_pc(ecx.location().global_pc)?
                .ok_or(UnitNotFound(ecx.location().global_pc))?;

            let evaluator =
                resolve_unit_call!(&dwarf.inner, unit, evaluator, debugee, dwarf.dwarf());
            Ok(evaluator)
        };

        let read_register_value = |addr: RelocatedAddress| -> Option<u64> {
            let bytes = weak_error!(debugger::read_memory_by_pid(
                ecx.pid_on_focus(),
                addr.into(),
                mem::size_of::<usize>()
            ))?;
            let value = usize::from_ne_bytes(weak_error!(
                bytes
                    .try_into()
                    .map_err(|data: Vec<u8>| TypeBinaryRepr("usize", data.into_boxed_slice()))
            )?);
            Some(value as u64)
        };

        row.registers()
            .filter_map(|(register, rule)| {
                let value = match rule {
                    RegisterRule::Undefined => return None,
                    RegisterRule::SameValue => weak_error!(registers_snap.value(*register))?,
                    RegisterRule::Offset(offset) => {
                        let addr = cfa.offset(*offset as isize);
                        read_register_value(addr)?
                    }
                    RegisterRule::ValOffset(offset) => cfa.offset(*offset as isize).into(),
                    RegisterRule::Register(reg) => weak_error!(registers_snap.value(*reg))?,
                    RegisterRule::Expression(expr) => {
                        let expr = weak_error!(expr.get(&dwarf.eh_frame))?;
                        let evaluator =
                            weak_error!(lazy_evaluator.try_get_or_insert_with(evaluator_init_fn))?;
                        let expr_result = weak_error!(evaluator.evaluate(ecx, expr))?;
                        let addr = weak_error!(
                            expr_result.into_scalar::<usize>(AddressKind::MemoryAddress)
                        )?;
                        read_register_value(RelocatedAddress::from(addr))?
                    }
                    RegisterRule::ValExpression(expr) => {
                        let expr = weak_error!(expr.get(&dwarf.eh_frame))?;
                        let evaluator =
                            weak_error!(lazy_evaluator.try_get_or_insert_with(evaluator_init_fn))?;
                        let expr_result = weak_error!(evaluator.evaluate(ecx, expr.clone()))?;
                        weak_error!(expr_result.into_scalar::<usize>(AddressKind::MemoryAddress))?
                            as u64
                    }
                    RegisterRule::Architectural => return None,
                    RegisterRule::Constant(val) => *val,
                };

                Some((*register, value))
            })
            .for_each(|(reg, val)| next_registers.update(reg, val));

        Ok(Some(Self {
            registers: next_registers,
            location: ecx.location(),
            debugee,
            fde,
            cfa,
        }))
    }

    pub fn next(
        previous_ucx: UnwindContext<'a>,
        ecx: &ExplorationContext,
    ) -> Result<Option<Self>, Error> {
        let mut next_frame_registers: DwarfRegisterMap = previous_ucx.registers;
        let sp_register = Register::SP
            .dwarf_register()
            .expect("stack pointer register must map to dwarf register");
        next_frame_registers.update(sp_register, previous_ucx.cfa.into());
        UnwindContext::new(previous_ucx.debugee, next_frame_registers, ecx)
    }

    fn return_address(&self) -> Option<RelocatedAddress> {
        let register = self.fde.cie().return_address_register();
        self.registers
            .value(register)
            .map(|addr| RelocatedAddress::from(strip_pac(addr)))
            .ok()
    }

    pub fn registers(&self) -> DwarfRegisterMap {
        self.registers.clone()
    }
}

/// Unwind debugee call stack by dwarf information.
///
/// [`DwarfUnwinder`] also useful for getting return address for current location and register values for subroutine entry.
pub struct DwarfUnwinder<'a> {
    debugee: &'a Debugee,
}

/// Hard safety cap to prevent pathological or corrupted DWARF unwind
/// from running indefinitely or allocating unbounded memory.
///
/// Real-world call stacks are typically far smaller than this value.
const MAX_UNWIND_DEPTH: usize = 512;

impl<'a> DwarfUnwinder<'a> {
    /// Creates new unwinder.
    ///
    /// # Arguments
    ///
    /// * `debugee`: current debugee program.
    pub fn new(debugee: &'a Debugee) -> DwarfUnwinder<'a> {
        Self { debugee }
    }

    /// AArch64 frame-pointer walk. Used as a fallback when DWARF
    /// unwinding can't make progress because frame 0 is in an
    /// untracked dylib (libsystem on darwin, libc on linux). Walks
    /// the `x29` chain and pushes a frame for each `lr` we read.
    /// Returns the DWARF [`UnwindContext`] for the first frame
    /// whose `lr` lands inside a tracked dylib so the caller can
    /// resume normal DWARF unwinding from there; returns `None` if
    /// we exhaust the chain without ever reaching a tracked module.
    ///
    /// The walk is bounded by `MAX_UNWIND_DEPTH` and a visited-fp
    /// loop guard.
    ///
    /// # Arguments
    ///
    /// * pid: thread for unwinding
    #[cfg(target_arch = "aarch64")]
    fn fp_walk_into_dwarf(
        &self,
        raw_registers: &RegisterMap,
        bt: &mut Vec<FrameSpan>,
        visited_ips: &mut HashSet<RelocatedAddress>,
        pid: Pid,
    ) -> Result<Option<UnwindContext<'a>>, Error> {
        let mut fp = raw_registers.value(Register::X29);
        let mut visited_fps: HashSet<u64> = HashSet::new();
        while bt.len() < MAX_UNWIND_DEPTH {
            if fp == 0 || !visited_fps.insert(fp) {
                return Ok(None);
            }
            // Read [saved_fp, saved_lr] = 16 bytes at *fp.
            let bytes = match debugger::read_memory_by_pid(pid, fp as usize, 16) {
                Ok(b) if b.len() == 16 => b,
                _ => return Ok(None),
            };
            let next_fp = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
            let lr = strip_pac(u64::from_ne_bytes(bytes[8..16].try_into().unwrap()));
            if lr == 0 {
                return Ok(None);
            }
            let lr_addr = RelocatedAddress::from(lr);
            if !visited_ips.insert(lr_addr) {
                return Ok(None);
            }

            // Probe whether `lr` lands in a dylib we have DWARF for.
            // If yes, build an UnwindContext at that PC and let the
            // caller continue with DWARF.
            let location = Location {
                pc: lr_addr,
                global_pc: match lr_addr.into_global(self.debugee) {
                    Ok(g) => g,
                    Err(_) => {
                        // Untracked module — push an anonymous frame
                        // and keep walking.
                        bt.push(FrameSpan {
                            ip: lr_addr,
                            fn_start_ip: None,
                            func_name: None,
                            place: None,
                        });
                        fp = next_fp;
                        continue;
                    }
                },
                pid,
            };
            // Synthesise registers for the caller frame: x29 = next_fp,
            // x30 = lr, sp = fp + 16 (caller's sp = our fp record top),
            // pc = lr.
            let mut next_regs: DwarfRegisterMap = DwarfRegisterMap::from(raw_registers.clone());
            let dw_x29 = Register::X29
                .dwarf_register()
                .expect("aarch64 x29 has a dwarf register number");
            let dw_x30 = Register::X30
                .dwarf_register()
                .expect("aarch64 x30 has a dwarf register number");
            let dw_sp = Register::SP
                .dwarf_register()
                .expect("aarch64 sp has a dwarf register number");
            let dw_pc = Register::PC
                .dwarf_register()
                .expect("aarch64 pc has a dwarf register number");
            next_regs.update(dw_x29, next_fp);
            next_regs.update(dw_x30, lr);
            next_regs.update(dw_sp, fp + 16);
            next_regs.update(dw_pc, lr);
            let ecx_at = ExplorationContext::new(location, bt.len() as u32);
            match UnwindContext::new(self.debugee, next_regs, &ecx_at) {
                Ok(Some(u)) => {
                    bt.push(FrameSpan::new(self.debugee, location)?);
                    return Ok(Some(u));
                }
                Ok(None) | Err(Error::NoDebugInformation(_)) => {
                    bt.push(FrameSpan {
                        ip: lr_addr,
                        fn_start_ip: None,
                        func_name: None,
                        place: None,
                    });
                    fp = next_fp;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    pub fn unwind(&self, pid: Pid) -> Result<Backtrace, Error> {
        let frame_0_location = self
            .debugee
            .tracee_ctl()
            .tracee_ensure(pid)
            .location(self.debugee)?;

        let mut ecx = ExplorationContext::new(frame_0_location, 0);
        let raw_registers = RegisterMap::current(ecx.pid_on_focus())?;
        // A thread parked mid-syscall (libsystem_kernel.dylib on
        // darwin, libc.so.6 on linux when ptrace catches a thread
        // mid-`nanosleep`) has no debug-info module covering its
        // current PC. Two ways forward: (a) emit just frame 0 and
        // stop; (b) follow the AArch64 frame-pointer chain through
        // the un-DWARF'd region until we land back in a tracked
        // dylib, then resume DWARF unwinding from there. (b) is
        // what tokio worker discovery / multithreaded backtrace
        // need, so we try (b) on darwin and fall through to (a) if
        // it can't make progress.
        let mb_ucx = match UnwindContext::new(
            self.debugee,
            DwarfRegisterMap::from(raw_registers.clone()),
            &ecx,
        ) {
            Ok(v) => v,
            Err(Error::NoDebugInformation(_)) => None,
            Err(e) => return Err(e),
        };

        let mut bt = vec![FrameSpan::new(self.debugee, ecx.location())?];
        let mut visited_ips = HashSet::new();
        visited_ips.insert(frame_0_location.pc);
        let mut ucx = match mb_ucx {
            Some(u) => u,
            #[cfg(target_arch = "aarch64")]
            None => {
                // Frame-pointer fallback. AArch64 ABI: x29 holds the
                // current frame's FP, which points to a 16-byte
                // record `[saved_fp, saved_lr]` at the caller's
                // stack frame top. Walk it until either:
                //   * fp == 0 (bottom of stack),
                //   * lr resolves to a PC inside a tracked dylib —
                //     re-arm DWARF unwinding from there,
                //   * loop / depth limit hit.
                if let Some(u) = self.fp_walk_into_dwarf(
                    &raw_registers,
                    &mut bt,
                    &mut visited_ips,
                    frame_0_location.pid,
                )? {
                    u
                } else {
                    return Ok(bt);
                }
            }
            #[cfg(not(target_arch = "aarch64"))]
            None => return Ok(bt),
        };
        ecx = ExplorationContext::new(ucx.location, bt.len() as u32 - 1);

        // start unwind
        while let Some(return_addr) = ucx.return_address() {
            if bt.len() >= MAX_UNWIND_DEPTH {
                warn!(
                    target: "debugger",
                    "unwind depth limit {MAX_UNWIND_DEPTH} reached, stopping at {return_addr}"
                );
                break;
            }

            if !visited_ips.insert(return_addr) {
                break;
            }

            let global_pc = match return_addr.into_global(self.debugee) {
                Ok(gpc) => gpc,
                Err(Error::MappingOffsetNotFound(_)) => {
                    // Address is outside any known mapped region (e.g. vDSO,
                    // dynamic linker trampoline, or bottom-of-stack sentinel).
                    // This is a normal unwind termination condition.
                    break;
                }
                Err(e) => return Err(e),
            };

            let next_location = Location {
                pc: return_addr,
                global_pc,
                pid: ucx.location.pid,
            };

            ecx = ExplorationContext::new(next_location, ecx.frame_num() + 1);
            ucx = match UnwindContext::next(ucx, &ecx)? {
                None => break,
                Some(ucx) => ucx,
            };

            let span = FrameSpan::new(self.debugee, next_location)?;
            bt.push(span);
        }

        Ok(bt)
    }

    pub fn restore_registers_at_frame(
        &self,
        pid: Pid,
        registers: &mut DwarfRegisterMap,
        frame_num: u32,
    ) -> Result<(), Error> {
        let frame_0_location = self
            .debugee
            .tracee_ctl()
            .tracee_ensure(pid)
            .location(self.debugee)?;
        let mut ecx = ExplorationContext::new(frame_0_location, 0);

        if frame_num == 0 {
            return Ok(());
        }

        let mut unwind_ucx = UnwindContext::new(
            self.debugee,
            DwarfRegisterMap::from(RegisterMap::current(ecx.pid_on_focus())?),
            &ecx,
        )?
        .ok_or(UnwindNoContext)?;

        for _ in 0..frame_num {
            let ret_addr = unwind_ucx.return_address().ok_or(UnwindTooDeepFrame)?;

            ecx = ExplorationContext::new(
                Location {
                    pc: ret_addr,
                    global_pc: ret_addr.into_global(self.debugee)?,
                    pid: ecx.pid_on_focus(),
                },
                ecx.frame_num() + 1,
            );

            unwind_ucx = UnwindContext::next(unwind_ucx, &ecx)?.ok_or(UnwindNoContext)?;
        }

        let unwind_registers = unwind_ucx.registers();
        registers.update_from(&unwind_registers);

        Ok(())
    }

    /// Returns return address for stopped thread.
    ///
    /// # Arguments
    ///
    /// * `pid`: pid of stopped thread.
    pub fn return_address(&self, pid: Pid) -> Result<Option<RelocatedAddress>, Error> {
        let frame_0_location = self
            .debugee
            .tracee_ctl()
            .tracee_ensure(pid)
            .location(self.debugee)?;
        let ecx = ExplorationContext::new(frame_0_location, 0);

        let mb_ucx = UnwindContext::new(
            self.debugee,
            DwarfRegisterMap::from(RegisterMap::current(ecx.pid_on_focus())?),
            &ecx,
        )?;

        if let Some(ucx) = mb_ucx {
            return Ok(ucx.return_address());
        }
        Ok(None)
    }

    /// Returns unwind context for location.
    ///
    /// # Arguments
    ///
    /// * `location`: some debugee thread position.
    pub fn context_for(
        &self,
        ecx: &ExplorationContext,
    ) -> Result<Option<UnwindContext<'_>>, Error> {
        UnwindContext::new(
            self.debugee,
            DwarfRegisterMap::from(RegisterMap::current(ecx.pid_on_focus())?),
            ecx,
        )
    }
}

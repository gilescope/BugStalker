mod cache;
pub mod fmt;
pub use cache::CallCache;

use super::{
    Debugger, Error, debugee::dwarf::DebugInformation, utils::PopIf, variable::dqe::Literal,
};
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use super::{
    TypeDeclaration,
    address::RelocatedAddress,
    debugee::dwarf::r#type::ComplexType,
    register::{Register, RegisterMap},
};
use crate::{
    debugger::{
        FunctionInfo,
        debugee::dwarf::unit::die_ref::{FatDieRef, Function},
    },
    weak_error,
};
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use crate::{
    debugger::{context::gcx, read_memory_by_pid},
    disable_when_not_stared,
};
#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "aarch64", target_os = "linux")
))]
use crate::debugger::utils;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use log::debug;
#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "aarch64", target_os = "linux")
))]
use nix::sys::{self, signal::Signal, wait::WaitStatus};
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use std::rc::Rc;

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("Invalid argument count, expect {0}, got {1}")]
    InvalidArgumentCount(usize, usize),
    #[error("At most 6 8-byte arguments allowed at this moment")]
    TooManyArguments,
    #[error("`{0}` literal type are not supported")]
    UnsupportedLiteral(&'static str),
    #[error("Type of argument {0} is unknown")]
    UnknownArgumentType(usize),
    #[error("The conversion of literal {0} to argument of type {1} is not allowed")]
    LiteralCast(usize, String),
    #[error("Argument {0} of type {0} is unsupported")]
    UnsupportedArgumentType(usize, String),
    #[error("Function not found or too many candidates")]
    FunctionNotFoundOrTooMany,
    #[error("mmap call failed")]
    Mmap,
    #[error("munmap call failed")]
    Munmap,
    #[error("JMP instruction failed")]
    Jmp,
}

/// Use general registers or floating point registers.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[derive(Clone, Copy)]
enum RegType {
    General,
    #[allow(unused)]
    Floating,
}

/// Function call arguments.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[derive(Default)]
pub(super) struct CallArgs(Box<[(u64, RegType)]>);

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn liter_to_arg_bin_repr(
    no: usize,
    lit: &Literal,
    to_type: &ComplexType,
) -> Result<(u64, RegType), CallError> {
    let root_type_id = to_type.root();
    let r#type = &to_type.types[&root_type_id];

    macro_rules! unsup_arg_bail {
        ($no: expr, $t: expr, $root: expr) => {
            return Err(CallError::UnsupportedArgumentType(
                $no,
                $t.identity($root).name_fmt().to_string(),
            ))
        };
    }

    macro_rules! lit_cast_bail {
        ($no: expr, $t: expr, $root: expr) => {
            return Err(CallError::LiteralCast(
                $no,
                $t.identity($root).name_fmt().to_string(),
            ))
        };
    }

    Ok(match lit {
        Literal::String(_) => return Err(CallError::UnsupportedLiteral("string")),
        Literal::Int(val) => {
            let TypeDeclaration::Scalar(scalar_type) = r#type else {
                lit_cast_bail!(no, to_type, root_type_id)
            };

            let Some(encoding) = scalar_type.encoding else {
                return Err(CallError::UnknownArgumentType(no));
            };

            let mut bytes = [0u8; 8];
            match encoding {
                gimli::DW_ATE_signed_char => {
                    let int8: i8 = *val as i8;
                    bytes[0] = int8 as u8;
                }
                gimli::DW_ATE_unsigned_char => {
                    bytes[0] = *val as u8;
                }
                gimli::DW_ATE_signed => {
                    match scalar_type.byte_size.unwrap_or(0) {
                        1 => {
                            let int8: i8 = *val as i8;
                            bytes[0] = int8 as u8;
                        }
                        2 => {
                            let int16: i16 = *val as i16;
                            let b = int16.to_le_bytes();
                            bytes[..2].copy_from_slice(&b);
                        }
                        4 => {
                            let int32: i32 = *val as i32;
                            let b = int32.to_le_bytes();
                            bytes[..4].copy_from_slice(&b);
                        }
                        8 => {
                            bytes.copy_from_slice(&((*val).to_ne_bytes()));
                        }
                        _ => unsup_arg_bail!(no, to_type, root_type_id),
                    };
                }
                gimli::DW_ATE_unsigned => match scalar_type.byte_size.unwrap_or(0) {
                    1 => {
                        bytes[0] = *val as u8;
                    }
                    2 => {
                        let b = (*val as u16).to_le_bytes();
                        bytes[..2].copy_from_slice(&b);
                    }
                    4 => {
                        let b = (*val as u32).to_le_bytes();
                        bytes[..4].copy_from_slice(&b);
                    }
                    8 => {
                        bytes.copy_from_slice(&((*val as u64).to_le_bytes()));
                    }
                    _ => unsup_arg_bail!(no, to_type, root_type_id),
                },
                _ => {
                    lit_cast_bail!(no, to_type, root_type_id)
                }
            };

            (u64::from_le_bytes(bytes), RegType::General)
        }
        Literal::Float(_) => return Err(CallError::UnsupportedLiteral("float")),
        Literal::Address(addr) => {
            let TypeDeclaration::Pointer { .. } = r#type else {
                lit_cast_bail!(no, to_type, root_type_id)
            };

            (*addr as u64, RegType::General)
        }
        Literal::Bool(val) => {
            let TypeDeclaration::Scalar(scalar_type) = r#type else {
                lit_cast_bail!(no, to_type, root_type_id)
            };

            if scalar_type.encoding != Some(gimli::DW_ATE_boolean) {
                lit_cast_bail!(no, to_type, root_type_id)
            }

            (*val as u64, RegType::General)
        }
        Literal::EnumVariant(_, _) => return Err(CallError::UnsupportedLiteral("enum")),
        Literal::Array(_) => return Err(CallError::UnsupportedLiteral("array")),
        Literal::AssocArray(_) => return Err(CallError::UnsupportedLiteral("assoc array")),
    })
}

/// Map argument to the register according to System V AMD64 ABI.
#[cfg(target_arch = "x86_64")]
fn get_reg_for_no(no: usize, reg_type: RegType) -> Register {
    match (no, reg_type) {
        (0, RegType::General) => Register::Rdi,
        (1, RegType::General) => Register::Rsi,
        (2, RegType::General) => Register::Rdx,
        (3, RegType::General) => Register::Rcx,
        (4, RegType::General) => Register::R8,
        (5, RegType::General) => Register::R9,
        _ => unreachable!("unsupported arg no or unknown register"),
    }
}

#[cfg(target_arch = "aarch64")]
fn get_reg_for_no(no: usize, reg_type: RegType) -> Register {
    match (no, reg_type) {
        (0, RegType::General) => Register::X0,
        (1, RegType::General) => Register::X1,
        (2, RegType::General) => Register::X2,
        (3, RegType::General) => Register::X3,
        (4, RegType::General) => Register::X4,
        (5, RegType::General) => Register::X5,
        (6, RegType::General) => Register::X6,
        (7, RegType::General) => Register::X7,
        _ => unreachable!("unsupported arg no or unknown register"),
    }
}

#[cfg(target_arch = "x86_64")]
const MAX_REGISTER_ARGS: usize = 6;
#[cfg(target_arch = "aarch64")]
const MAX_REGISTER_ARGS: usize = 8;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl CallArgs {
    fn new(literals: &[Literal], fn_params: &[Rc<ComplexType>]) -> Result<Self, CallError> {
        if literals.len() != fn_params.len() {
            return Err(CallError::InvalidArgumentCount(
                fn_params.len(),
                literals.len(),
            ));
        }

        if literals.len() > MAX_REGISTER_ARGS {
            return Err(CallError::TooManyArguments);
        }

        let args = literals
            .iter()
            .enumerate()
            .map(|(idx, lit)| liter_to_arg_bin_repr(idx, lit, &fn_params[idx]))
            .collect::<Result<Box<[(u64, RegType)]>, CallError>>()?;

        Ok(CallArgs(args))
    }

    /// Fill registers with arguments.
    fn prepare_registers(self, reg_map: &mut RegisterMap) {
        debug_assert!(self.0.len() <= MAX_REGISTER_ARGS);
        for (idx, (val, reg_type)) in self.0.iter().enumerate() {
            reg_map.update(get_reg_for_no(idx, *reg_type), *val);
        }
    }
}

/// Call context (or ccx). Program state before a call.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
struct CallContext<'a> {
    dbg: &'a Debugger,
    pid: nix::unistd::Pid,
    pc: RelocatedAddress,
    regs: RegisterMap,
    text: usize,
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl<'a> CallContext<'a> {
    fn new(dbg: &'a Debugger) -> Result<Self, Error> {
        let pid = dbg.ecx().pid_on_focus();
        let pc = dbg.ecx().location().pc;
        let text = read_memory_by_pid(pid, pc.into(), size_of::<u64>()).map_err(Error::Ptrace)?;
        let text = usize::from_ne_bytes(text.try_into().expect("unexpected size"));
        let regs = RegisterMap::current(pid)?;

        Ok(Self {
            dbg,
            pid,
            pc,
            regs,
            text,
        })
    }

    fn retrieve_original_state(self) -> Result<(), Error> {
        self.regs.clone().persist(self.pid)?; // TODO clone
        self.dbg.write_memory(self.pc.as_usize(), self.text)?;
        Ok(())
    }

    fn with_ccx<F, T>(mut self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut Self) -> Result<T, Error>,
    {
        let result = f(&mut self);

        debug!(target: "debugger", "retrieve original registers and instructions");
        self.retrieve_original_state()
            .expect("failed to retrieve original program state after a call");

        result
    }
}

struct CallHelper;

#[cfg(target_arch = "x86_64")]
impl CallHelper {
    fn call_fn(ccx: &CallContext, rip: u64, fn_addr: u64, args: CallArgs) -> Result<(), Error> {
        // new text:
        // FF D0 - CALL %rax
        // CC - break
        const CALL_FN: usize = 0xFFusize | (0xD0usize << 0x8) | (0xCCusize << 0x10);

        debug!(target: "debugger", "add call instructions");
        ccx.dbg.write_memory(rip as usize, CALL_FN)?;

        debug!(target: "debugger", "prepare function arguments");
        let mut regs: RegisterMap = ccx.regs.clone();
        args.prepare_registers(&mut regs);
        regs.update(Register::Rax, fn_addr);
        regs.update(Register::Rip, rip);
        // System V AMD64 ABI: at the point of `CALL`, RSP must be 16-byte
        // aligned so that on entry to the callee `RSP + 8` is aligned
        // (the callee's prologue compensates for the pushed return
        // address). The debuggee's RSP at the stop point is whatever
        // its compiler arranged for *that* instruction — typically
        // 16-aligned at function-call sites but commonly only 8-aligned
        // mid-function. If we leave it as-is, callees that use
        // alignment-sensitive instructions (movaps/movdqa on SSE
        // locals, e.g. inside Vec::reserve/realloc) take a #GP at a
        // load that happens to land on an odd 8-byte slot — which made
        // `test_debug_trait_repr_vars` flake whenever the breakpoint
        // line happened to leave RSP & 0xf == 8.
        //
        // Round RSP down to 16 bytes (we're allocating into unused
        // scratch below the live frame; ccx.regs is restored after the
        // call so the alignment shim is invisible to the debuggee).
        let aligned_sp = regs.value(Register::Rsp) & !0xfu64;
        regs.update(Register::Rsp, aligned_sp);
        regs.persist(ccx.pid)?;

        debug!(target: "debugger", "call a function, wait until breakpoint are hit");
        sys::ptrace::cont(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(res == WaitStatus::Stopped(ccx.pid, Signal::SIGTRAP));

        Ok(())
    }

    fn jump(ccx: &CallContext, dest_ptr: u64) -> Result<(), Error> {
        debug_assert!(ccx.regs.value(Register::Rip) == ccx.pc.as_u64());

        let mut regs = ccx.regs.clone();
        regs.update(Register::Rax, dest_ptr);
        regs.persist(ccx.pid)?;

        const JMP_RAX: usize = 0x000000000000E0FF;
        const JMP_RAX_MASK: usize = 0xFFFFFFFFFFFF0000;

        let new_text = (ccx.text & JMP_RAX_MASK) | JMP_RAX;

        ccx.dbg.write_memory(ccx.pc.as_usize(), new_text)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        if RegisterMap::current(ccx.pid)?.value(Register::Rip) != dest_ptr {
            return Err(CallError::Jmp.into());
        }

        Ok(())
    }

    fn mmap(ccx: &CallContext) -> Result<u64, Error> {
        debug_assert!(ccx.regs.value(Register::Rip) == ccx.pc.as_u64());

        // Update registers for calling a `mmap` syscall
        let mut regs = ccx.regs.clone();
        const MMAP: u64 = 9;
        const PROT: u64 =
            (nix::libc::PROT_READ | nix::libc::PROT_EXEC | nix::libc::PROT_WRITE) as u64;
        const FLAGS: u64 = (nix::libc::MAP_PRIVATE | nix::libc::MAP_ANONYMOUS) as u64;
        regs.update(Register::Rax, MMAP);
        regs.update(Register::Rdi, 0);
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as u64 };
        regs.update(Register::Rsi, page_size);
        regs.update(Register::Rdx, PROT);
        regs.update(Register::R10, FLAGS);
        regs.update(Register::R8, -1i32 as u64);
        regs.update(Register::R9, 0);

        regs.persist(ccx.pid)?;

        const SYSCALL: usize = 0x000000000000050F;
        const SYSCALL_MASK: usize = 0xFFFFFFFFFFFF0000;

        let new_instructions = (ccx.text & SYSCALL_MASK) | SYSCALL;

        ccx.dbg.write_memory(ccx.pc.as_usize(), new_instructions)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        let regs = RegisterMap::current(ccx.pid)?;
        let alloc_ptr: u64 = regs.value(Register::Rax);
        if alloc_ptr as i64 == -1 {
            return Err(CallError::Mmap.into());
        }

        debug_assert!(utils::region_exist(ccx.pid, alloc_ptr)?);

        Ok(alloc_ptr)
    }

    fn munmap(ccx: &CallContext, addr: u64) -> Result<(), Error> {
        const SYSCALL: usize = 0x000000000000050F;
        const SYSCALL_MASK: usize = 0xFFFFFFFFFFFF0000;

        let new_text = (ccx.text & SYSCALL_MASK) | SYSCALL;
        ccx.dbg.write_memory(ccx.pc.as_usize(), new_text)?;

        // Update registers for calling a `munmap` syscall
        let mut regs = ccx.regs.clone();
        const MUNMAP: u64 = 11;
        regs.update(Register::Rax, MUNMAP);
        regs.update(Register::Rdi, addr);
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as u64 };
        regs.update(Register::Rsi, page_size);
        regs.persist(ccx.pid)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        let regs: RegisterMap = RegisterMap::current(ccx.pid)?;
        if regs.value(Register::Rax) != 0 {
            return Err(CallError::Munmap.into());
        }
        debug_assert!(utils::region_non_exist(ccx.pid, addr)?);

        ccx.dbg.write_memory(ccx.pc.as_usize(), ccx.text)?;

        Ok(())
    }
}

// aarch64 CallHelper still uses ptrace::cont / ptrace::step to
// drive the trampoline (cont-until-BRK + single-step). That works
// on linux/aarch64 but is incompatible with the darwin/aarch64
// pure-Mach Tracer cutover (`Child::install` no longer calls
// PT_TRACE_ME, so the inferior isn't in a ptrace relationship and
// these ptrace ops fail). A Mach-native CallHelper for darwin
// would need to allocate a temporary exception port, swap it in
// over the Tracer's port via task_set_exception_ports (saving the
// original via the LLDB-style SaveExceptionPortInfo pattern),
// drive task_resume / arm_set_single_step + port.receive for each
// trampoline step, then restore. Deferred — see roadmap.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
impl CallHelper {
    fn call_fn(ccx: &CallContext, pc: u64, fn_addr: u64, args: CallArgs) -> Result<(), Error> {
        const BLR_X8_BRK0: usize = 0xD420_0000usize << 32 | 0xD63F_0100usize;

        debug!(target: "debugger", "add call instructions");
        ccx.dbg.write_memory(pc as usize, BLR_X8_BRK0)?;

        debug!(target: "debugger", "prepare function arguments");
        let mut regs: RegisterMap = ccx.regs.clone();
        args.prepare_registers(&mut regs);
        regs.update(Register::X8, fn_addr);
        regs.update(Register::Pc, pc);
        regs.persist(ccx.pid)?;

        debug!(target: "debugger", "call a function, wait until breakpoint are hit");
        sys::ptrace::cont(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(res == WaitStatus::Stopped(ccx.pid, Signal::SIGTRAP));

        Ok(())
    }

    fn jump(ccx: &CallContext, dest_ptr: u64) -> Result<(), Error> {
        debug_assert!(ccx.regs.value(Register::Pc) == ccx.pc.as_u64());

        let mut regs = ccx.regs.clone();
        regs.update(Register::X8, dest_ptr);
        regs.persist(ccx.pid)?;

        const BR_X8: usize = 0xD61F_0100;
        const BR_X8_MASK: usize = 0xFFFF_FFFF_0000_0000;

        let new_text = (ccx.text & BR_X8_MASK) | BR_X8;

        ccx.dbg.write_memory(ccx.pc.as_usize(), new_text)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        if RegisterMap::current(ccx.pid)?.value(Register::Pc) != dest_ptr {
            return Err(CallError::Jmp.into());
        }

        Ok(())
    }

    fn mmap(ccx: &CallContext) -> Result<u64, Error> {
        debug_assert!(ccx.regs.value(Register::Pc) == ccx.pc.as_u64());

        let mut regs = ccx.regs.clone();
        const PROT: u64 =
            (nix::libc::PROT_READ | nix::libc::PROT_EXEC | nix::libc::PROT_WRITE) as u64;
        const FLAGS: u64 = (nix::libc::MAP_PRIVATE | nix::libc::MAP_ANONYMOUS) as u64;
        regs.update(syscall_abi::NR_REG, syscall_abi::NR_MMAP);
        regs.update(Register::X0, 0);
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as u64 };
        regs.update(Register::X1, page_size);
        regs.update(Register::X2, PROT);
        regs.update(Register::X3, FLAGS);
        regs.update(Register::X4, -1i32 as u64);
        regs.update(Register::X5, 0);

        regs.persist(ccx.pid)?;

        let new_instructions = (ccx.text & syscall_abi::SVC_MASK) | syscall_abi::SVC_INSTR;

        ccx.dbg.write_memory(ccx.pc.as_usize(), new_instructions)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        let regs = RegisterMap::current(ccx.pid)?;
        let alloc_ptr: u64 = regs.value(Register::X0);
        if syscall_abi::is_syscall_error(&regs, alloc_ptr) {
            return Err(CallError::Mmap.into());
        }

        debug_assert!(utils::region_exist(ccx.pid, alloc_ptr)?);

        Ok(alloc_ptr)
    }

    fn munmap(ccx: &CallContext, addr: u64) -> Result<(), Error> {
        let new_text = (ccx.text & syscall_abi::SVC_MASK) | syscall_abi::SVC_INSTR;
        ccx.dbg.write_memory(ccx.pc.as_usize(), new_text)?;

        let mut regs = ccx.regs.clone();
        regs.update(syscall_abi::NR_REG, syscall_abi::NR_MUNMAP);
        regs.update(Register::X0, addr);
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as u64 };
        regs.update(Register::X1, page_size);
        regs.persist(ccx.pid)?;

        sys::ptrace::step(ccx.pid, None).map_err(Error::Ptrace)?;
        let res = nix::sys::wait::waitpid(ccx.pid, None).map_err(Error::Waitpid)?;
        debug_assert!(matches!(res, WaitStatus::Stopped(_, _)));

        let regs: RegisterMap = RegisterMap::current(ccx.pid)?;
        if syscall_abi::is_syscall_error(&regs, regs.value(Register::X0)) {
            return Err(CallError::Munmap.into());
        }
        debug_assert!(utils::region_non_exist(ccx.pid, addr)?);

        ccx.dbg.write_memory(ccx.pc.as_usize(), ccx.text)?;

        Ok(())
    }
}

/// Darwin/aarch64 stub CallHelper. Inferior function calls
/// (`vard`, `argd`, `fmt::call_debug_fmt`, `Debugger::call`) are
/// not yet wired to the Mach exception-port loop — the linux
/// impl above uses ptrace::cont/step which is incompatible with
/// our pure-Mach Tracer cutover. Returning a clear `Mmap` error
/// (the first step the caller takes) keeps the engine's state
/// machine intact and surfaces the gap to the user instead of
/// hanging or corrupting the inferior.
#[cfg(all(target_arch = "aarch64", not(target_os = "linux")))]
impl CallHelper {
    fn call_fn(_ccx: &CallContext, _pc: u64, _fn_addr: u64, _args: CallArgs) -> Result<(), Error> {
        Err(CallError::Mmap.into())
    }
    fn jump(_ccx: &CallContext, _dest_ptr: u64) -> Result<(), Error> {
        Err(CallError::Jmp.into())
    }
    fn mmap(_ccx: &CallContext) -> Result<u64, Error> {
        Err(CallError::Mmap.into())
    }
    fn munmap(_ccx: &CallContext, _addr: u64) -> Result<(), Error> {
        Err(CallError::Munmap.into())
    }
}

/// aarch64 OS-specific syscall ABI bits. Both linux and darwin run
/// the AArch64 architecture, but the syscall convention is OS, not
/// arch:
///
/// |               | linux            | darwin           |
/// |---------------|------------------|------------------|
/// | nr register   | x8               | x16              |
/// | trap insn     | `svc #0`         | `svc #0x80`      |
/// | mmap nr       | 222              | 197 (BSD)        |
/// | munmap nr     | 215              | 73  (BSD)        |
/// | error signal  | x0 = -errno      | CPSR.C set, x0=errno |
///
/// The encoding for `svc #imm16` is
/// `1101_0100_000_imm16_0000_1`, so:
/// * `svc #0`     → `0xD400_0001`
/// * `svc #0x80`  → `0xD400_0001 | (0x80 << 5)` = `0xD400_1001`
#[cfg(target_arch = "aarch64")]
mod syscall_abi {
    use super::{Register, RegisterMap};

    #[cfg(target_os = "linux")]
    pub const NR_REG: Register = Register::X8;
    #[cfg(not(target_os = "linux"))]
    pub const NR_REG: Register = Register::X16;

    #[cfg(target_os = "linux")]
    pub const NR_MMAP: u64 = 222;
    #[cfg(target_os = "linux")]
    pub const NR_MUNMAP: u64 = 215;

    #[cfg(not(target_os = "linux"))]
    pub const NR_MMAP: u64 = 197;
    #[cfg(not(target_os = "linux"))]
    pub const NR_MUNMAP: u64 = 73;

    #[cfg(target_os = "linux")]
    pub const SVC_INSTR: usize = 0xD400_0001;
    #[cfg(not(target_os = "linux"))]
    pub const SVC_INSTR: usize = 0xD400_1001;
    pub const SVC_MASK: usize = 0xFFFF_FFFF_0000_0000;

    /// linux: raw syscall return is `-errno` on failure; valid mmap
    /// addresses are large positive values, so `x0 == -1` means
    /// `EPERM` (or any address-as-MAP_FAILED).  Stricter checks
    /// would inspect the full negative range; we keep the original
    /// liberal check for behavioural parity.
    #[cfg(target_os = "linux")]
    pub fn is_syscall_error(_regs: &RegisterMap, x0: u64) -> bool {
        x0 as i64 == -1
    }

    /// darwin BSD syscall: success → `CPSR.C = 0`, x0 holds the
    /// result; failure → `CPSR.C = 1`, x0 holds the errno (positive).
    /// Read CPSR via the `pstate` slot of the register map; the C
    /// flag lives at bit 29 of NZCV.
    #[cfg(not(target_os = "linux"))]
    pub fn is_syscall_error(regs: &RegisterMap, _x0: u64) -> bool {
        regs.value(Register::Pstate) & (1u64 << 29) != 0
    }
}

impl Debugger {
    pub(crate) fn search_fn_to_call(
        &self,
        linkage_name_tpl: &str,
        name: Option<&str>,
    ) -> Result<(&DebugInformation, FatDieRef<'_, Function>, &FunctionInfo), CallError> {
        let dwarfs = self.debugee.debug_info_all();

        let mut candidates = dwarfs
            .iter()
            .filter(|dwarf| {
                dwarf.has_debug_info() && dwarf.tpl_in_pub_names(linkage_name_tpl) != Some(false)
            })
            .filter_map(|&dwarf| {
                let funcs = weak_error!(dwarf.search_functions(linkage_name_tpl))?;
                if funcs.is_empty() {
                    return None;
                }
                Some((dwarf, funcs))
            })
            .collect::<Vec<_>>();

        candidates
            .pop_if_cond(|c| c.len() == 1)
            .and_then(|(dwarf, mut funcs)| {
                if name.is_some() {
                    funcs.retain(|(_, info)| info.name.as_deref() == name);
                }

                funcs.retain(|(f, _)| {
                    let low = f
                        .prolog_start_place()
                        .map(|p| usize::from(p.address))
                        .unwrap_or_default();
                    low != 0
                });

                let (f_ref, info) = funcs.pop()?;
                Some((
                    dwarf, // TODO take first suitable, is this a good approach?
                    f_ref, info,
                ))
            })
            .ok_or(CallError::FunctionNotFoundOrTooMany)
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn with_disabled_brkpts<F>(&self, f: F) -> Result<(), Error>
    where
        F: FnOnce(&Self) -> Result<(), Error>,
    {
        debug!(target: "debugger", "disable all active breakpoints");
        for brkpt in self.breakpoints.active_breakpoints() {
            brkpt.disable()?;
        }

        let cb_result = f(self);

        debug!(target: "debugger", "enable all active breakpoints");
        for brkpt in self.breakpoints.active_breakpoints() {
            brkpt
                .enable()
                .expect("enable breakpoint after disable should not leads to error");
        }

        cb_result
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub(super) fn call_fn_raw(&self, fn_addr: RelocatedAddress, args: CallArgs) -> Result<(), Error> {
        let call_context = CallContext::new(self)?;

        call_context.with_ccx(|ccx| {
            debug!(target: "debugger", "alloc temporary memory area");
            let alloc_ptr = CallHelper::mmap(ccx)?;

            debug!(target: "debugger", "jump into mmap'ed region");
            CallHelper::jump(ccx, alloc_ptr)?;

            debug!(target: "debugger", "call a given function");
            CallHelper::call_fn(ccx, alloc_ptr, fn_addr.as_u64(), args)?;

            debug!(target: "debugger", "going to original rip");
            ccx.regs.clone().persist(ccx.pid)?;

            debug!(target: "debugger", "dealloc temporary memory area");
            CallHelper::munmap(ccx, alloc_ptr)?;

            Ok(())
        })
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn call_fn(&self, linkage_name: &str, arguments: &[Literal]) -> Result<(), Error> {
        debug!(target: "debugger", "find function address and prepare arguments");

        let fn_info = gcx().with_call_cache(|cc| cc.get_or_insert(self, linkage_name, None))?;

        let args = CallArgs::new(arguments, fn_info.fn_param_types())?;
        self.call_fn_raw(fn_info.fn_addr(), args)
    }

    /// Do a function call.
    ///
    /// # Arguments
    ///
    /// * `fn_name`: function to call.
    /// * `arguments`: list of literals.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn call(&mut self, fn_name: &str, arguments: &[Literal]) -> Result<(), Error> {
        disable_when_not_stared!(self);

        self.with_disabled_brkpts(|dbg| dbg.call_fn(fn_name, arguments))
    }
}

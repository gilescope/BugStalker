use crate::debugger::error::Error;
use crate::debugger::error::Error::{Ptrace, RegisterNotFound};
use gimli::Register as DwarfRegister;
use nix::errno::Errno;
use nix::libc::{self, c_void, iovec, user_regs_struct};
use nix::unistd::Pid;
use smallvec::{SmallVec, smallvec};
use std::mem::MaybeUninit;
use strum_macros::Display;
use strum_macros::EnumString;

/// aarch64 registers.
///
/// DWARF register numbering follows "DWARF for the ARM 64-bit Architecture
/// (AArch64)" (ARM IHI 0057): X0..X30 => 0..30, SP => 31, PC => 32.
/// PSTATE has no standard DWARF number.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, EnumString, Display)]
#[strum(serialize_all = "snake_case")]
pub enum Register {
    X0,
    X1,
    X2,
    X3,
    X4,
    X5,
    X6,
    X7,
    X8,
    X9,
    X10,
    X11,
    X12,
    X13,
    X14,
    X15,
    X16,
    X17,
    X18,
    X19,
    X20,
    X21,
    X22,
    X23,
    X24,
    X25,
    X26,
    X27,
    X28,
    X29,
    X30,
    Sp,
    Pc,
    Pstate,
}

impl Register {
    /// Architecture-agnostic alias for the stack pointer.
    pub const SP: Register = Register::Sp;
    /// Architecture-agnostic alias for the program counter.
    pub const PC: Register = Register::Pc;

    pub fn dwarf_register(self) -> Option<DwarfRegister> {
        let n = match self {
            Register::X0 => 0,
            Register::X1 => 1,
            Register::X2 => 2,
            Register::X3 => 3,
            Register::X4 => 4,
            Register::X5 => 5,
            Register::X6 => 6,
            Register::X7 => 7,
            Register::X8 => 8,
            Register::X9 => 9,
            Register::X10 => 10,
            Register::X11 => 11,
            Register::X12 => 12,
            Register::X13 => 13,
            Register::X14 => 14,
            Register::X15 => 15,
            Register::X16 => 16,
            Register::X17 => 17,
            Register::X18 => 18,
            Register::X19 => 19,
            Register::X20 => 20,
            Register::X21 => 21,
            Register::X22 => 22,
            Register::X23 => 23,
            Register::X24 => 24,
            Register::X25 => 25,
            Register::X26 => 26,
            Register::X27 => 27,
            Register::X28 => 28,
            Register::X29 => 29,
            Register::X30 => 30,
            Register::Sp => 31,
            Register::Pc => 32,
            Register::Pstate => return None,
        };
        Some(DwarfRegister(n))
    }
}

impl From<gimli::Register> for Register {
    fn from(value: gimli::Register) -> Self {
        match value.0 as i32 {
            -1 => Register::Pc,
            0 => Register::X0,
            1 => Register::X1,
            2 => Register::X2,
            3 => Register::X3,
            4 => Register::X4,
            5 => Register::X5,
            6 => Register::X6,
            7 => Register::X7,
            8 => Register::X8,
            9 => Register::X9,
            10 => Register::X10,
            11 => Register::X11,
            12 => Register::X12,
            13 => Register::X13,
            14 => Register::X14,
            15 => Register::X15,
            16 => Register::X16,
            17 => Register::X17,
            18 => Register::X18,
            19 => Register::X19,
            20 => Register::X20,
            21 => Register::X21,
            22 => Register::X22,
            23 => Register::X23,
            24 => Register::X24,
            25 => Register::X25,
            26 => Register::X26,
            27 => Register::X27,
            28 => Register::X28,
            29 => Register::X29,
            30 => Register::X30,
            31 => Register::Sp,
            32 => Register::Pc,
            n => panic!("unknown dwarf register number {n}"),
        }
    }
}

/// aarch64 register values.
#[derive(Debug, Clone)]
pub struct RegisterMap {
    regs: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
}

impl From<user_regs_struct> for RegisterMap {
    fn from(v: user_regs_struct) -> Self {
        Self {
            regs: v.regs,
            sp: v.sp,
            pc: v.pc,
            pstate: v.pstate,
        }
    }
}

impl From<RegisterMap> for user_regs_struct {
    fn from(m: RegisterMap) -> Self {
        user_regs_struct {
            regs: m.regs,
            sp: m.sp,
            pc: m.pc,
            pstate: m.pstate,
        }
    }
}

fn ptrace_regset(request: libc::c_uint, pid: Pid, regs: &mut user_regs_struct) -> Result<(), Error> {
    let mut iov = iovec {
        iov_base: regs as *mut _ as *mut c_void,
        iov_len: std::mem::size_of::<user_regs_struct>(),
    };
    // NT_PRSTATUS = 1 — general-purpose register set.
    let ret = unsafe {
        libc::ptrace(
            request,
            pid.as_raw(),
            1usize as *mut c_void,
            &mut iov as *mut _ as *mut c_void,
        )
    };
    if ret < 0 {
        return Err(Ptrace(Errno::last()));
    }
    Ok(())
}

impl RegisterMap {
    pub fn current(pid: Pid) -> Result<Self, Error> {
        let mut regs = MaybeUninit::<user_regs_struct>::zeroed();
        unsafe {
            ptrace_regset(libc::PTRACE_GETREGSET, pid, regs.assume_init_mut())?;
            Ok(regs.assume_init().into())
        }
    }

    /// Architecture-agnostic program counter accessor (aarch64: `pc`).
    pub fn pc(&self) -> u64 {
        self.pc
    }

    /// Architecture-agnostic program counter setter (aarch64: `pc`).
    pub fn set_pc(&mut self, value: u64) {
        self.pc = value;
    }

    /// Architecture-agnostic stack pointer accessor (aarch64: `sp`).
    pub fn sp(&self) -> u64 {
        self.sp
    }

    /// Architecture-agnostic stack pointer setter (aarch64: `sp`).
    pub fn set_sp(&mut self, value: u64) {
        self.sp = value;
    }

    pub fn value(&self, register: impl Into<Register>) -> u64 {
        let r = register.into();
        match r {
            Register::X0 => self.regs[0],
            Register::X1 => self.regs[1],
            Register::X2 => self.regs[2],
            Register::X3 => self.regs[3],
            Register::X4 => self.regs[4],
            Register::X5 => self.regs[5],
            Register::X6 => self.regs[6],
            Register::X7 => self.regs[7],
            Register::X8 => self.regs[8],
            Register::X9 => self.regs[9],
            Register::X10 => self.regs[10],
            Register::X11 => self.regs[11],
            Register::X12 => self.regs[12],
            Register::X13 => self.regs[13],
            Register::X14 => self.regs[14],
            Register::X15 => self.regs[15],
            Register::X16 => self.regs[16],
            Register::X17 => self.regs[17],
            Register::X18 => self.regs[18],
            Register::X19 => self.regs[19],
            Register::X20 => self.regs[20],
            Register::X21 => self.regs[21],
            Register::X22 => self.regs[22],
            Register::X23 => self.regs[23],
            Register::X24 => self.regs[24],
            Register::X25 => self.regs[25],
            Register::X26 => self.regs[26],
            Register::X27 => self.regs[27],
            Register::X28 => self.regs[28],
            Register::X29 => self.regs[29],
            Register::X30 => self.regs[30],
            Register::Sp => self.sp,
            Register::Pc => self.pc,
            Register::Pstate => self.pstate,
        }
    }

    pub fn update(&mut self, register: impl Into<Register>, value: u64) {
        match register.into() {
            Register::X0 => self.regs[0] = value,
            Register::X1 => self.regs[1] = value,
            Register::X2 => self.regs[2] = value,
            Register::X3 => self.regs[3] = value,
            Register::X4 => self.regs[4] = value,
            Register::X5 => self.regs[5] = value,
            Register::X6 => self.regs[6] = value,
            Register::X7 => self.regs[7] = value,
            Register::X8 => self.regs[8] = value,
            Register::X9 => self.regs[9] = value,
            Register::X10 => self.regs[10] = value,
            Register::X11 => self.regs[11] = value,
            Register::X12 => self.regs[12] = value,
            Register::X13 => self.regs[13] = value,
            Register::X14 => self.regs[14] = value,
            Register::X15 => self.regs[15] = value,
            Register::X16 => self.regs[16] = value,
            Register::X17 => self.regs[17] = value,
            Register::X18 => self.regs[18] = value,
            Register::X19 => self.regs[19] = value,
            Register::X20 => self.regs[20] = value,
            Register::X21 => self.regs[21] = value,
            Register::X22 => self.regs[22] = value,
            Register::X23 => self.regs[23] = value,
            Register::X24 => self.regs[24] = value,
            Register::X25 => self.regs[25] = value,
            Register::X26 => self.regs[26] = value,
            Register::X27 => self.regs[27] = value,
            Register::X28 => self.regs[28] = value,
            Register::X29 => self.regs[29] = value,
            Register::X30 => self.regs[30] = value,
            Register::Sp => self.sp = value,
            Register::Pc => self.pc = value,
            Register::Pstate => self.pstate = value,
        };
    }

    pub fn persist(self, pid: Pid) -> Result<(), Error> {
        let mut regs: user_regs_struct = self.into();
        ptrace_regset(libc::PTRACE_SETREGSET, pid, &mut regs)
    }
}

/// DWARF-indexed register map.
#[derive(Debug, Clone)]
pub struct DwarfRegisterMap(SmallVec<[Option<u64>; 0x80]>);

impl DwarfRegisterMap {
    pub fn value(&self, register: gimli::Register) -> Result<u64, Error> {
        self.0
            .get(register.0 as usize)
            .copied()
            .and_then(|v| v)
            .ok_or(RegisterNotFound(register))
    }

    pub fn update(&mut self, register: gimli::Register, value: u64) {
        self.0[register.0 as usize] = Some(value);
    }

    pub fn update_from(&mut self, other: &Self) {
        for (idx, value) in other.0.iter().enumerate() {
            if let Some(value) = value {
                self.0[idx] = Some(*value);
            }
        }
    }
}

impl From<RegisterMap> for DwarfRegisterMap {
    fn from(map: RegisterMap) -> Self {
        let mut dwarf_map = smallvec![None; 0x80];
        for (i, v) in map.regs.iter().enumerate() {
            dwarf_map.insert(i, Some(*v));
        }
        dwarf_map.insert(31, Some(map.sp));
        dwarf_map.insert(32, Some(map.pc));
        DwarfRegisterMap(dwarf_map)
    }
}

pub mod debug_impl {
    //! Hardware watchpoints via aarch64's `NT_ARM_HW_WATCH`/`NT_ARM_HW_BREAK`
    //! regsets are not yet implemented. For now this module exposes the same
    //! `HardwareDebugState` shape the rest of the debugger expects and
    //! returns `Err(WatchpointOOM)` at runtime, making hardware watchpoints
    //! unavailable but keeping the debugger otherwise operational.

    use crate::debugger::Error;
    use crate::debugger::register::debug::{DebugControlRegister, DebugStatusRegister};
    use nix::unistd::Pid;

    pub type DebugAddressRegister = usize;

    #[derive(PartialEq, Debug, Default)]
    pub struct HardwareDebugState {
        pub address_regs: [DebugAddressRegister; 4],
        pub dr6: DebugStatusRegister,
        pub dr7: DebugControlRegister,
    }

    impl HardwareDebugState {
        pub fn current(_pid: Pid) -> Result<Self, Error> {
            Err(Error::WatchpointUnsupported)
        }

        pub fn sync(&self, _pid: Pid) -> Result<(), Error> {
            Err(Error::WatchpointUnsupported)
        }
    }
}

use crate::debugger::error::Error;
use crate::debugger::error::Error::RegisterNotFound;
use gimli::Register as DwarfRegister;
use nix::unistd::Pid;
use smallvec::{SmallVec, smallvec};
use strum_macros::Display;
use strum_macros::EnumString;

// Linux exposes the aarch64 GP registers through `PTRACE_GETREGSET`
// with a `nix::libc::user_regs_struct` payload. Darwin uses Mach
// (`thread_get_state(thread, ARM_THREAD_STATE64, …)`) and a
// `arm_thread_state64_t`. Until the Mach-based path lands we just
// stub the darwin side so `cargo check` passes.
#[cfg(target_os = "linux")]
use crate::debugger::error::Error::Ptrace;
#[cfg(target_os = "linux")]
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::libc::{self, c_void, iovec, user_regs_struct};
#[cfg(target_os = "linux")]
use std::mem::MaybeUninit;

// On darwin the kernel-level register struct isn't exposed through
// libc (no `user_regs_struct`); we re-declare an equivalent layout
// so the rest of the file doesn't have to reach for `mach2`/Mach
// types just for the From-impls below.
#[cfg(not(target_os = "linux"))]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
#[allow(non_camel_case_types)]
struct user_regs_struct {
    regs: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
}

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
    /// Architecture-agnostic alias for the return-address register
    /// (the DWARF column the CIE conventionally points its
    /// `return_address_register` field at). aarch64 uses `x30`/LR
    /// for procedure return; x86_64 uses the synthetic RIP column.
    pub const RA: Register = Register::X30;

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

#[cfg(target_os = "linux")]
fn ptrace_regset(request: libc::c_uint, pid: Pid, regs: &mut user_regs_struct) -> Result<(), Error> {
    let mut iov = iovec {
        iov_base: regs as *mut _ as *mut c_void,
        iov_len: std::mem::size_of::<user_regs_struct>(),
    };
    // NT_PRSTATUS = 1 — general-purpose register set. The `addr`
    // argument here is the regset number, not a real pointer, so
    // we use a magic sentinel constant rather than a true address.
    // (clippy::manual_dangling_ptr would suggest dangling_mut, but
    // that's a different sentinel value the kernel doesn't accept.)
    #[allow(clippy::manual_dangling_ptr)]
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
    #[cfg(target_os = "linux")]
    pub fn current(pid: Pid) -> Result<Self, Error> {
        let mut regs = MaybeUninit::<user_regs_struct>::zeroed();
        unsafe {
            ptrace_regset(libc::PTRACE_GETREGSET, pid, regs.assume_init_mut())?;
            Ok(regs.assume_init().into())
        }
    }

    /// Darwin path: read the GP register set via Mach
    /// `thread_get_state(thread, ARM_THREAD_STATE64, …)`.
    ///
    /// The mapping from the Mach `arm_thread_state64_t` layout
    /// (`__x[29]`, `__fp`, `__lr`, `__sp`, `__pc`, `__cpsr`,
    /// `__flags`) to the cross-arch `RegisterMap` (`regs[31]`,
    /// `sp`, `pc`, `pstate`) is:
    ///   * `regs[0..=28]` ← `__x[0..=28]`
    ///   * `regs[29]`     ← `__fp` (= x29 / FP)
    ///   * `regs[30]`     ← `__lr` (= x30 / LR)
    ///   * `sp`           ← `__sp`
    ///   * `pc`           ← `__pc`
    ///   * `pstate`       ← `__cpsr`  (only the low 32 bits are
    ///                                  meaningful; high bits zero)
    ///
    /// `pid` is interpreted as a synthetic per-thread id — the
    /// Tracer registers each Mach thread port under one such id in
    /// `darwin_mach::set_thread_port`. If the registry has no entry
    /// (legacy single-thread callers), we fall back to the first
    /// thread of the inferior's task.
    #[cfg(not(target_os = "linux"))]
    pub fn current(pid: Pid) -> Result<Self, Error> {
        use crate::debugger::darwin_mach;
        let thread = darwin_mach::thread_port_for_pid_or_first(pid)?;
        let s = darwin_mach::thread_get_arm_state64(thread)?;
        let mut regs = [0u64; 31];
        regs[..29].copy_from_slice(&s.__x);
        regs[29] = s.__fp;
        regs[30] = s.__lr;
        Ok(Self {
            regs,
            sp: s.__sp,
            pc: s.__pc,
            pstate: s.__cpsr as u64,
        })
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

    #[cfg(target_os = "linux")]
    pub fn persist(self, pid: Pid) -> Result<(), Error> {
        let mut regs: user_regs_struct = self.into();
        ptrace_regset(libc::PTRACE_SETREGSET, pid, &mut regs)
    }

    /// Darwin path: write the GP register set via Mach
    /// `thread_set_state(thread, ARM_THREAD_STATE64, …)`. Inverse
    /// of `current` above — same per-pid → thread-port lookup.
    #[cfg(not(target_os = "linux"))]
    pub fn persist(self, pid: Pid) -> Result<(), Error> {
        use crate::debugger::darwin_mach;
        use mach2::structs::arm_thread_state64_t;
        let thread = darwin_mach::thread_port_for_pid_or_first(pid)?;
        let mut x = [0u64; 29];
        x.copy_from_slice(&self.regs[..29]);
        let state = arm_thread_state64_t {
            __x: x,
            __fp: self.regs[29],
            __lr: self.regs[30],
            __sp: self.sp,
            __pc: self.pc,
            __cpsr: self.pstate as u32,
            __pad: 0,
        };
        darwin_mach::thread_set_arm_state64(thread, &state)?;
        Ok(())
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

// Darwin hardware-watchpoint backend.
//
// The aarch64 WCR / WVR encoding is identical to the linux side
// (same architecture spec — DDI 0487); only the kernel-call shape
// differs. On linux it's `PTRACE_{GET,SET}REGSET(NT_ARM_HW_WATCH)`;
// on darwin it's `thread_{get,set}_state(ARM_DEBUG_STATE64)` with
// `arm_debug_state64_t { bvr, bcr, wvr, wcr, mdscr_el1 }`.
//
// `arm_debug_state64_t` exposes 16 watchpoint slots; we expose the
// first 4 to match `DebugRegisterNumber::DR0..DR3` and the cross-
// arch slot count the watchpoint registry uses today.
#[cfg(not(target_os = "linux"))]
pub mod debug_impl {
    use crate::debugger::Error;
    use crate::debugger::darwin_mach::{self, arm_debug_state64_t};
    use crate::debugger::register::debug::{
        BreakCondition, BreakSize, DebugRegisterNumber,
    };
    use nix::unistd::Pid;

    pub type DebugAddressRegister = usize;

    /// Number of watchpoint slots we expose to the registry.
    /// `arm_debug_state64_t` actually has 16; capped at 4 to mirror
    /// the linux side.
    const SLOT_COUNT: usize = 4;

    /// Encode WCR — same layout the linux/aarch64 path uses.
    fn encode_ctrl(cond: BreakCondition, bas: u8) -> u32 {
        let lsc: u32 = match cond {
            BreakCondition::DataWrites => 0b10,
            BreakCondition::DataReadsWrites => 0b11,
        };
        1 | (0b10u32 << 1) | (lsc << 3) | ((bas as u32) << 5)
    }

    fn compute_bas(addr: usize, size: BreakSize) -> (u64, u8) {
        let window = (addr as u64) & !7;
        let offset = addr & 7;
        let width = match size {
            BreakSize::Bytes1 => 1,
            BreakSize::Bytes2 => 2,
            BreakSize::Bytes4 => 4,
            BreakSize::Bytes8 => 8,
        };
        let mask: u8 = ((1u16 << width) - 1) as u8;
        (window, mask << offset)
    }

    fn decode_bas(window: u64, bas: u8) -> usize {
        if bas == 0 {
            return window as usize;
        }
        let offset = bas.trailing_zeros() as usize;
        window as usize + offset
    }

    #[derive(PartialEq, Debug, Default)]
    pub struct HardwareDebugState {
        raw: arm_debug_state64_t,
    }

    impl HardwareDebugState {
        pub fn current(pid: Pid) -> Result<Self, Error> {
            let task = darwin_mach::task_for_pid(pid)?;
            let thread = darwin_mach::first_thread_of(task)?;
            let raw = darwin_mach::thread_get_arm_debug_state64(thread)?;
            Ok(Self { raw })
        }

        /// Mirror this slot table to **every** thread of the
        /// task. Hardware watchpoint registers (WCR/WVR) are
        /// per-thread on aarch64; if we set them on the main
        /// thread only, accesses from a worker thread go
        /// undetected. Linux gets this for free because the
        /// `Tracer` already enumerates all tids and the kernel
        /// applies `PTRACE_SETREGSET(NT_ARM_HW_WATCH)` per tid;
        /// on darwin the analogous step is `task_threads()` +
        /// `thread_set_state(ARM_DEBUG_STATE64)` per thread port.
        ///
        /// We continue on per-thread errors so a transient thread
        /// (created and then exited between enumerate and write)
        /// can't sink the whole sync. The first error is returned
        /// once the loop is done.
        pub fn sync(&self, pid: Pid) -> Result<(), Error> {
            let task = darwin_mach::task_for_pid(pid)?;
            let threads = darwin_mach::task_threads_vec(task)?;
            let mut first_err: Option<Error> = None;
            for thread in threads {
                if let Err(e) = darwin_mach::thread_set_arm_debug_state64(thread, &self.raw) {
                    if first_err.is_none() {
                        first_err = Some(Error::from(e));
                    }
                }
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            Ok(())
        }

        pub fn slot_enabled(&self, slot: DebugRegisterNumber) -> bool {
            (self.raw.wcr[slot as usize] & 1) == 1
        }

        pub fn slot_addr(&self, slot: DebugRegisterNumber) -> usize {
            let i = slot as usize;
            let bas = ((self.raw.wcr[i] >> 5) & 0xff) as u8;
            decode_bas(self.raw.wvr[i], bas)
        }

        pub fn install(
            &mut self,
            slot: DebugRegisterNumber,
            addr: usize,
            cond: BreakCondition,
            size: BreakSize,
        ) {
            let (window, bas) = compute_bas(addr, size);
            let i = slot as usize;
            self.raw.wvr[i] = window;
            self.raw.wcr[i] = encode_ctrl(cond, bas) as u64;
        }

        pub fn uninstall(&mut self, slot: DebugRegisterNumber) {
            let i = slot as usize;
            self.raw.wvr[i] = 0;
            self.raw.wcr[i] = 0;
        }

        /// Identify which slot fired by matching `si_addr` against
        /// each enabled slot's BAS-selected byte set. Same shape as
        /// the linux path because the WCR/WVR semantics are the
        /// same — only the kernel call to read state differs.
        pub fn detect_and_flush_hit(
            &mut self,
            si_addr: Option<usize>,
        ) -> Option<DebugRegisterNumber> {
            let addr = si_addr?;
            for i in 0..SLOT_COUNT {
                let dr = DebugRegisterNumber::from_repr(i)?;
                if !self.slot_enabled(dr) {
                    continue;
                }
                let window = self.raw.wvr[i] as usize;
                let mut bas = ((self.raw.wcr[i] >> 5) & 0xff) as u8;
                while bas != 0 {
                    let b = bas.trailing_zeros() as usize;
                    if addr == window + b {
                        return Some(dr);
                    }
                    bas &= bas - 1;
                }
            }
            None
        }
    }
}

#[cfg(target_os = "linux")]
pub mod debug_impl {
    //! aarch64 data-watchpoint plumbing backed by `NT_ARM_HW_WATCH`
    //! (`PTRACE_GETREGSET` / `PTRACE_SETREGSET` regset 0x403).
    //!
    //! The kernel view is an array of `{ addr: u64, ctrl: u32 }` pairs
    //! — one pair per hardware slot (commonly 4). Each pair mirrors the
    //! `DBGWVR<n>_EL1` (value) and `DBGWCR<n>_EL1` (control) registers:
    //!
    //! * `addr` is the 8-byte-aligned base address of the watched window.
    //! * `ctrl` packs enable / privilege / load-store / byte-address-select
    //!   flags — we only use the minimum subset needed for unprivileged
    //!   data watchpoints; see `encode_ctrl` below.
    //!
    //! Hit attribution: the kernel delivers `SIGTRAP` with
    //! `si_code == TRAP_HWBKPT` and `si_addr` set to the exact faulting
    //! byte; we match that byte against each slot's watched range to
    //! recover the slot index.

    use crate::debugger::Error;
    use crate::debugger::error::Error::Ptrace;
    use crate::debugger::register::debug::{
        BreakCondition, BreakSize, DebugRegisterNumber,
    };
    use nix::errno::Errno;
    use nix::libc::{self, c_void, iovec};
    use nix::unistd::Pid;
    use std::mem::{MaybeUninit, size_of};

    pub type DebugAddressRegister = usize;

    /// Linux `NT_ARM_HW_WATCH` regset id.
    const NT_ARM_HW_WATCH: libc::c_uint = 0x403;
    /// Number of watchpoint slots we expose (the kernel may offer up to
    /// 16, but we cap at 4 to match `DebugRegisterNumber::DR0..DR3`
    /// which is the cross-arch shape the watchpoint registry uses).
    const SLOT_COUNT: usize = 4;

    #[repr(C)]
    #[derive(Default, Copy, Clone, PartialEq, Debug)]
    struct DbgReg {
        addr: u64,
        ctrl: u32,
        _pad: u32,
    }

    /// Mirror of `struct user_hwdebug_state` from `<asm/ptrace.h>`.
    #[repr(C)]
    #[derive(Copy, Clone, PartialEq, Debug)]
    struct UserHwDebugState {
        dbg_info: u32,
        _pad: u32,
        dbg_regs: [DbgReg; SLOT_COUNT],
    }

    impl Default for UserHwDebugState {
        fn default() -> Self {
            Self {
                dbg_info: 0,
                _pad: 0,
                dbg_regs: [DbgReg::default(); SLOT_COUNT],
            }
        }
    }

    /// Build a WCR value for a user-mode data watchpoint:
    ///   bit  0   : E  (enable)
    ///   bits 1-2 : PAC = 0b10 (unprivileged / EL0)
    ///   bits 3-4 : LSC = load (0b01) / store (0b10) / either (0b11)
    ///   bits 5-12: BAS (byte-address-select; one bit per watched byte
    ///              within the 8-byte window at `addr`)
    fn encode_ctrl(cond: BreakCondition, bas: u8) -> u32 {
        let lsc: u32 = match cond {
            BreakCondition::DataWrites => 0b10,
            BreakCondition::DataReadsWrites => 0b11,
        };
        1                       // E
            | (0b10u32 << 1)    // PAC = EL0
            | (lsc << 3)        // LSC
            | ((bas as u32) << 5)
    }

    /// Translate (address, size) into the 8-byte-aligned window the
    /// hardware watches plus the BAS mask selecting bytes in that
    /// window. Sizes 1/2/4/8 are supported and must be naturally
    /// aligned, matching the `BreakSize` enum's guarantees.
    fn compute_bas(addr: usize, size: BreakSize) -> (u64, u8) {
        let window = (addr as u64) & !7;
        let offset = addr & 7;
        let width = match size {
            BreakSize::Bytes1 => 1,
            BreakSize::Bytes2 => 2,
            BreakSize::Bytes4 => 4,
            BreakSize::Bytes8 => 8,
        };
        let mask: u8 = ((1u16 << width) - 1) as u8;
        (window, mask << offset)
    }

    /// Inverse of `compute_bas` — recover the base address and size
    /// class of a slot from its (addr, bas) pair. Used when the
    /// watchpoint registry asks which slots are in use without having
    /// kept its own record.
    fn decode_bas(window: u64, bas: u8) -> (usize, BreakSize) {
        if bas == 0 {
            return (window as usize, BreakSize::Bytes1);
        }
        let offset = bas.trailing_zeros() as usize;
        let size = match bas.count_ones() {
            1 => BreakSize::Bytes1,
            2 => BreakSize::Bytes2,
            4 => BreakSize::Bytes4,
            8 => BreakSize::Bytes8,
            // Non-power-of-two BAS patterns are not produced by
            // `compute_bas`. If we encounter one (foreign tooling set
            // it), treat it as 1-byte at the low bit so the upper
            // layers see a conservative answer.
            _ => BreakSize::Bytes1,
        };
        (window as usize + offset, size)
    }

    /// Invoke `PTRACE_{GET,SET}REGSET(pid, NT_ARM_HW_WATCH, ...)`,
    /// returning the current slot state or writing `self.raw` back.
    fn ptrace_hw_watch(
        request: libc::c_uint,
        pid: Pid,
        state: &mut UserHwDebugState,
    ) -> Result<(), Error> {
        let mut iov = iovec {
            iov_base: state as *mut _ as *mut c_void,
            iov_len: size_of::<UserHwDebugState>(),
        };
        let ret = unsafe {
            libc::ptrace(
                request,
                pid.as_raw(),
                NT_ARM_HW_WATCH as *mut c_void,
                &mut iov as *mut _ as *mut c_void,
            )
        };
        if ret < 0 {
            return Err(Ptrace(Errno::last()));
        }
        Ok(())
    }

    #[derive(PartialEq, Debug, Default)]
    pub struct HardwareDebugState {
        raw: UserHwDebugState,
    }

    impl HardwareDebugState {
        pub fn current(pid: Pid) -> Result<Self, Error> {
            let mut raw = MaybeUninit::<UserHwDebugState>::zeroed();
            // SAFETY: MaybeUninit::zeroed is a valid bit pattern for a
            // struct of POD types; we hand the storage to the kernel
            // which populates it before we `assume_init`.
            unsafe {
                ptrace_hw_watch(libc::PTRACE_GETREGSET, pid, raw.assume_init_mut())?;
                Ok(Self {
                    raw: raw.assume_init(),
                })
            }
        }

        pub fn sync(&self, pid: Pid) -> Result<(), Error> {
            let mut raw = self.raw;
            ptrace_hw_watch(libc::PTRACE_SETREGSET, pid, &mut raw)
        }

        // --- Slot-oriented API (mirrored on x86_64) ---

        pub fn slot_enabled(&self, slot: DebugRegisterNumber) -> bool {
            (self.raw.dbg_regs[slot as usize].ctrl & 1) == 1
        }

        pub fn slot_addr(&self, slot: DebugRegisterNumber) -> usize {
            let reg = &self.raw.dbg_regs[slot as usize];
            let bas = ((reg.ctrl >> 5) & 0xff) as u8;
            let (addr, _) = decode_bas(reg.addr, bas);
            addr
        }

        pub fn install(
            &mut self,
            slot: DebugRegisterNumber,
            addr: usize,
            cond: BreakCondition,
            size: BreakSize,
        ) {
            let (window, bas) = compute_bas(addr, size);
            let reg = &mut self.raw.dbg_regs[slot as usize];
            reg.addr = window;
            reg.ctrl = encode_ctrl(cond, bas);
        }

        pub fn uninstall(&mut self, slot: DebugRegisterNumber) {
            let reg = &mut self.raw.dbg_regs[slot as usize];
            reg.addr = 0;
            reg.ctrl = 0;
        }

        /// Identify which slot fired for this stop. Unlike x86 there's
        /// no per-slot "trap occurred" bit; the kernel gives us
        /// `si_addr` pointing at the triggering byte and we match it
        /// against each enabled slot's BAS-selected byte set.
        pub fn detect_and_flush_hit(
            &mut self,
            si_addr: Option<usize>,
        ) -> Option<DebugRegisterNumber> {
            let addr = si_addr?;
            for idx in 0..SLOT_COUNT {
                let dr = DebugRegisterNumber::from_repr(idx)?;
                if !self.slot_enabled(dr) {
                    continue;
                }
                let reg = &self.raw.dbg_regs[idx];
                let window = reg.addr as usize;
                let mut bas = ((reg.ctrl >> 5) & 0xff) as u8;
                while bas != 0 {
                    let b = bas.trailing_zeros() as usize;
                    if addr == window + b {
                        return Some(dr);
                    }
                    bas &= bas - 1;
                }
            }
            None
        }
    }
}

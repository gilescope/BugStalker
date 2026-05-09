// SPDX-License-Identifier: MIT
//! Debug registers and watchpoint primitives.
//!
//! The logical enums (`BreakCondition`, `BreakSize`, `DebugRegisterNumber`,
//! `DebugStatusRegister`, `DebugControlRegister`) are shared across
//! architectures. The concrete `HardwareDebugState` — which actually talks
//! to the hardware-debug registers via ptrace — lives in the arch-specific
//! `debug_impl` submodules.

use crate::debugger::error::Error;
use bit_field::BitField;
use std::fmt::{Display, Formatter};
use strum_macros::FromRepr;

#[cfg(target_arch = "x86_64")]
pub use super::x86_64::debug_impl::{DebugAddressRegister, HardwareDebugState};

#[cfg(target_arch = "aarch64")]
pub use super::aarch64::debug_impl::{DebugAddressRegister, HardwareDebugState};

/// Debug register representation.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, FromRepr)]
pub enum DebugRegisterNumber {
    DR0,
    DR1,
    DR2,
    DR3,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct DebugStatusRegister(usize);

macro_rules! impl_trap {
    ($fn_name: ident, $trap: path) => {
        #[doc = "Return true if breakpoint X condition was detected."]
        #[doc = "Reset the corresponding flag."]
        pub fn $fn_name(&mut self) -> bool {
            let is_set = (self.0 & $trap) == $trap;
            self.0 &= !$trap;
            is_set
        }
    };
}

impl DebugStatusRegister {
    /// Breakpoint condition 0 was detected.
    const TRAP0: usize = 1;
    /// Breakpoint condition 1 was detected.
    const TRAP1: usize = 1 << 1;
    /// Breakpoint condition 2 was detected.
    const TRAP2: usize = 1 << 2;
    /// Breakpoint condition 3 was detected.
    const TRAP3: usize = 1 << 3;

    pub fn new(bits: usize) -> Self {
        Self(bits)
    }

    pub fn bits(&self) -> usize {
        self.0
    }

    impl_trap!(trap0, Self::TRAP0);
    impl_trap!(trap1, Self::TRAP1);
    impl_trap!(trap2, Self::TRAP2);
    impl_trap!(trap3, Self::TRAP3);

    /// Return debug register number and flush it if breakpoint was hit.
    pub fn detect_and_flush(&mut self) -> Option<DebugRegisterNumber> {
        let dr = if self.trap0() {
            DebugRegisterNumber::DR0
        } else if self.trap1() {
            DebugRegisterNumber::DR1
        } else if self.trap2() {
            DebugRegisterNumber::DR2
        } else if self.trap3() {
            DebugRegisterNumber::DR3
        } else {
            return None;
        };
        Some(dr)
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct DebugControlRegister(usize);

impl DebugControlRegister {
    /// Enable detection of exact instruction causing a data breakpoint condition for the current task.
    const LOCAL_EXACT_BREAKPOINT_ENABLE_BIT: usize = 8;
    /// Enable detection of exact instruction causing a data breakpoint condition for all tasks.
    const GLOBAL_EXACT_BREAKPOINT_ENABLE_BIT: usize = 9;

    pub fn new(bits: usize) -> Self {
        Self(bits)
    }

    pub fn bits(&self) -> usize {
        self.0
    }

    #[inline(always)]
    pub fn dr_enabled(&self, dr: DebugRegisterNumber, global: bool) -> bool {
        let dr = dr as usize;
        let idx = if global { dr * 2 + 1 } else { dr * 2 };
        debug_assert!(idx <= 7);
        self.0.get_bit(idx)
    }

    #[inline(always)]
    pub fn configure_bp(&mut self, dr: DebugRegisterNumber, cond: BreakCondition, size: BreakSize) {
        let dr = dr as usize;
        let idx = 16 + (dr * 4);
        self.0.set_bits(idx..=idx + 1, cond as usize);
        let idx = 18 + (dr * 4);
        self.0.set_bits(idx..=idx + 1, size as usize);
    }

    #[inline(always)]
    pub fn set_dr(&mut self, dr: DebugRegisterNumber, global: bool, enable: bool) {
        let dr = dr as usize;
        let idx = if global { dr * 2 + 1 } else { dr * 2 };
        self.0.set_bit(idx, enable);

        let detection_bit = if global {
            Self::GLOBAL_EXACT_BREAKPOINT_ENABLE_BIT
        } else {
            Self::LOCAL_EXACT_BREAKPOINT_ENABLE_BIT
        };

        if enable {
            self.0.set_bit(detection_bit, true);
        } else {
            let all_disabled = [0, 1, 2, 3].iter().all(|&n| {
                !self.dr_enabled(
                    DebugRegisterNumber::from_repr(n).expect("infallible"),
                    global,
                )
            });
            if all_disabled {
                self.0.set_bit(detection_bit, false);
            }
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd)]
pub enum BreakCondition {
    DataWrites = 0b01,
    DataReadsWrites = 0b11,
}

impl Display for BreakCondition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakCondition::DataWrites => f.write_str("w"),
            BreakCondition::DataReadsWrites => f.write_str("rw"),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BreakSize {
    Bytes1 = 0b00,
    Bytes2 = 0b01,
    Bytes8 = 0b10,
    Bytes4 = 0b11,
}

impl TryFrom<u8> for BreakSize {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let size = match value {
            1 => BreakSize::Bytes1,
            2 => BreakSize::Bytes2,
            4 => BreakSize::Bytes4,
            8 => BreakSize::Bytes8,
            _ => return Err(Error::WatchpointWrongSize),
        };
        Ok(size)
    }
}

impl Display for BreakSize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakSize::Bytes1 => f.write_str("1b"),
            BreakSize::Bytes2 => f.write_str("2b"),
            BreakSize::Bytes8 => f.write_str("8b"),
            BreakSize::Bytes4 => f.write_str("4b"),
        }
    }
}

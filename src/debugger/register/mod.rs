//! Target-architecture register API.
//!
//! The hand-written register file is arch-specific; callers should keep
//! anything that names a concrete register (e.g. `Register::Rdi`) in an
//! arch-gated module. Cross-arch code should use the shared accessors
//! `RegisterMap::pc`/`set_pc`/`sp`/`set_sp`.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

pub mod debug;

#[cfg(target_arch = "x86_64")]
pub use x86_64::{DwarfRegisterMap, Register, RegisterMap};

#[cfg(target_arch = "aarch64")]
pub use aarch64::{DwarfRegisterMap, Register, RegisterMap};

// SPDX-License-Identifier: MIT
//! Off-Linux stub. `PerfMonitor` exists as a unit struct so the
//! cross-platform `pub use` re-export shape works; every method
//! returns [`PerfError::Unsupported`].

#![cfg(not(target_os = "linux"))]

use crate::PerfError;

/// Unit-struct stand-in for the real Linux `PerfMonitor`. The
/// presence of this type lets cross-platform consumers keep one
/// import path.
pub struct PerfMonitor {
    _private: (),
}

impl PerfMonitor {
    /// Returns `Err(PerfError::Unsupported)`. Stub.
    pub fn enable(&mut self) -> Result<(), PerfError> {
        Err(PerfError::Unsupported)
    }

    /// Returns `Err(PerfError::Unsupported)`. Stub.
    pub fn disable(&mut self) -> Result<(), PerfError> {
        Err(PerfError::Unsupported)
    }

    /// Returns `Err(PerfError::Unsupported)`. Stub.
    pub fn reset(&mut self) -> Result<(), PerfError> {
        Err(PerfError::Unsupported)
    }
}

/// Stub: returns `Err(PerfError::Unsupported)` on every non-Linux
/// host. The real implementation lives in `crate::linux`.
pub fn open_cycles_for_pid(_pid: i32) -> Result<PerfMonitor, PerfError> {
    Err(PerfError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_returns_unsupported() {
        let r = open_cycles_for_pid(0);
        assert!(matches!(r, Err(PerfError::Unsupported)));
    }
}

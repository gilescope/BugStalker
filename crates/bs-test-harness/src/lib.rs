// SPDX-License-Identifier: MIT
//! Stub crate. Phase 8 will host the extracted launch-debuggee →
//! set-breakpoint → render → assert flow currently inlined in
//! `tests/debugger/variables.rs`. Until then this crate exists only
//! to validate the workspace layout.

#![forbid(unsafe_code)]

/// Returns the harness crate version. Used by Phase 0 acceptance tests
/// to confirm the workspace member compiles and links.
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_zero_zero_zero() {
        assert_eq!(version(), "0.0.0");
    }
}

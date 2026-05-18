// SPDX-License-Identifier: MIT
pub mod dap;
pub mod debugger;
pub mod log;
pub mod oracle;
pub mod ui;
pub mod version;

/// Re-export the visualiser-spec data types so integration tests
/// (and downstream embedders) can name `Format`, `FieldSpec`, and
/// `TypeViewSpec` without a separate crate dependency.
pub use bs_viz_spec;

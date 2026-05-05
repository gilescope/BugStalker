// SPDX-License-Identifier: MIT
//! Trace file format (sub-phase 3A).
//!
//! A trace is a directory:
//!
//! ```text
//! trace/
//! ├── manifest.toml
//! ├── event-000001.zst
//! ├── event-000002.zst
//! ├── ...
//! └── checkpoint-000005.snap
//! ```
//!
//! See `doc/plans/phase-5-time-travel.md` § "3A. Trace format and
//! storage".

pub mod checkpoint;
pub mod manifest;
pub mod segment;
pub mod version;

pub use version::{FormatVersion, MAX_SUPPORTED_FORMAT_VERSION, TRACE_MAGIC};

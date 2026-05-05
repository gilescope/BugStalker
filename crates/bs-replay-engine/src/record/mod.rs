// SPDX-License-Identifier: MIT
//! Record path — sub-phases 3B, 3D, 3E, 3F, 3H.
//!
//! Per-platform driver lives under `record::linux::*` /
//! `record::darwin::*`. Skeletons only — see plan for the
//! sub-phase breakdown.

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod darwin;

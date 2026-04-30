// SPDX-License-Identifier: MIT
//! What crate authors depend on to ship BugStalker visualisers.
//!
//! Step 1 only re-exports Tier A — the declarative `TypeViewSpec`
//! path. Tier B (wasm visualisers) and the `CustomView` escape
//! hatch land in later phase-4 batches.
//!
//! # Quickstart
//!
//! ```ignore
//! use bs_viz_sdk::DebugView;
//!
//! #[derive(DebugView)]
//! #[bs_viz(summary = "Person({name}, age {age})")]
//! pub struct Person {
//!     pub name: String,
//!     pub age: u32,
//!     #[bs_viz(skip)]
//!     private_token: Vec<u8>,
//! }
//! ```
//!
//! Rebuild. BugStalker now finds a [`TypeViewSpec`] for `Person`
//! in the binary's `.bs_viz_spec` section and applies the summary
//! template at render time.
//!
//! # What's in scope today (step 1)
//!
//! - Type-level `#[bs_viz(summary = "tmpl")]`.
//! - Field-level `#[bs_viz(skip)]`, `#[bs_viz(rename = "...")]`,
//!   `#[bs_viz(format = "hex" | "bin" | "oct" | "iso8601" |
//!   "duration" | "utf8" | "hexdump")]`.
//! - Plain (non-generic, non-tuple) named-field structs.
//!
//! Generics, enums, tuple structs, and the `custom` escape hatch
//! are tracked under later batches and currently emit a clear
//! compile error so users hit a wall, not a silent miss.

#![deny(rustdoc::broken_intra_doc_links)]

pub use bs_viz_derive::DebugView;
pub use bs_viz_spec::{FieldSpec, Format, TypeViewSpec};

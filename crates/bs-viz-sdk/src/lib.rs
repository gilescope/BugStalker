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
//! # What's in scope today
//!
//! Type-level:
//!
//! - `#[bs_viz(summary = "tmpl")]` — single-line summary template
//!   with `{field}` placeholders.
//! - `#[bs_viz(name = "fully::qualified::Path")]` — override the
//!   recorded type name. Use this to disambiguate when two crates
//!   each derive `DebugView` on a type with the same local name
//!   and the registry's suffix-match lookup falls back to
//!   ambiguous (`None`).
//!
//! Field-level:
//!
//! - `#[bs_viz(skip)]` — hide the field at render time.
//! - `#[bs_viz(rename = "...")]` — display the field under a
//!   different name.
//! - `#[bs_viz(format = "hex" | "bin" | "oct" | "iso8601" |
//!   "duration" | "utf8" | "hexdump")]` — apply a per-field
//!   render format. `hex` / `bin` / `oct` and `iso8601` /
//!   `duration` are honoured at render time today; `utf8` /
//!   `hexdump` round-trip but still fall back to the default
//!   render until byte-array detection lands.
//!
//! Variant-level (on enum variants):
//!
//! - `#[bs_viz(summary = "...")]` — overrides the type-level
//!   summary when this variant is active.
//! - `#[bs_viz(tag = "...")]` — annotates the rendered enum with
//!   a `[tag]` chip the IDE can surface as a colour pill or
//!   status icon.
//!
//! Supported type shapes: named-field structs, tuple structs,
//! unit structs, enums (with or without variant attrs), generic
//! types (one spec per definition; the registry's lookup strips
//! generic args at query time). The `custom` escape hatch
//! (non-declarative Tier A) and Tier B (wasm) are tracked under
//! ROADMAP §4 "Remaining".

#![deny(rustdoc::broken_intra_doc_links)]

pub use bs_viz_derive::DebugView;
pub use bs_viz_spec::{FieldSpec, Format, TypeViewSpec};

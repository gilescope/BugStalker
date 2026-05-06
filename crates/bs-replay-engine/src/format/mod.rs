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
pub mod event;
pub mod event_cursor;
pub mod manifest;
pub mod segment;
pub mod trace_reader;
pub mod trace_writer;
pub mod validator;
pub mod version;

pub use checkpoint::{Checkpoint, CheckpointHeader, CheckpointIoError};
pub use event::Event;
pub use event_cursor::EventCursor;
pub use manifest::{Manifest, ManifestParseError};
pub use segment::{Segment, SegmentHeader};
pub use trace_reader::{SegmentRange, SegmentReader, TraceReadError, TraceReader};
pub use trace_writer::{DEFAULT_SEGMENT_SIZE_BYTES, TraceWriteError, TraceWriter};
pub use validator::{validate, Diag, DiagKind, Severity, ValidationReport};
pub use version::{FormatVersion, MAX_SUPPORTED_FORMAT_VERSION, TRACE_MAGIC};

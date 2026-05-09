// SPDX-License-Identifier: MIT
//! PC-to-source resolver for Phase 6.
//!
//! The perf ring yields raw program counters. The overlay should
//! defer expensive source attribution until a stop, then batch those
//! PCs through this resolver and hand `(file, line)` keys to the
//! aggregator.
//!
//! This module is intentionally independent of the live debugger
//! process model. It parses an object file's `.debug_line` tables
//! into compact rows and caches lookups by PC. Runtime users that
//! sample a PIE/shared object can set a load bias and continue to
//! call [`SourceResolver::resolve`] with runtime addresses.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gimli::{Reader, RunTimeEndian, SectionId};
use object::{Object, ObjectSection};

use crate::PerfError;
use crate::pt_decode::DecodedPtTrace;

const IS_STMT: u8 = 1 << 0;
const PROLOGUE_END: u8 = 1 << 1;
const EPILOGUE_BEGIN: u8 = 1 << 2;
const END_SEQUENCE: u8 = 1 << 3;

type EndianArcSlice = gimli::EndianArcSlice<RunTimeEndian>;

/// One source frame attributed to a sampled PC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceFrame {
    /// Source path from the line table.
    pub file: PathBuf,
    /// One-based source line. Zero is never emitted.
    pub line: u64,
    /// Source column, or zero for left edge / unavailable.
    pub column: u64,
}

/// Full source attribution for a PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPc {
    /// Object-relative PC after subtracting the resolver load bias.
    pub object_pc: u64,
    /// Primary source frame.
    pub primary: SourceFrame,
    /// Inlined caller/callee frames. Step 116 leaves this empty;
    /// the field is present so the aggregator API already matches
    /// Phase 6's "attribute inlined frames too" requirement.
    pub inlined: Vec<SourceFrame>,
    /// Whether the selected line row has DWARF `is_stmt`.
    pub is_stmt: bool,
    /// Whether the selected line row has DWARF `prologue_end`.
    pub prologue_end: bool,
    /// Whether the selected line row has DWARF `epilogue_begin`.
    pub epilogue_begin: bool,
}

/// Source attribution for a decoded Intel PT instruction window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPtTrace {
    /// Instructions that resolved to source frames.
    pub resolved: Vec<ResolvedPc>,
    /// Decoded instructions that did not resolve to source.
    pub unresolved_instructions: u64,
    /// Total decoded instructions observed before source lookup.
    pub total_instructions: u64,
    /// Synchronization points consumed by the PT decoder.
    pub decode_sync_points: usize,
    /// Decode errors skipped by resynchronizing forward.
    pub decode_skipped_errors: usize,
    /// True when the PT decoder truncated output at its configured bound.
    pub decode_truncated: bool,
}

/// Cached line-table resolver.
#[derive(Debug, Clone)]
pub struct SourceResolver {
    files: Vec<PathBuf>,
    rows: Vec<LineRow>,
    load_bias: u64,
    cache: HashMap<u64, Option<ResolvedPc>>,
}

impl SourceResolver {
    /// Build a resolver from an object file path.
    pub fn from_object_path(path: impl AsRef<Path>) -> Result<Self, PerfError> {
        let bytes = fs::read(path).map_err(PerfError::DebugInfoIo)?;
        Self::from_object_bytes(&bytes)
    }

    /// Build a resolver from object-file bytes.
    pub fn from_object_bytes(bytes: &[u8]) -> Result<Self, PerfError> {
        let file = object::File::parse(bytes)?;
        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };
        let dwarf = gimli::Dwarf::load(|id| load_section(id, &file, endian))?;
        let mut resolver = Self {
            files: Vec::new(),
            rows: Vec::new(),
            load_bias: 0,
            cache: HashMap::new(),
        };
        resolver.load_lines(&dwarf)?;
        resolver.rows.sort_unstable_by_key(|row| row.address);
        resolver.rows.shrink_to_fit();
        resolver.files.shrink_to_fit();
        Ok(resolver)
    }

    /// Set the runtime load bias for PIE/shared-object samples.
    ///
    /// `resolve(runtime_pc)` subtracts this bias before searching
    /// line rows. `resolve_object_pc` is available when callers
    /// already have object-relative addresses.
    pub fn with_load_bias(mut self, load_bias: u64) -> Self {
        self.load_bias = load_bias;
        self.cache.clear();
        self
    }

    /// Number of parsed line rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of source files referenced by the line tables.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Resolve a runtime PC to source.
    pub fn resolve(&mut self, pc: u64) -> Option<ResolvedPc> {
        let object_pc = pc.checked_sub(self.load_bias)?;
        if let Some(cached) = self.cache.get(&object_pc) {
            return cached.clone();
        }
        let resolved = self.resolve_object_pc_uncached(object_pc);
        self.cache.insert(object_pc, resolved.clone());
        resolved
    }

    /// Resolve an object-relative PC to source.
    pub fn resolve_object_pc(&mut self, object_pc: u64) -> Option<ResolvedPc> {
        if let Some(cached) = self.cache.get(&object_pc) {
            return cached.clone();
        }
        let resolved = self.resolve_object_pc_uncached(object_pc);
        self.cache.insert(object_pc, resolved.clone());
        resolved
    }

    /// Resolve a decoded Intel PT instruction stream to source frames.
    pub fn resolve_decoded_pt_trace(&mut self, trace: &DecodedPtTrace) -> ResolvedPtTrace {
        let mut resolved = Vec::new();
        let mut unresolved_instructions = 0_u64;
        for instruction in &trace.instructions {
            if let Some(source) = self.resolve(instruction.ip) {
                resolved.push(source);
            } else {
                unresolved_instructions = unresolved_instructions.saturating_add(1);
            }
        }

        ResolvedPtTrace {
            resolved,
            unresolved_instructions,
            total_instructions: trace.instructions.len() as u64,
            decode_sync_points: trace.sync_points,
            decode_skipped_errors: trace.skipped_errors.len(),
            decode_truncated: trace.truncated,
        }
    }

    fn load_lines(&mut self, dwarf: &gimli::Dwarf<EndianArcSlice>) -> Result<(), PerfError> {
        let mut units = dwarf.units();
        while let Some(header) = units.next()? {
            let unit = dwarf.unit(header)?;
            let Some(ref program) = unit.line_program else {
                continue;
            };
            let mut rows = program.clone().rows();
            let header = rows.header().clone();
            let file_base = self.files.len();
            self.files.extend(parse_files(dwarf, &unit, &header)?);

            while let Some((_, row)) = rows.next_row()? {
                let file_index = file_base + row.file_index() as usize;
                let column = match row.column() {
                    gimli::ColumnType::LeftEdge => 0,
                    gimli::ColumnType::Column(col) => col.get(),
                };
                let mut flags = 0;
                if row.is_stmt() {
                    flags |= IS_STMT;
                }
                if row.prologue_end() {
                    flags |= PROLOGUE_END;
                }
                if row.epilogue_begin() {
                    flags |= EPILOGUE_BEGIN;
                }
                if row.end_sequence() {
                    flags |= END_SEQUENCE;
                }
                self.rows.push(LineRow {
                    address: row.address(),
                    file_index,
                    line: row.line().map(std::num::NonZeroU64::get).unwrap_or(0),
                    column,
                    flags,
                });
            }
        }
        Ok(())
    }

    fn resolve_object_pc_uncached(&self, object_pc: u64) -> Option<ResolvedPc> {
        if self.rows.is_empty() {
            return None;
        }
        let pos = match self
            .rows
            .binary_search_by_key(&object_pc, |row| row.address)
        {
            Ok(mut pos) => {
                while pos > 0 && self.rows[pos - 1].address == object_pc {
                    pos -= 1;
                }
                pos
            }
            Err(0) => return None,
            Err(pos) => pos - 1,
        };

        self.rows[..=pos]
            .iter()
            .rev()
            .find_map(|row| self.row_to_resolved(object_pc, row))
    }

    fn row_to_resolved(&self, object_pc: u64, row: &LineRow) -> Option<ResolvedPc> {
        if row.line == 0 || row.has(END_SEQUENCE) {
            return None;
        }
        let file = self.files.get(row.file_index)?.clone();
        Some(ResolvedPc {
            object_pc,
            primary: SourceFrame {
                file,
                line: row.line,
                column: row.column,
            },
            inlined: Vec::new(),
            is_stmt: row.has(IS_STMT),
            prologue_end: row.has(PROLOGUE_END),
            epilogue_begin: row.has(EPILOGUE_BEGIN),
        })
    }

    #[cfg(test)]
    fn from_parts_for_test(files: Vec<PathBuf>, rows: Vec<LineRow>) -> Self {
        Self {
            files,
            rows,
            load_bias: 0,
            cache: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineRow {
    address: u64,
    file_index: usize,
    line: u64,
    column: u64,
    flags: u8,
}

impl LineRow {
    fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

fn load_section(
    id: SectionId,
    file: &object::File<'_>,
    endian: RunTimeEndian,
) -> Result<EndianArcSlice, gimli::Error> {
    let data = file
        .section_by_name(id.name())
        .and_then(|section| section.uncompressed_data().ok())
        .unwrap_or(Cow::Borrowed(&[]));
    Ok(gimli::EndianArcSlice::new(Arc::from(&*data), endian))
}

fn parse_files<R>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    header: &gimli::LineProgramHeader<R, R::Offset>,
) -> Result<Vec<PathBuf>, gimli::Error>
where
    R: Reader,
{
    let mut files = Vec::new();
    match header.file(0) {
        Some(file) => files.push(render_file_path(dwarf, unit, header, file)?),
        None => files.push(PathBuf::default()),
    }
    let mut index = 1;
    while let Some(file) = header.file(index) {
        files.push(render_file_path(dwarf, unit, header, file)?);
        index += 1;
    }
    Ok(files)
}

fn render_file_path<R>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    header: &gimli::LineProgramHeader<R, R::Offset>,
    file: &gimli::FileEntry<R, R::Offset>,
) -> Result<PathBuf, gimli::Error>
where
    R: Reader,
{
    let mut path = unit
        .comp_dir
        .as_ref()
        .map(|dir| dir.to_string_lossy())
        .transpose()?
        .map(|dir| PathBuf::from(dir.as_ref()))
        .unwrap_or_default();

    if file.directory_index() != 0
        && let Some(directory) = file.directory(header)
    {
        path.push(
            dwarf
                .attr_string(unit, directory)?
                .to_string_lossy()?
                .as_ref(),
        );
    }

    path.push(
        dwarf
            .attr_string(unit, file.path_name())?
            .to_string_lossy()?
            .as_ref(),
    );

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pt_decode::{DecodedPtInstruction, DecodedPtTrace};

    fn resolver() -> SourceResolver {
        SourceResolver::from_parts_for_test(
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")],
            vec![
                LineRow {
                    address: 0x10,
                    file_index: 0,
                    line: 10,
                    column: 1,
                    flags: IS_STMT,
                },
                LineRow {
                    address: 0x20,
                    file_index: 1,
                    line: 20,
                    column: 3,
                    flags: PROLOGUE_END,
                },
                LineRow {
                    address: 0x30,
                    file_index: 1,
                    line: 0,
                    column: 0,
                    flags: END_SEQUENCE,
                },
            ],
        )
    }

    #[test]
    fn resolves_exact_and_nearest_previous_pc() {
        let mut resolver = resolver();

        let exact = resolver.resolve_object_pc(0x20).expect("exact");
        assert_eq!(exact.primary.file, PathBuf::from("src/lib.rs"));
        assert_eq!(exact.primary.line, 20);
        assert!(exact.prologue_end);

        let nearest = resolver.resolve_object_pc(0x24).expect("nearest");
        assert_eq!(nearest.primary.file, PathBuf::from("src/lib.rs"));
        assert_eq!(nearest.primary.line, 20);
    }

    #[test]
    fn resolves_runtime_pc_with_load_bias() {
        let mut resolver = resolver().with_load_bias(0x1000);
        let loc = resolver.resolve(0x1012).expect("biased");
        assert_eq!(loc.object_pc, 0x12);
        assert_eq!(loc.primary.line, 10);
    }

    #[test]
    fn skips_zero_line_and_end_sequence_rows() {
        let mut resolver = resolver();
        let loc = resolver.resolve_object_pc(0x30).expect("previous real row");
        assert_eq!(loc.primary.line, 20);
    }

    #[test]
    fn returns_none_before_first_row() {
        let mut resolver = resolver();
        assert!(resolver.resolve_object_pc(0xf).is_none());
    }

    #[test]
    fn resolves_decoded_pt_trace_with_stats() {
        let mut resolver = resolver();
        let trace = DecodedPtTrace {
            instructions: vec![
                decoded_instruction(0x10),
                decoded_instruction(0xf),
                decoded_instruction(0x24),
            ],
            sync_points: 2,
            skipped_errors: vec!["bad packet".to_owned()],
            truncated: true,
        };

        let resolved = resolver.resolve_decoded_pt_trace(&trace);

        assert_eq!(resolved.total_instructions, 3);
        assert_eq!(resolved.resolved.len(), 2);
        assert_eq!(resolved.resolved[0].primary.line, 10);
        assert_eq!(resolved.resolved[1].primary.line, 20);
        assert_eq!(resolved.unresolved_instructions, 1);
        assert_eq!(resolved.decode_sync_points, 2);
        assert_eq!(resolved.decode_skipped_errors, 1);
        assert!(resolved.decode_truncated);
    }

    fn decoded_instruction(ip: u64) -> DecodedPtInstruction {
        DecodedPtInstruction {
            ip,
            size: 1,
            speculative: false,
            truncated: false,
        }
    }
}

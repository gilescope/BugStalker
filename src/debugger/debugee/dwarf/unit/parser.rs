// SPDX-License-Identifier: MIT
use crate::debugger::context::gcx;
use crate::debugger::debugee::dwarf::unit::die::DerefContext;
use crate::debugger::debugee::dwarf::unit::{
    BsUnit, DieRange, END_SEQUENCE, EPILOG_BEGIN, FileScopeVariable, FunctionInfo, IS_STMT,
    LineRow, PROLOG_END, UnitLazyPart, UnitProperties,
};
use crate::debugger::debugee::dwarf::utils::PathSearchIndex;
use crate::debugger::debugee::dwarf::{EndianArcSlice, NamespaceHierarchy};
use crate::debugger::error::Error;
use crate::debugger::rust::Environment;
use gimli::{
    AttributeValue, DW_AT_decl_file, DW_AT_decl_line, DW_AT_language, DW_AT_linkage_name,
    DW_AT_name, DW_AT_producer, DW_AT_specification, DebuggingInformationEntry, DwAt, Range,
    Reader, UnitHeader, UnitOffset,
};
use indexmap::IndexMap;
use log::warn;
use once_cell::sync::OnceCell;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::path::PathBuf;
use uuid::Uuid;

pub struct DwarfUnitParser<'a> {
    dwarf: &'a gimli::Dwarf<EndianArcSlice>,
}

impl<'a> DwarfUnitParser<'a> {
    pub fn new(dwarf: &'a gimli::Dwarf<EndianArcSlice>) -> Self {
        Self { dwarf }
    }

    fn attr_to_string(
        &self,
        unit: &gimli::Unit<EndianArcSlice, usize>,
        die: &DebuggingInformationEntry<EndianArcSlice, usize>,
        attr: DwAt,
    ) -> gimli::Result<Option<String>> {
        die.attr(attr)
            .and_then(|attr| self.dwarf.attr_string(unit, attr.value()).ok())
            .map(|l| l.to_string_lossy().map(|s| s.to_string()))
            .transpose()
    }

    pub fn parse(&self, header: UnitHeader<EndianArcSlice>) -> gimli::Result<BsUnit> {
        let unit = self.dwarf.unit(header.clone())?;

        let name = unit
            .name
            .as_ref()
            .and_then(|n| n.to_string_lossy().ok().map(|s| s.to_string()));

        let mut files = vec![];
        let mut lines = vec![];
        if let Some(ref lp) = unit.line_program {
            let mut rows = lp.clone().rows();
            lines = parse_lines(&mut rows)?;
            files = parse_files(self.dwarf, &unit, &rows)?;
        }
        lines.sort_unstable_by_key(|x| x.address);

        let mut ranges = self
            .dwarf
            .unit_ranges(&unit)?
            .collect::<Result<Vec<_>, _>>()?;
        ranges.sort_unstable_by_key(|r| r.begin);

        let mut cursor = unit.header.entries(&unit.abbreviations);
        cursor.next_dfs()?;
        let root = cursor.current().ok_or(gimli::Error::MissingUnitDie)?;

        let language = root.attr(DW_AT_language).and_then(|attr| {
            if let AttributeValue::Language(lang) = attr.value() {
                return Some(lang);
            }
            None
        });
        let producer = self.attr_to_string(&unit, root, DW_AT_producer)?;

        ranges.shrink_to_fit();

        Ok(BsUnit {
            idx: usize::MAX,
            properties: UnitProperties {
                encoding: unit.encoding(),
                offset: unit.debug_info_offset(),
                low_pc: unit.low_pc,
                addr_base: unit.addr_base,
                loclists_base: unit.loclists_base,
                address_size: unit.header.address_size(),
            },
            unit,
            id: Uuid::new_v4(),
            name,
            files,
            lines,
            ranges,
            lazy_part: OnceCell::new(),
            language,
            producer,
        })
    }

    pub(super) fn parse_additional(&self, bs_unit: &BsUnit) -> Result<UnitLazyPart, Error> {
        let mut fn_ranges: Vec<DieRange> = vec![];
        let mut variable_index: HashMap<
            string_interner::DefaultSymbol,
            Vec<(NamespaceHierarchy, UnitOffset)>,
        > = HashMap::new();
        let mut type_index: HashMap<string_interner::DefaultSymbol, UnitOffset> = HashMap::new();
        let mut function_index = HashMap::<UnitOffset, FunctionInfo>::new();
        let mut function_name_index = PathSearchIndex::new("::");
        let mut parent_index = IndexMap::<UnitOffset, UnitOffset>::new();
        // Track every DW_TAG_subprogram offset seen during the walk so
        // we can later classify each DW_TAG_variable as local
        // (subprogram-nested) or file-scope. Inserted before the
        // `!ranges.is_empty()` gate in the subprogram arm so even
        // range-less subprograms participate in the ancestor check.
        let mut subprogram_offsets: HashSet<UnitOffset> = HashSet::new();
        // (name_sym, namespace, offset) for every DW_TAG_variable seen
        // in unit-walk order. Filtered after the main loop using
        // `subprogram_offsets` + `parent_index` to keep only file-scope
        // entries. We can't decide at insertion time because the
        // variable's subprogram ancestor may be later in unit order
        // (children come after parents in DWARF, but we want the
        // *complete* parent chain which only stabilises post-walk).
        let mut all_variables: Vec<(
            string_interner::DefaultSymbol,
            NamespaceHierarchy,
            UnitOffset,
        )> = Vec::new();

        let mut cursor = bs_unit.unit.entries();
        let mut parent_offset = None;
        while cursor.next_entry()? {
            let Some(die) = cursor.current() else {
                if let Some(offset) = parent_offset {
                    parent_offset = parent_index.get(&offset).copied();
                }
                continue;
            };
            if let Some(offset) = parent_offset {
                parent_index.insert(die.offset(), offset);
            }

            match die.tag() {
                gimli::DW_TAG_subprogram => {
                    // Variables-view §5.4: every subprogram, with or
                    // without ranges, is a "this DIE is a function"
                    // signal for the file-scope-variable classifier.
                    subprogram_offsets.insert(die.offset());
                    let fn_info_from_die = |d: &DebuggingInformationEntry<
                        EndianArcSlice,
                        usize,
                    >|
                     -> Result<FunctionInfo, gimli::Error> {
                        let name = self.attr_to_string(bs_unit.unit(), d, DW_AT_name)?;

                        let mb_file = d
                            .attr(DW_AT_decl_file)
                            .and_then(|attr| attr.udata_value());
                        let mb_line = d
                            .attr(DW_AT_decl_line)
                            .and_then(|attr| attr.udata_value());
                        let decl_file_line =
                            mb_file.and_then(|file_idx| Some((file_idx, mb_line?)));

                        let mb_linkage_name = d
                            .attr(DW_AT_linkage_name)
                            .and_then(|attr| self.dwarf.attr_string(bs_unit.unit(), attr.value()).ok());

                        let (fn_ns, linkage_name) = match mb_linkage_name {
                            Some(linkage_name) => {
                                let linkage_name = linkage_name.to_string_lossy()?;
                                let (ns, fn_name) = NamespaceHierarchy::from_mangled(&linkage_name);
                                (ns, Some(fn_name))
                            }
                            None => (
                                NamespaceHierarchy::for_die(
                                    DerefContext::new(self.dwarf,
                                    bs_unit.unit()),
                                    die.offset(),
                                    &parent_index,
                                ),
                                None,
                            ),
                        };
                        Ok(FunctionInfo {
                            namespace: fn_ns,
                            name,
                            decl_file_line,
                            linkage_name,
                        })
                    };

                    let ranges: Box<[Range]> = self
                        .dwarf
                        .die_ranges(bs_unit.unit(), die)?
                        .collect::<Result<Vec<Range>, _>>()?
                        .into();

                    // subprograms without a range are useless for indexing
                    if !ranges.is_empty() {
                        let mut fn_info = fn_info_from_die(die)?;

                        ranges.iter().for_each(|r| {
                            fn_ranges.push(DieRange {
                                range: *r,
                                die_off: die.offset(),
                            })
                        });

                        let specification = die.attr(DW_AT_specification).and_then(|attr| {
                            if let AttributeValue::UnitRef(r) = attr.value() {
                                return Some(r);
                            }
                            warn!(target: "parser", "unexpected non-local (unit) reference to function declaration");
                            None
                        });
                        if let Some(decl_offset) = specification {
                            let decl_info = fn_info_from_die(
                                &bs_unit
                                    .unit()
                                    .entry(decl_offset)
                                    .map_err(|_| Error::InvalidSpecification(decl_offset))?,
                            )?;

                            fn_info.complete_from_decl(&decl_info);
                        }

                        function_index.insert(die.offset(), fn_info.clone());

                        let any_name = fn_info.linkage_name.as_ref().or(fn_info.name.as_ref());
                        if let Some(fn_name) = any_name {
                            function_name_index.insert_w_head(
                                fn_info.namespace.as_parts().iter(),
                                fn_name,
                                die.offset(),
                            );
                        }
                    }
                }
                gimli::DW_TAG_variable => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;

                    if let Some(ref name) = name {
                        let mb_linkage_name = die.attr(DW_AT_linkage_name).and_then(|attr| {
                            self.dwarf.attr_string(bs_unit.unit(), attr.value()).ok()
                        });

                        let variable_ns = match mb_linkage_name {
                            Some(linkage_name) => {
                                let linkage_name = linkage_name.to_string_lossy()?;
                                let (ns, _) = NamespaceHierarchy::from_mangled(&linkage_name);
                                ns
                            }
                            None => NamespaceHierarchy::for_die(
                                DerefContext::new(self.dwarf, bs_unit.unit()),
                                die.offset(),
                                &parent_index,
                            ),
                        };

                        let name_sym = gcx().with_interner(|i| i.get_or_intern(name));
                        variable_index
                            .entry(name_sym)
                            .or_default()
                            .push((variable_ns.clone(), die.offset()));
                        // Stash the same triple for post-walk
                        // file-scope classification (variables-view
                        // §5.4). The clones are cheap — NamespaceHierarchy
                        // is a `Vec<DefaultSymbol>` of interned ids.
                        all_variables.push((name_sym, variable_ns, die.offset()));
                    }
                }
                gimli::DW_TAG_base_type => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;
                    if let Some(ref name) = name {
                        let sym = gcx().with_interner(|i| i.get_or_intern(name));
                        type_index.insert(sym, die.offset());
                    }
                }
                gimli::DW_TAG_structure_type => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;
                    if let Some(ref name) = name {
                        let sym = gcx().with_interner(|i| i.get_or_intern(name));
                        type_index.insert(sym, die.offset());
                    }
                }
                gimli::DW_TAG_union_type => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;
                    if let Some(ref name) = name {
                        let sym: string_interner::symbol::SymbolU32 =
                            gcx().with_interner(|i| i.get_or_intern(name));
                        type_index.insert(sym, die.offset());
                    }
                }
                gimli::DW_TAG_array_type => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;
                    if let Some(ref name) = name {
                        let sym = gcx().with_interner(|i| i.get_or_intern(name));
                        type_index.insert(sym, die.offset());
                    }
                }
                gimli::DW_TAG_pointer_type => {
                    let name = self.attr_to_string(bs_unit.unit(), die, DW_AT_name)?;
                    if let Some(ref name) = name {
                        let sym = gcx().with_interner(|i| i.get_or_intern(name));
                        type_index.insert(sym, die.offset());
                    }
                }
                _ => {}
            };

            if die.has_children() {
                parent_offset = Some(die.offset());
            }
        }
        fn_ranges.sort_unstable_by_key(|dr| dr.range.begin);

        // Variables-view §5.4: classify each DW_TAG_variable as
        // file-scope (any-`static`, any-`thread_local!`) vs local.
        //
        // Two paths:
        //   * TLS internals (DW_AT_name in {__KEY, VAL,
        //     __RUST_STD_INTERNAL_VAL}) are ALWAYS file-scope. rustc
        //     lowers `thread_local!{}` into a chain of nested
        //     closures + const-eval scopes; the literal DIE is
        //     subprogram-nested but the *meaning* is "TLS belonging
        //     to the surrounding namespace". The user expects to
        //     see these in the Thread-locals scope, so we don't
        //     gate on the parent chain.
        //   * Everything else uses the parent_index ancestor chain
        //     check: file-scope iff no DW_TAG_subprogram ancestor
        //     before exhausting. (Function-local statics like
        //     `static mut INNER_STATIC` inside `fn` are therefore
        //     correctly excluded — they're locals, not statics.)
        // `get_or_intern` (not `get`) because the first unit parsed
        // may not have encountered any of these names yet — `get`
        // would return None and the TLS path would silently miss
        // everything in the first unit until something else seeded
        // the interner. Forcing the intern is idempotent and cheap.
        let tls_key_sym = gcx().with_interner(|i| i.get_or_intern("__KEY"));
        let tls_val_sym = gcx().with_interner(|i| i.get_or_intern("VAL"));
        let tls_rsi_sym = gcx().with_interner(|i| i.get_or_intern("__RUST_STD_INTERNAL_VAL"));
        let is_tls_internal = |sym: string_interner::DefaultSymbol| {
            sym == tls_key_sym || sym == tls_val_sym || sym == tls_rsi_sym
        };
        let mut file_scope_variables: Vec<FileScopeVariable> =
            Vec::with_capacity(all_variables.len() / 4);
        for (name_sym, namespace, offset) in all_variables {
            if is_tls_internal(name_sym) {
                file_scope_variables.push(FileScopeVariable {
                    name_sym,
                    namespace,
                    offset,
                });
                continue;
            }
            let mut cursor = parent_index.get(&offset).copied();
            let mut is_local = false;
            while let Some(parent_off) = cursor {
                if subprogram_offsets.contains(&parent_off) {
                    is_local = true;
                    break;
                }
                cursor = parent_index.get(&parent_off).copied();
            }
            if !is_local {
                file_scope_variables.push(FileScopeVariable {
                    name_sym,
                    namespace,
                    offset,
                });
            }
        }

        fn_ranges.shrink_to_fit();
        variable_index.shrink_to_fit();
        type_index.shrink_to_fit();
        function_index.shrink_to_fit();
        function_name_index.shrink_to_fit();
        file_scope_variables.shrink_to_fit();

        Ok(UnitLazyPart {
            fn_ranges,
            variable_index,
            type_index,
            function_index,
            function_name_index,
            parent_index,
            file_scope_variables,
        })
    }
}

#[inline(always)]
fn parse_lines<R, Offset>(
    rows: &mut gimli::LineRows<R, gimli::IncompleteLineProgram<R, Offset>, Offset>,
) -> gimli::Result<Vec<LineRow>>
where
    R: Reader<Offset = Offset>,
    Offset: gimli::ReaderOffset,
{
    let mut lines = vec![];
    while let Some((_, line_row)) = rows.next_row()? {
        let column = match line_row.column() {
            gimli::ColumnType::LeftEdge => 0,
            gimli::ColumnType::Column(x) => x.get(),
        };

        let mut flags = 0_u8;
        if line_row.is_stmt() {
            flags |= IS_STMT;
        }
        if line_row.prologue_end() {
            flags |= PROLOG_END;
        }
        if line_row.epilogue_begin() {
            flags |= EPILOG_BEGIN;
        }
        if line_row.end_sequence() {
            flags |= END_SEQUENCE;
        }

        lines.push(LineRow {
            address: line_row.address(),
            file_index: line_row.file_index(),
            line: line_row.line().map(NonZeroU64::get).unwrap_or(0),
            column,
            flags,
        })
    }

    lines.shrink_to_fit();
    Ok(lines)
}

#[inline(always)]
fn parse_files<R, Offset>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    rows: &gimli::LineRows<R, gimli::IncompleteLineProgram<R, Offset>, Offset>,
) -> gimli::Result<Vec<PathBuf>>
where
    R: Reader<Offset = Offset>,
    Offset: gimli::ReaderOffset,
{
    let mut files = vec![];
    let header = rows.header();
    match header.file(0) {
        Some(file) => files.push(render_file_path(unit, file, header, dwarf)?),
        None => files.push(PathBuf::default()),
    }
    let mut index = 1;
    while let Some(file) = header.file(index) {
        files.push(render_file_path(unit, file, header, dwarf)?);
        index += 1;
    }

    files.shrink_to_fit();
    Ok(files)
}

#[inline(always)]
fn render_file_path<R: Reader>(
    dw_unit: &gimli::Unit<R>,
    file: &gimli::FileEntry<R, R::Offset>,
    header: &gimli::LineProgramHeader<R, R::Offset>,
    sections: &gimli::Dwarf<R>,
) -> Result<PathBuf, gimli::Error> {
    let mut path = if let Some(ref comp_dir) = dw_unit.comp_dir {
        PathBuf::from(comp_dir.to_string_lossy()?.as_ref())
    } else {
        PathBuf::new()
    };

    if file.directory_index() != 0
        && let Some(directory) = file.directory(header)
    {
        path.push(
            sections
                .attr_string(dw_unit, directory)?
                .to_string_lossy()?
                .as_ref(),
        );
    }

    if path.starts_with("/rustc/") {
        let rust_env = Environment::current();
        if let Some(ref std_lib_path) = rust_env.std_lib_path {
            let mut new_path = std_lib_path.clone();
            path.iter().skip(3).for_each(|part| {
                new_path.push(part);
            });
            path = new_path;
        }
    }

    path.push(
        sections
            .attr_string(dw_unit, file.path_name())?
            .to_string_lossy()?
            .as_ref(),
    );

    Ok(path)
}

// SPDX-License-Identifier: MIT
use crate::debugger::context::gcx;
use crate::debugger::debugee::dwarf::DebugInformation;
use crate::debugger::debugee::dwarf::eval::{EvaluationContext, ExpressionEvaluator};
use crate::debugger::debugee::dwarf::r#type::ComplexType;
use crate::debugger::debugee::dwarf::unit::BsUnit;
use crate::debugger::debugee::dwarf::unit::die_ref::{Argument, FatDieRef, Typed, Variable};
use crate::debugger::error::Error;
use crate::debugger::error::Error::FunctionNotFound;
use crate::debugger::variable::dqe::{DataCast, Dqe, PointerCast, Selector};
use crate::debugger::variable::storage::StorageClass;
use crate::debugger::variable::value::Value;
use crate::debugger::variable::value::parser::{ParseContext, ValueModifiers, ValueParser};
use crate::debugger::variable::r#virtual::VirtualVariableDie;
use crate::debugger::variable::{Identity, ObjectBinaryRepr};
use crate::debugger::{Debugger, read_memory_by_pid};
use crate::{ref_resolve_unit_call, resolve_unit_call, weak_error};
use bytes::Bytes;
use gimli::Range;
use std::fmt::Debug;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryResultKind {
    /// Result value is an argument or variable
    Root,
    /// Result value calculated using DQE
    Expression,
}

/// Variables-view §5.6 — shallow payload / padding breakdown of
/// a struct type. `total = payload + padding`; the vscode-
/// extension uses the proportion to paint an HSL lightness split
/// on the row background (payload at base lightness, padding at
/// `base ± Δ`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutBreakdown {
    /// `DW_AT_byte_size` of the struct.
    pub total: u64,
    /// Sum of member sizes — the bytes doing real work.
    pub payload: u64,
    /// `total - payload` — interior + trailing alignment slack.
    pub padding: u64,
}

impl LayoutBreakdown {
    /// Padding as a percentage of total (0–100). Returns `None`
    /// when `total` is zero (a zero-sized type can't have
    /// meaningful padding).
    pub fn padding_pct(&self) -> Option<u8> {
        if self.total == 0 {
            return None;
        }
        Some(
            (self.padding.saturating_mul(100) / self.total)
                .min(100) as u8,
        )
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn padding_pct_zero_total_is_none() {
        let l = LayoutBreakdown {
            total: 0,
            payload: 0,
            padding: 0,
        };
        assert_eq!(l.padding_pct(), None);
    }

    #[test]
    fn padding_pct_basic_arithmetic() {
        // 24-byte struct with 14 bytes of padding (58%).
        let l = LayoutBreakdown {
            total: 24,
            payload: 10,
            padding: 14,
        };
        assert_eq!(l.padding_pct(), Some(58));
    }

    #[test]
    fn padding_pct_caps_at_100() {
        // Pathological: padding exceeds total (shouldn't happen
        // in practice but defensive saturation).
        let l = LayoutBreakdown {
            total: 8,
            payload: 0,
            padding: 16,
        };
        assert_eq!(l.padding_pct(), Some(100));
    }

    #[test]
    fn padding_pct_zero_padding() {
        let l = LayoutBreakdown {
            total: 16,
            payload: 16,
            padding: 0,
        };
        assert_eq!(l.padding_pct(), Some(0));
    }
}

/// Result of DQE evaluation.
#[derive(Clone)]
pub struct QueryResult<'a> {
    // TODO tmp pub
    pub value: Option<Value>,
    scope: Option<Box<[Range]>>,
    kind: QueryResultKind,
    base_type: Rc<ComplexType>,
    identity: Identity,
    evcx_builder: EvaluationContextBuilder<'a>,
    /// Variables-view §5.3 storage class. Computed at construction
    /// time from the variable's DW_AT_location expression + the
    /// segment-kind index. `None` for results derived via DQE
    /// (DataCast / PointerCast) where there's no source DIE.
    storage: Option<StorageClass>,
}

impl QueryResult<'_> {
    /// Return CU in which result values are located.
    pub fn unit(&self) -> &BsUnit {
        self.evcx_builder.unit()
    }

    /// Return underlying typed value representation.
    #[inline(always)]
    pub fn value(&self) -> &Value {
        self.value.as_ref().expect("should be `Some`")
    }

    /// Return underlying typed value representation.
    #[inline(always)]
    pub fn into_value(mut self) -> Value {
        self.value.take().expect("should be `Some`")
    }

    /// Return underlying value and result identity (variable or argument identity).
    #[inline(always)]
    pub fn into_identified_value(mut self) -> (Identity, Value) {
        (self.identity, self.value.take().expect("should be `Some`"))
    }

    /// Return result kind:
    /// - `Root` kind means that value is an argument or variable
    /// - `Expression` kind means that value calculated using DQE
    #[inline(always)]
    pub fn kind(&self) -> QueryResultKind {
        self.kind
    }

    /// Return type graph using for parse a result.
    #[inline(always)]
    pub fn type_graph(&self) -> &ComplexType {
        self.base_type.as_ref()
    }

    /// Return result identity.
    #[inline(always)]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Return variable or argument scope. Scope is a PC ranges where value is valid,
    /// `None` for global or virtual variables.
    #[inline(always)]
    pub fn scope(&self) -> &Option<Box<[Range]>> {
        &self.scope
    }

    /// Variables-view §5.3 storage class. `None` for synthetic
    /// QueryResults produced by DQE casts (no source DIE to walk).
    #[inline(always)]
    pub fn storage(&self) -> Option<StorageClass> {
        self.storage
    }

    /// Variables-view §5.5 — total byte size of this value's type,
    /// resolved from `DW_AT_byte_size` via the existing
    /// `ComplexType::type_size_in_bytes` path. Returns `None` for
    /// types whose size depends on dynamic runtime data the
    /// evaluator can't determine (extremely rare for normal Rust
    /// types — slices and dyn-trait fat pointers have a known
    /// header size, the dynamic payload lives behind a pointer).
    pub fn byte_size(&self) -> Option<u64> {
        let graph = self.type_graph();
        self.with_evcx(|evcx| graph.type_size_in_bytes(evcx, graph.root()))
    }

    /// Variables-view §5.6 — shallow payload-vs-padding breakdown
    /// of this value's type. `payload` is the sum of the member
    /// types' sizes; `padding = total - payload`. Computed only
    /// for `Structure` types (where `Σ(members) < total` indicates
    /// interior padding for alignment); `None` for primitives,
    /// arrays, slices, pointers, and any type the evaluator can't
    /// fully size. The vscode-extension uses this to paint the
    /// HSL lightness split on the row background.
    ///
    /// Shallow only — nested structs' own padding is not summed.
    /// Deep waste is harder to act on; the design doc defers it
    /// to a follow-up.
    pub fn layout(&self) -> Option<LayoutBreakdown> {
        use crate::debugger::debugee::dwarf::r#type::TypeDeclaration;
        let graph = self.type_graph();
        let root = graph.root();
        let decl = graph.types.get(&root)?;
        let members: &[_] = match decl {
            TypeDeclaration::Structure { members, .. } => members.as_slice(),
            _ => return None,
        };
        self.with_evcx(|evcx| {
            let total = graph.type_size_in_bytes(evcx, root)?;
            let mut payload: u64 = 0;
            for m in members {
                let t = m.type_ref?;
                payload = payload.saturating_add(graph.type_size_in_bytes(evcx, t)?);
            }
            let padding = total.saturating_sub(payload);
            Some(LayoutBreakdown {
                total,
                payload,
                padding,
            })
        })
    }

    /// Evaluate any function with evaluation context.
    pub fn with_evcx<T, F: FnOnce(&EvaluationContext) -> T>(&self, cb: F) -> T {
        self.evcx_builder.with_evcx(cb)
    }

    /// Modify the underlying value and return a new result extended from the current one.
    pub fn modify_value<F: FnOnce(&ParseContext, Value) -> Option<Value>>(
        mut self,
        cb: F,
    ) -> Option<Self> {
        let value = self.value.take().expect("should be `Some`");
        let type_graph = self.type_graph();
        let eval_cb = |evcx: &EvaluationContext| {
            let pcx = &ParseContext {
                evcx,
                type_graph,
                visited_allocations: Default::default(),
                recursion_depth: Default::default(),
            };
            cb(pcx, value)
        };
        let new_value = self.evcx_builder.with_evcx(eval_cb)?;
        self.value = Some(new_value);
        Some(self)
    }
}

impl PartialEq for QueryResult<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value && self.identity == other.identity
    }
}

#[derive(Clone)]
enum EvaluationContextBuilder<'a> {
    Ready(&'a Debugger, ExpressionEvaluator<'a>),
    Virtual {
        debugger: &'a Debugger,
        debug_info: &'a DebugInformation,
        unit: &'a BsUnit,
    },
}

impl EvaluationContextBuilder<'_> {
    pub fn unit(&self) -> &BsUnit {
        match self {
            EvaluationContextBuilder::Ready(_, evaluator) => evaluator.unit(),
            EvaluationContextBuilder::Virtual { unit, .. } => unit,
        }
    }

    fn with_evcx<T, F: FnOnce(&EvaluationContext) -> T>(&self, cb: F) -> T {
        let evaluator;
        let evcx = match self {
            EvaluationContextBuilder::Ready(debugger, evaluator) => EvaluationContext {
                evaluator,
                ecx: debugger.ecx(),
            },
            EvaluationContextBuilder::Virtual {
                debugger,
                debug_info,
                unit,
                ..
            } => {
                let dwarf = debug_info.dwarf();
                evaluator = resolve_unit_call!(
                    dwarf,
                    unit,
                    evaluator,
                    &debugger.debugee,
                    debug_info.dwarf()
                );
                EvaluationContext {
                    evaluator: &evaluator,
                    ecx: debugger.ecx(),
                }
            }
        };
        cb(&evcx)
    }
}

#[macro_export]
macro_rules! type_from_cache {
    ($variable: expr, $cache: expr) => {
        $variable
            .deref_ensure()
            .type_ref()
            .and_then(
                |type_ref| match $cache.entry(($variable.unit().id, type_ref)) {
                    std::collections::hash_map::Entry::Occupied(o) => {
                        Some(std::rc::Rc::clone(o.get()))
                    }
                    std::collections::hash_map::Entry::Vacant(v) => $variable.r#type().map(|t| {
                        let t = std::rc::Rc::new(t);
                        v.insert(t.clone());
                        t
                    }),
                },
            )
            .ok_or_else(|| {
                $crate::debugger::variable::value::ParsingError::Assume(
                    $crate::debugger::variable::value::AssumeError::NoType("variable"),
                )
            })
    };
}

/// Evaluate DQE at current location.
pub struct DqeExecutor<'a> {
    debugger: &'a Debugger,
}

impl<'dbg> DqeExecutor<'dbg> {
    pub fn new(debugger: &'dbg Debugger) -> Self {
        Self { debugger }
    }

    fn variable_die_by_selector(
        &self,
        selector: &Selector,
    ) -> Result<Vec<FatDieRef<'dbg, Variable>>, Error> {
        let ecx = self.debugger.ecx();

        let debugee = &self.debugger.debugee;
        // For a global lookup by name (`local_only = false`) we
        // must NOT require the focus PC to live in a registered
        // dylib: tokio worker threads are routinely parked deep in
        // libsystem on darwin (no DWARF, no entry in our
        // registry), and the global TLS variable being asked for
        // (e.g. tokio's `CONTEXT`) lives in the main executable
        // regardless. Only a `local_only = true` selector or
        // `Selector::Any` actually need the current function.
        let pc_debug_info_and_func = debugee.debug_info(ecx.location().pc).and_then(|di| {
            let func = di
                .find_function_by_pc(ecx.location().global_pc)?
                .ok_or(FunctionNotFound(ecx.location().global_pc))?;
            Ok((di, func))
        });

        let vars = match selector {
            Selector::Name {
                var_name,
                local_only: local,
            } => {
                let local_variants = pc_debug_info_and_func
                    .as_ref()
                    .ok()
                    .and_then(|(_, (current_func, _))| {
                        current_func.local_variable(ecx.location().global_pc, var_name)
                    })
                    .map(|v| vec![v])
                    .unwrap_or_default();

                let local = *local;

                // local variables is in priority anyway, if there are no local variables and
                // selector allow non-locals then try to search in a whole object.
                if !local && local_variants.is_empty() {
                    // Walk every loaded debug_info — the variable
                    // may live in a dylib other than the one the
                    // focus PC is in, and on darwin the focus may
                    // not have a tracked debug_info at all.
                    let mut found = vec![];
                    for di in debugee.debug_info_all() {
                        if let Ok(vars) = di.find_variables(ecx.location(), var_name) {
                            found.extend(vars);
                        }
                    }
                    found
                } else {
                    local_variants
                }
            }
            Selector::Any => {
                let (_, (current_func, _)) = pc_debug_info_and_func?;
                current_func.local_variables(ecx.location().global_pc)
            }
        };

        Ok(vars)
    }

    fn param_die_by_selector(
        &self,
        selector: &Selector,
    ) -> Result<Vec<FatDieRef<'dbg, Argument>>, Error> {
        let ecx_loc = self.debugger.ecx().location();
        let debugee = &self.debugger.debugee;
        let (current_function, _) = debugee
            .debug_info(ecx_loc.pc)?
            .find_function_by_pc(ecx_loc.global_pc)?
            .ok_or(FunctionNotFound(ecx_loc.global_pc))?;
        let params = current_function.parameters();
        let params = match selector {
            Selector::Name { var_name, .. } => params
                .into_iter()
                .filter(|r| r.deref_ensure().name().as_ref() == Some(var_name))
                .collect::<Vec<_>>(),
            Selector::Any => params,
        };
        Ok(params)
    }

    /// Build a [`QueryResult`] for the value referred to by `die_ref`.
    /// Shared by [`Self::apply_select_die`] and the file-scope
    /// enumeration path (variables-view §5.4). Returns `None` if any
    /// of type resolution, value reading, or parsing fail — same
    /// best-effort semantics as the variable selector path.
    fn root_from_die<H: Typed>(
        &self,
        die_ref: &FatDieRef<'dbg, H>,
        ranges: Option<Box<[Range]>>,
    ) -> Option<QueryResult<'dbg>> {
        // Storage classification (variables-view §5.3) happens at
        // the call site, after this method returns — the caller
        // knows whether it's looking at a Variable or Argument
        // and we don't want to specialise on H here. Default `None`
        // is then overwritten by `qr.storage = …`.
        let storage = None;
        let debugger = self.debugger;
        let r#type = gcx().with_type_cache(|tc| weak_error!(type_from_cache!(die_ref, tc)))?;

        let evaluator = ref_resolve_unit_call!(
            die_ref,
            evaluator,
            &debugger.debugee,
            die_ref.debug_info.dwarf()
        );
        let context_builder = EvaluationContextBuilder::Ready(debugger, evaluator);

        let value = context_builder.with_evcx(|evcx| {
            let data = die_ref.read_value(debugger.ecx(), &debugger.debugee, &r#type);

            let parser = ValueParser::new();
            let pcx = &ParseContext {
                evcx,
                type_graph: &r#type,
                visited_allocations: Default::default(),
                recursion_depth: Default::default(),
            };
            let modifiers = &ValueModifiers::from_identity(pcx, Identity::from_die(die_ref));
            parser.parse(pcx, data, modifiers)
        })?;

        Some(QueryResult {
            value: Some(value),
            scope: ranges,
            kind: QueryResultKind::Root,
            base_type: r#type,
            identity: Identity::from_die(die_ref),
            evcx_builder: context_builder,
            storage,
        })
    }

    /// Select variables or arguments from debugee state.
    fn apply_select_die(
        &self,
        selector: &Selector,
        on_args: bool,
    ) -> Result<Vec<QueryResult<'dbg>>, Error> {
        match on_args {
            true => {
                let params = self.param_die_by_selector(selector)?;
                Ok(params
                    .iter()
                    .filter_map(|arg_die| {
                        self.root_from_die(
                            arg_die,
                            arg_die.max_range().map(|r| {
                                let scope: Box<[Range]> = Box::new([r]);
                                scope
                            }),
                        )
                    })
                    .collect())
            }
            false => {
                let vars = self.variable_die_by_selector(selector)?;
                Ok(vars
                    .iter()
                    .filter_map(|var_die| {
                        let mut qr = self.root_from_die(var_die, var_die.ranges())?;
                        // Variables-view §5.3: now that root_from_die
                        // has populated `qr.value` (and therefore
                        // `value.in_memory_location()`), compute the
                        // storage class by walking the DW_AT_location
                        // expression + the segment-kind index.
                        let addr = qr.value().in_memory_location();
                        qr.storage =
                            compute_storage_for_variable(var_die, addr, self.debugger);
                        Some(qr)
                    })
                    .collect())
            }
        }
    }

    /// Create virtual DIE from an existing type,
    /// then return a query result with a value from this DIE and address in debugee memory.
    fn apply_ptr_cast_op(&self, ptr_cast: &PointerCast) -> Result<QueryResult<'dbg>, Error> {
        let mut var_die = VirtualVariableDie::workpiece();
        let var_die_ref = var_die.init_with_type(&self.debugger.debugee, &ptr_cast.ty)?;

        let r#type = gcx().with_type_cache(|tc| type_from_cache!(var_die_ref, tc))?;

        let context_builder = EvaluationContextBuilder::Virtual {
            debugger: self.debugger,
            debug_info: var_die_ref.debug_info,
            unit: var_die_ref.unit(),
        };

        let value = context_builder.with_evcx(|evcx| {
            let data = ObjectBinaryRepr {
                raw_data: Bytes::copy_from_slice(&ptr_cast.ptr.to_le_bytes()),
                address: None,
                size: std::mem::size_of::<usize>(),
            };

            let parser = ValueParser::new();
            let pcx = &ParseContext {
                evcx,
                type_graph: &r#type,
                visited_allocations: Default::default(),
                recursion_depth: Default::default(),
            };
            parser.parse(pcx, Some(data), &ValueModifiers::default())
        });

        Ok(QueryResult {
            value,
            scope: None,
            kind: QueryResultKind::Expression,
            base_type: r#type,
            identity: Identity::default(),
            evcx_builder: context_builder,
            // Synthetic QueryResult — no source DIE to walk for
            // storage classification (variables-view §5.3).
            storage: None,
        })
    }

    /// Create virtual DIE from an existing type,
    /// then return a query result with a value from this DIE and address in debugee memory.
    fn apply_data_cast(&self, data_cast: &DataCast) -> Result<QueryResult<'dbg>, Error> {
        let mut var_die = VirtualVariableDie::workpiece();
        let debug_info = self
            .debugger
            .debugee
            .debug_info_from_file(&data_cast.ty_debug_info)?;
        let var_die_ref = var_die.init_with_known_type(
            debug_info,
            data_cast.ty_unit_off,
            data_cast.ty_die_off,
        )?;

        let r#type = gcx().with_type_cache(|tc| type_from_cache!(var_die_ref, tc))?;

        let context_builder = EvaluationContextBuilder::Virtual {
            debugger: self.debugger,
            debug_info: var_die_ref.debug_info,
            unit: var_die_ref.unit(),
        };

        let value = context_builder.with_evcx(|evcx| {
            let size = r#type.type_size_in_bytes(evcx, r#type.root())? as usize;

            let raw_data = weak_error!(read_memory_by_pid(
                evcx.ecx.pid_on_focus(),
                data_cast.ptr,
                size
            ))?;

            let data = ObjectBinaryRepr {
                raw_data: Bytes::copy_from_slice(&raw_data),
                address: Some(data_cast.ptr),
                size,
            };

            let parser = ValueParser::new();
            let pcx = &ParseContext {
                evcx,
                type_graph: &r#type,
                visited_allocations: Default::default(),
                recursion_depth: Default::default(),
            };
            parser.parse(pcx, Some(data), &ValueModifiers::default())
        });

        Ok(QueryResult {
            value,
            scope: None,
            kind: QueryResultKind::Expression,
            base_type: r#type,
            identity: Identity::default(),
            evcx_builder: context_builder,
            // Synthetic QueryResult — no source DIE to walk for
            // storage classification (variables-view §5.3).
            storage: None,
        })
    }

    fn apply_dqe(&self, dqe: &Dqe, on_args: bool) -> Result<Vec<QueryResult<'dbg>>, Error> {
        match dqe {
            Dqe::Variable(selector) => self.apply_select_die(selector, on_args),
            Dqe::PtrCast(ptr_cast) => self.apply_ptr_cast_op(ptr_cast).map(|q| vec![q]),
            Dqe::DataCast(data_cast) => self.apply_data_cast(data_cast).map(|q| vec![q]),
            Dqe::Field(next, field) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|_, val| val.field(field)))
                    .collect())
            }
            Dqe::Index(next, idx) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|_, val| val.index(idx)))
                    .collect())
            }
            Dqe::Slice(next, left, right) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|pcx, val| val.slice(pcx, *left, *right)))
                    .collect())
            }
            Dqe::Deref(next) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|pcx, val| val.deref(pcx)))
                    .collect())
            }
            Dqe::Address(next) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|pcx, val| val.address(pcx)))
                    .collect())
            }
            Dqe::Canonic(next) => {
                let results = self.apply_dqe(next, on_args)?;
                Ok(results
                    .into_iter()
                    .filter_map(|q| q.modify_value(|_, val| Some(val.canonic())))
                    .collect())
            }
        }
    }

    /// Query variables and returns matched list.
    pub fn query(&self, dqe: &Dqe) -> Result<Vec<QueryResult<'dbg>>, Error> {
        self.apply_dqe(dqe, false)
    }

    /// Query only variable names.
    /// Only filter expression supported.
    ///
    /// # Panics
    ///
    /// This method will panic if select expression
    /// contains any operators excluding a variable selector.
    pub fn query_names(&self, dqe: &Dqe) -> Result<Vec<String>, Error> {
        match dqe {
            Dqe::Variable(selector) => {
                let vars = self.variable_die_by_selector(selector)?;
                Ok(vars
                    .into_iter()
                    .filter_map(|die_ref| die_ref.deref_ensure().name())
                    .collect())
            }
            _ => unreachable!("unexpected expression variant"),
        }
    }

    /// Same as [`DqeExecutor::query`] but for function arguments.
    pub fn query_arguments(&self, dqe: &Dqe) -> Result<Vec<QueryResult<'dbg>>, Error> {
        self.apply_dqe(dqe, true)
    }

    /// Same as [`DqeExecutor::query_names`] but for function arguments.
    pub fn query_arguments_names(&self, dqe: &Dqe) -> Result<Vec<String>, Error> {
        match dqe {
            Dqe::Variable(selector) => {
                let params = self.param_die_by_selector(selector)?;
                Ok(params
                    .into_iter()
                    .filter_map(|r| r.deref_ensure().name())
                    .collect())
            }
            _ => unreachable!("unexpected expression variant"),
        }
    }

    /// Enumerate every file-scope `DW_TAG_variable` in the debugee
    /// (statics + thread-locals), filtered by `kind` and `filter`.
    /// Backs the variables-pane `Statics` / `Thread-locals` scopes
    /// (variables-view §5.4).
    ///
    /// `kind` selects statics vs thread-locals. TLS classification
    /// is by the rustc `thread_local!` lowering: a `DW_TAG_variable`
    /// named `__KEY`, `VAL`, or `__RUST_STD_INTERNAL_VAL` is a TLS
    /// internal; everything else is a static.
    ///
    /// `filter` controls breadth — see [`FileScopeFilter`].
    pub fn query_file_scope(
        &self,
        kind: FileScopeKind,
        filter: FileScopeFilter,
    ) -> Result<Vec<QueryResult<'dbg>>, Error> {
        let tls_names = TlsInternalNames::resolve();
        let current_crate = match filter {
            FileScopeFilter::CurrentCrate => self.current_crate_namespace_root(),
            _ => None,
        };
        let current_unit_id = match filter {
            FileScopeFilter::CurrentUnit => self.current_unit_id(),
            _ => None,
        };

        let mut out = Vec::new();
        for debug_info in self.debugger.debugee.debug_info_all() {
            let Ok(entries) = debug_info.enumerate_file_scope_variables() else {
                continue;
            };
            for (meta, die_ref) in entries {
                let is_tls = tls_names.is_tls_internal(meta.name_sym);
                match kind {
                    FileScopeKind::Statics if is_tls => continue,
                    FileScopeKind::ThreadLocals if !is_tls => continue,
                    _ => {}
                }
                if let Some(crate_root) = current_crate.as_ref()
                    && meta.namespace.as_parts().first() != Some(crate_root)
                {
                    continue;
                }
                if let Some(uid) = current_unit_id
                    && die_ref.unit().id != uid
                {
                    continue;
                }
                // `root_from_die` may return None for TLS internals
                // whose runtime slot hasn't been initialised on the
                // current thread (variables-view §5.4 known limit) —
                // silently drop those entries for v0. A future
                // refactor could surface them with an "<unavailable>"
                // placeholder so the user still sees the name.
                if let Some(mut qr) = self.root_from_die(&die_ref, None) {
                    // Variables-view §5.3 storage class for the
                    // file-scope enumeration path (statics + TLS).
                    let addr = qr.value().in_memory_location();
                    qr.storage = compute_storage_for_variable(&die_ref, addr, self.debugger);
                    out.push(qr);
                }
            }
        }
        Ok(out)
    }

    /// Namespace root component (the user's crate name) for the
    /// current function. Returns `None` if the PC isn't in a known
    /// compilation unit / function — falls back to "no crate
    /// filter" so the user still sees *something*.
    fn current_crate_namespace_root(&self) -> Option<String> {
        let ecx = self.debugger.ecx();
        let debugee = &self.debugger.debugee;
        let di = debugee.debug_info(ecx.location().pc).ok()?;
        let (func, _) = di.find_function_by_pc(ecx.location().global_pc).ok()??;
        let ns = func.namespace();
        ns.as_parts().first().cloned()
    }

    /// Compilation-unit id for the current PC's function. Returns
    /// `None` if the PC isn't in a known compilation unit.
    fn current_unit_id(&self) -> Option<uuid::Uuid> {
        let ecx = self.debugger.ecx();
        let debugee = &self.debugger.debugee;
        let di = debugee.debug_info(ecx.location().pc).ok()?;
        let (func, _) = di.find_function_by_pc(ecx.location().global_pc).ok()??;
        Some(func.unit().id)
    }
}

/// Breadth filter for [`DqeExecutor::query_file_scope`]. See
/// variables-view.md §4 for the user-facing setting key
/// (`variablesView.statics.scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileScopeFilter {
    /// Only variables whose namespace root matches the current
    /// frame's crate. **Default.** Avoids flooding the pane with
    /// std / dependency statics.
    CurrentCrate,
    /// Only variables in the current PC's compilation unit.
    CurrentUnit,
    /// All file-scope variables across all loaded debug-info.
    All,
}

/// Which slice of the file-scope variable space to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileScopeKind {
    /// `static`s (any segment).
    Statics,
    /// `thread_local!`s.
    ThreadLocals,
}

/// Walk a Variable DIE's `DW_AT_location` and classify the storage
/// class (variables-view §5.3). Cross-references the evaluated
/// address (passed in via `addr` from the parsed Value, no need to
/// re-evaluate) with the segment-kind index to split Static into
/// RO / RW.
fn compute_storage_for_variable(
    die_ref: &FatDieRef<'_, Variable>,
    addr: Option<usize>,
    dbg: &Debugger,
) -> Option<StorageClass> {
    use crate::debugger::variable::storage;
    let pc = dbg.ecx().location().global_pc;
    let expr = die_ref.location_expression(pc);
    let encoding = die_ref.unit_encoding();
    Some(storage::classify(expr.as_ref(), addr, encoding, dbg))
}

/// Interned name symbols for the three names rustc gives to
/// `thread_local!` internals. Cached for fast `is_tls_internal`
/// checks across many variables in one enumeration pass.
struct TlsInternalNames {
    key: Option<string_interner::DefaultSymbol>,
    val: Option<string_interner::DefaultSymbol>,
    rust_std_internal: Option<string_interner::DefaultSymbol>,
}

impl TlsInternalNames {
    fn resolve() -> Self {
        let lookup = |name: &str| gcx().with_interner(|i| i.get(name));
        Self {
            key: lookup("__KEY"),
            val: lookup("VAL"),
            rust_std_internal: lookup("__RUST_STD_INTERNAL_VAL"),
        }
    }

    fn is_tls_internal(&self, sym: string_interner::DefaultSymbol) -> bool {
        Some(sym) == self.key
            || Some(sym) == self.val
            || Some(sym) == self.rust_std_internal
    }
}

#[cfg(test)]
mod tls_classification_tests {
    use super::*;

    /// Intern the three known TLS internal names + a control, then
    /// check the classifier picks them correctly. Uses the real
    /// global interner so the prod path is exercised.
    #[test]
    fn classifies_only_rustc_tls_internals() {
        let key = gcx().with_interner(|i| i.get_or_intern("__KEY"));
        let val = gcx().with_interner(|i| i.get_or_intern("VAL"));
        let rsi = gcx().with_interner(|i| i.get_or_intern("__RUST_STD_INTERNAL_VAL"));
        let other = gcx().with_interner(|i| i.get_or_intern("MY_STATIC"));

        let names = TlsInternalNames::resolve();
        assert!(names.is_tls_internal(key));
        assert!(names.is_tls_internal(val));
        assert!(names.is_tls_internal(rsi));
        assert!(!names.is_tls_internal(other));
    }
}

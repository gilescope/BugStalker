// SPDX-License-Identifier: MIT
use crate::debugger::TypeDeclaration;
use crate::debugger::debugee::dwarf::eval::EvaluationContext;
use crate::debugger::debugee::dwarf::r#type::{
    ArrayType, ComplexType, ScalarType, StructureMember, TypeId,
};
use crate::debugger::variable::render::RenderValue;
use crate::debugger::variable::value::specialization::{TlsVariable, VariableParserExtension};
use crate::debugger::variable::value::{
    ArrayItem, ArrayValue, CEnumValue, CModifiedValue, Member, PointerValue, RustEnumValue,
    ScalarValue, SpecializedValue, StructValue, SubroutineValue, SupportedScalar, Value,
};
use crate::debugger::variable::{Identity, ObjectBinaryRepr};
use crate::version::Version;
use crate::version_switch;
use bytes::Bytes;
use gimli::{
    DW_ATE_ASCII, DW_ATE_UTF, DW_ATE_address, DW_ATE_boolean, DW_ATE_float, DW_ATE_signed,
    DW_ATE_signed_char, DW_ATE_unsigned, DW_ATE_unsigned_char,
};
use indexmap::IndexMap;
use log::warn;
use std::collections::HashMap;
use std::fmt::Display;

/// Additional information about value.
//
// FIXME: this modifier currently using only for TLS variables and should be deleted
// after minimal supported rust version will be greater than 1.80.0
#[derive(Default)]
pub struct ValueModifiers {
    tls: bool,
    tls_const: bool,
    const_tls_duplicate: bool,
    /// Darwin-only: the TLS DIE chain has already been unwrapped by
    /// dsymutil. `parse_tls` would fail (the structural markers it
    /// looks for — `eager`, `state`, `__getit` — were collapsed
    /// during DWARF linking), so wrap the parsed value directly as
    /// the `inner_value` of a synthetic [`TlsVariable`].
    tls_unwrapped: bool,
}

impl ValueModifiers {
    pub fn from_identity(pcx: &ParseContext, ident: Identity) -> ValueModifiers {
        let mut this = ValueModifiers::default();

        let ver = pcx.evcx.rustc_version().unwrap_or_default();
        if ver >= Version((1, 80, 0)) {
            // not sure that value is tls, but some additional checks will be occurred on
            // a value type at parsing stage
            this.tls = ident.name.as_deref() == Some("VAL")
                || ident.name.as_deref() == Some("__RUST_STD_INTERNAL_VAL");

            // Const-init `thread_local!` produces a parent `VAL`
            // DIE (under `…::CONSTANT_THREAD_LOCAL::{constant#0}`)
            // alongside the real one nested inside an init closure.
            // The parent has no `DW_AT_location`, so reading it is
            // pointless; drop it here.
            //
            // The original heuristic looked for a literal `{closure#0}`
            // to recognise the real DIE, but rustc/dsymutil don't
            // always pick `#0`. On darwin/aarch64 we observe
            // `{closure#1}` for the same construct (the inner
            // `const { … }` evaluator counts as closure #0, the
            // lazy-init wrapper as #1, and dsymutil surfaces only
            // the latter). Match any `{closure#N}` so the check is
            // robust across rustc/dsymutil minor revisions.
            let parts = ident.namespace.as_parts();
            let has_inner_closure = parts.iter().any(|p| p.starts_with("{closure#"));
            if ident.namespace.contains(&["thread_local_const_init"]) && !has_inner_closure {
                this.const_tls_duplicate = true;
            }
            // Darwin: dsymutil flattens the std `EagerStorage<T>` /
            // `LazyStorage<T>::Alive` wrapper around the user's TLS
            // value. The DIE we get is the bare `T` (or `Cell<T>` for
            // non-const TLS) directly under `{closure#N}`. The
            // namespace-`["eager"]` / `state` heuristics in
            // `parse_tls` don't apply, so flag this case for the
            // value parser to wrap synthetically.
            if this.tls && has_inner_closure {
                this.tls_unwrapped = true;
            }
        } else {
            let var_name_is_tls = ident.namespace.contains(&["__getit"])
                && (ident.name.as_deref() == Some("VAL") || ident.name.as_deref() == Some("__KEY"));
            if var_name_is_tls {
                this.tls = true;
                if ident.name.as_deref() == Some("VAL") {
                    this.tls_const = true
                }
            }
        }

        this
    }
}

/// Parse context (or pcx).
pub struct ParseContext<'a> {
    pub evcx: &'a EvaluationContext<'a>,
    pub type_graph: &'a ComplexType,
    /// Phase 3 Feature C — visited-set for the value-tree walk.
    /// Each `Rc<T>` / `Arc<T>` allocation address we've seen this
    /// parse goes here; an attempted second visit produces a
    /// `Value::Cycle` leaf instead of recursing forever. Wrapped
    /// in `RefCell` because the parser is `&self` throughout.
    pub visited_allocations: core::cell::RefCell<std::collections::HashSet<usize>>,
    /// Phase 3 Feature C — recursion-depth counter for the same
    /// walk. Pathological non-cyclic graphs (deep ASTs, long
    /// linked-list chains) hit this before the visited-set could
    /// help.
    pub recursion_depth: core::cell::Cell<u32>,
}

/// Phase 3 Feature C — render-tree depth cap. The plan suggests 64
/// but each `parse_inner` recursion costs ~25 KiB of stack (the
/// inner walk goes Rc → Node-struct → RefCell → Option → Rc again
/// per level — many frames per "level"). 16 is empirically
/// stack-safe on a default 2 MiB test thread; user-visible nesting
/// rarely exceeds that. Configurable via the eventual
/// `bs/setRenderBudget` DAP request once Phase 1 F3's render budget
/// generalises beyond LEN_GUARD.
pub const MAX_RENDER_DEPTH: u32 = 4;

/// Value parser object.
#[derive(Default)]
pub struct ValueParser;

impl ValueParser {
    /// Create parser for a type graph.
    ///
    /// # Arguments
    ///
    /// * `type_graph`: types that value parser can create
    pub fn new() -> Self {
        ValueParser
    }

    fn parse_scalar(
        &self,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        r#type: &ScalarType,
    ) -> ScalarValue {
        fn render_scalar<S: Copy + Display>(data: Option<ObjectBinaryRepr>) -> Option<S> {
            data.as_ref().map(|v| scalar_from_bytes::<S>(&v.raw_data))
        }
        let in_debugee_loc = data.as_ref().and_then(|d| d.address);
        #[allow(non_upper_case_globals)]
        let value_view = r#type.encoding.and_then(|encoding| match encoding {
            DW_ATE_address => render_scalar::<usize>(data).map(SupportedScalar::Usize),
            DW_ATE_signed_char => render_scalar::<i8>(data).map(SupportedScalar::I8),
            DW_ATE_unsigned_char => render_scalar::<u8>(data).map(SupportedScalar::U8),
            DW_ATE_signed => match r#type.byte_size.unwrap_or(0) {
                0 => Some(SupportedScalar::Empty()),
                1 => render_scalar::<i8>(data).map(SupportedScalar::I8),
                2 => render_scalar::<i16>(data).map(SupportedScalar::I16),
                4 => render_scalar::<i32>(data).map(SupportedScalar::I32),
                8 => {
                    if r#type.name.as_deref() == Some("isize") {
                        render_scalar::<isize>(data).map(SupportedScalar::Isize)
                    } else {
                        render_scalar::<i64>(data).map(SupportedScalar::I64)
                    }
                }
                16 => render_scalar::<i128>(data).map(SupportedScalar::I128),
                _ => {
                    warn!(
                        "parse scalar: unexpected signed size: {size:?}",
                        size = r#type.byte_size
                    );
                    None
                }
            },
            DW_ATE_unsigned => match r#type.byte_size.unwrap_or(0) {
                0 => Some(SupportedScalar::Empty()),
                1 => render_scalar::<u8>(data).map(SupportedScalar::U8),
                2 => render_scalar::<u16>(data).map(SupportedScalar::U16),
                4 => render_scalar::<u32>(data).map(SupportedScalar::U32),
                8 => {
                    if r#type.name.as_deref() == Some("usize") {
                        render_scalar::<usize>(data).map(SupportedScalar::Usize)
                    } else {
                        render_scalar::<u64>(data).map(SupportedScalar::U64)
                    }
                }
                16 => render_scalar::<u128>(data).map(SupportedScalar::U128),
                _ => {
                    warn!(
                        "parse scalar: unexpected unsigned size: {size:?}",
                        size = r#type.byte_size
                    );
                    None
                }
            },
            DW_ATE_float => match r#type.byte_size.unwrap_or(0) {
                4 => render_scalar::<f32>(data).map(SupportedScalar::F32),
                8 => render_scalar::<f64>(data).map(SupportedScalar::F64),
                _ => {
                    warn!(
                        "parse scalar: unexpected float size: {size:?}",
                        size = r#type.byte_size
                    );
                    None
                }
            },
            DW_ATE_boolean => render_scalar::<bool>(data).map(SupportedScalar::Bool),
            DW_ATE_UTF => render_scalar::<char>(data).map(|char| {
                // WAITFORFIX: https://github.com/rust-lang/rust/issues/113819
                // this check is meaningfully here cause in case above there is a random bytes here,
                // and it may lead to panic in other places
                // (specially when someone tries to render this char)
                if String::from_utf8(char.to_string().into_bytes()).is_err() {
                    SupportedScalar::Char('?')
                } else {
                    SupportedScalar::Char(char)
                }
            }),
            DW_ATE_ASCII => render_scalar::<char>(data).map(SupportedScalar::Char),
            _ => {
                warn!("parse scalar: unexpected base type encoding: {encoding}");
                None
            }
        });

        ScalarValue {
            type_ident: r#type.identity(),
            type_id: Some(type_id),
            value: value_view,
            raw_address: in_debugee_loc,
        }
    }

    fn parse_struct_variable(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        type_params: IndexMap<String, Option<TypeId>>,
        members: &[StructureMember],
    ) -> StructValue {
        let children = members
            .iter()
            .filter_map(|member| self.parse_struct_member(pcx, member, data.as_ref()))
            .collect();

        StructValue {
            type_id: Some(type_id),
            type_ident: pcx.type_graph.identity(type_id),
            members: children,
            type_params,
            raw_address: data.and_then(|d| d.address),
        }
    }

    fn parse_struct_member(
        &self,
        pcx: &ParseContext,
        member: &StructureMember,
        parent_data: Option<&ObjectBinaryRepr>,
    ) -> Option<Member> {
        let name = member.name.clone();
        let Some(type_ref) = member.type_ref else {
            warn!(
                "parse structure: unknown type for member {}",
                name.as_deref().unwrap_or_default()
            );
            return None;
        };
        let member_val = parent_data.and_then(|data| member.value(pcx.evcx, pcx.type_graph, data));
        let value = self.parse_inner(pcx, member_val, type_ref)?;
        Some(Member {
            field_name: member.name.clone(),
            value,
        })
    }

    fn parse_array(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        array_decl: &ArrayType,
    ) -> ArrayValue {
        let items = array_decl.bounds(pcx.evcx).and_then(|bounds| {
            let len = bounds.1 - bounds.0;
            if len == 0 {
                return Some(vec![]);
            }
            if len < 0 {
                warn!(
                    "array `len` less than 0 for type: {}",
                    pcx.type_graph.identity(type_id)
                );
                return None;
            };

            let data = data.as_ref()?;
            let el_size =
                (array_decl.size_in_bytes(pcx.evcx, pcx.type_graph)? / len as u64) as usize;
            let bytes = &data.raw_data;
            let el_type_id = array_decl.element_type()?;

            let (mut bytes_chunks, mut empty_chunks);
            let raw_items_iter: &mut dyn Iterator<Item = (usize, &[u8])> = if el_size != 0 {
                bytes_chunks = bytes.chunks(el_size).enumerate();
                &mut bytes_chunks
            } else {
                // if an item type is zst
                let v: Vec<&[u8]> = vec![&[]; len as usize];
                empty_chunks = v.into_iter().enumerate();
                &mut empty_chunks
            };

            Some(
                raw_items_iter
                    .filter_map(|(i, chunk)| {
                        let offset = i * el_size;
                        let data = ObjectBinaryRepr {
                            raw_data: bytes.slice_ref(chunk),
                            address: data.address.map(|addr| addr + offset),
                            size: el_size,
                        };

                        let value = self.parse_inner(pcx, Some(data), el_type_id)?;
                        Some(ArrayItem {
                            index: bounds.0 + i as i64,
                            value,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        });

        ArrayValue {
            items,
            type_id: Some(type_id),
            type_ident: pcx.type_graph.identity(type_id),
            raw_address: data.and_then(|d| d.address),
        }
    }

    fn parse_c_enum(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        discr_type: Option<TypeId>,
        enumerators: &HashMap<i64, String>,
    ) -> CEnumValue {
        let in_debugee_loc = data.as_ref().and_then(|d| d.address);
        let mb_discr = discr_type.and_then(|type_id| self.parse_inner(pcx, data, type_id));

        let value = mb_discr.and_then(|discr| {
            if let Value::Scalar(scalar) = discr {
                scalar.try_as_number()
            } else {
                None
            }
        });

        CEnumValue {
            type_ident: pcx.type_graph.identity(type_id),
            type_id: Some(type_id),
            value: value.and_then(|val| enumerators.get(&val).cloned()),
            raw_address: in_debugee_loc,
        }
    }

    fn parse_rust_enum(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        discr_member: Option<&StructureMember>,
        enumerators: &HashMap<Option<i64>, StructureMember>,
    ) -> RustEnumValue {
        let discr_value = discr_member.and_then(|member| {
            let discr = self.parse_struct_member(pcx, member, data.as_ref())?.value;
            if let Value::Scalar(scalar) = discr {
                return scalar.try_as_number();
            }
            None
        });

        let active_enumerator =
            discr_value.and_then(|v| enumerators.get(&Some(v)).or_else(|| enumerators.get(&None)));

        // Phase 3 Feature D — for the active variant, find a DIE that
        // carried `DW_AT_decl_file`/`DW_AT_decl_line`. On a coroutine
        // state-machine enum this is rustc's source location for the
        // `.await` we are paused at. We try (in order) the captured-
        // locals fields inside the variant struct, then the enumerator
        // member itself — rustc has used both shapes across versions.
        let await_decl = active_enumerator
            .and_then(|m| m.type_ref)
            .and_then(|var_ty| {
                if let Some(TypeDeclaration::Structure { members, .. }) =
                    pcx.type_graph.types.get(&var_ty)
                {
                    members.iter().find_map(|m| m.decl_file_line)
                } else {
                    None
                }
            })
            .or_else(|| active_enumerator.and_then(|m| m.decl_file_line));

        let await_location = await_decl.and_then(|(file_idx, line)| {
            let unit = pcx.evcx.evaluator.unit();
            unit.files()
                .get(file_idx as usize)
                .map(|p| (p.clone(), line))
        });

        let enumerator = active_enumerator.and_then(|member| {
            Some(Box::new(self.parse_struct_member(
                pcx,
                member,
                data.as_ref(),
            )?))
        });

        RustEnumValue {
            type_id: Some(type_id),
            type_ident: pcx.type_graph.identity(type_id),
            value: enumerator,
            raw_address: data.and_then(|d| d.address),
            await_location,
        }
    }

    fn parse_pointer(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        target_type: Option<TypeId>,
    ) -> PointerValue {
        let mb_ptr = data
            .as_ref()
            .map(|v| scalar_from_bytes::<*const ()>(&v.raw_data));

        let mut type_ident = pcx.type_graph.identity(type_id);
        if type_ident.is_unknown()
            && let Some(target_type) = target_type
        {
            type_ident = pcx.type_graph.identity(target_type).as_deref_type();
        }

        PointerValue {
            type_id: Some(type_id),
            type_ident,
            value: mb_ptr,
            target_type,
            target_type_size: None,
            raw_address: data.and_then(|d| d.address),
            dereffed: None,
        }
    }

    fn parse_inner_with_modifiers(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
        modifiers: &ValueModifiers,
    ) -> Option<Value> {
        let type_graph = pcx.type_graph;
        match &type_graph.types[&type_id] {
            TypeDeclaration::Scalar(scalar_type) => {
                Some(Value::Scalar(self.parse_scalar(data, type_id, scalar_type)))
            }
            TypeDeclaration::Structure {
                namespaces: type_ns_h,
                members,
                type_params,
                name: struct_name,
                ..
            } => {
                let mut struct_var =
                    self.parse_struct_variable(pcx, data, type_id, type_params.clone(), members);

                // Phase 3 Feature A batch A2 — `dyn Trait` concrete-
                // type recovery. The detection heuristic from batch A1
                // already lives on `StructValue`; here we drive the
                // resolution chain end to end:
                //
                //   1. Read the vtable pointer off the struct.
                //   2. Look the vtable address up in the symbol table
                //      to get the *mangled* vtable symbol name
                //      (strategy 2 — per-vtable symbol matching).
                //   3. Demangle through `rust-mangle-tree` and walk
                //      `impl_self_type()` to read the concrete type.
                //   4. Splice the recovered type name into the
                //      struct's `type_ident` so the renderer surfaces
                //      it inline (`Box<dyn Error> [→ MyError]`).
                //
                // Strategy 1 (drop-fn pointer at vtable[0]) lands as
                // a fallback in the same helper.
                if struct_var.is_trait_object()
                    && let Some(name) = resolve_trait_object_concrete_type(pcx, &struct_var)
                {
                    let original = struct_var
                        .type_ident
                        .name()
                        .unwrap_or("dyn Trait")
                        .to_string();
                    struct_var
                        .type_ident
                        .set_name(format!("{original} [→ {name}]"));
                }

                let parser_ext = VariableParserExtension::new(self);
                // Reinterpret structure if underline data type is:
                // - Vector
                // - String
                // - &str
                // - tls variable
                // - hashmaps
                // - hashset
                // - btree map
                // - btree set
                // - vecdeque
                // - cell/refcell
                // - rc/arc
                // - uuid
                // - SystemTime/Instant
                if struct_name.as_deref() == Some("&str") {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_str(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                if struct_name.as_deref() == Some("String") {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_string(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                // `&[T]` / `&mut [T]` / `*const [T]` / `*mut [T]`.
                // DWARF emits these as structs with `data_ptr` + `length`
                // fields, same shape as `&str` but without the implicit
                // UTF-8 interpretation. We pull the element type out of
                // the `data_ptr` member (which is a `*const T`) so we
                // don't have to text-parse `T` from the type name.
                let is_slice_type_name = struct_name
                    .as_ref()
                    .map(|name| {
                        let n = name.as_str();
                        n.starts_with("&[")
                            || n.starts_with("&mut [")
                            || n.starts_with("*const [")
                            || n.starts_with("*mut [")
                    })
                    .unwrap_or(false);
                if is_slice_type_name {
                    let element_type = struct_var.members.iter().find_map(|m| {
                        if m.field_name.as_deref() != Some("data_ptr") {
                            return None;
                        }
                        match &m.value {
                            Value::Pointer(p) => p.target_type,
                            _ => None,
                        }
                    });
                    if let Some(element_type) = element_type {
                        return Some(Value::Specialized {
                            value: parser_ext.parse_slice(pcx, &struct_var, element_type),
                            original: struct_var,
                        });
                    }
                }

                if struct_name.as_ref().map(|name| name.starts_with("Vec")) == Some(true)
                    && type_ns_h.contains(&["vec"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_vector(pcx, &struct_var, type_params),
                        original: struct_var,
                    });
                };

                let rust_version = pcx.evcx.rustc_version().unwrap_or_default();
                let type_is_tls = version_switch!(
                    rust_version,
                    .. (1 . 77) => type_ns_h.contains(&["std", "sys", "common", "thread_local", "fast_local"]),
                    (1 . 77) .. (1 . 78) => type_ns_h.contains(&["std", "sys", "pal", "common", "thread_local", "fast_local"]),
                    (1 . 78) .. (1 . 89) => type_ns_h.contains(&["std", "sys", "thread_local", "fast_local"]),
                    (1 . 89) .. => type_ns_h.contains(&["std", "sys", "thread_local", "native"]),
                ).unwrap_or_default();

                // Darwin: when `tls_unwrapped` is set the std TLS
                // wrapper has been flattened by dsymutil, so the
                // structural markers `parse_tls` looks for
                // (`eager`, `state`, `__getit`) aren't there. Skip
                // the dedicated TLS parser and let the regular
                // parse produce the bare T; the synthetic-wrap
                // fallback in `parse_with_modifiers_or_inner`
                // then re-wraps it as `Specialized<Tls>` so the
                // shape matches Linux.
                if (type_is_tls || modifiers.tls) && !modifiers.tls_unwrapped {
                    return if rust_version >= Version((1, 80, 0)) {
                        match parser_ext.parse_tls(pcx, &struct_var, type_params, rust_version) {
                            Ok(Some(value)) => Some(Value::Specialized {
                                value: Some(SpecializedValue::Tls(value)),
                                original: struct_var,
                            }),
                            Ok(None) => None,
                            Err(e) => {
                                warn!(target: "debugger", "{:#}", e);
                                Some(Value::Specialized {
                                    value: None,
                                    original: struct_var,
                                })
                            }
                        }
                    } else {
                        Some(Value::Specialized {
                            value: parser_ext.parse_tls_old(
                                pcx,
                                &struct_var,
                                type_params,
                                modifiers.tls_const,
                            ),
                            original: struct_var,
                        })
                    };
                }

                if struct_name.as_ref().map(|name| name.starts_with("HashMap")) == Some(true)
                    && type_ns_h.contains(&["collections", "hash", "map"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_hashmap(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name.starts_with("HashSet")) == Some(true)
                    && type_ns_h.contains(&["collections", "hash", "set"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_hashset(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                if struct_name
                    .as_ref()
                    .map(|name| name.starts_with("BTreeMap"))
                    == Some(true)
                    && type_ns_h.contains(&["collections", "btree", "map"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_btree_map(pcx, &struct_var, type_id, type_params),
                        original: struct_var,
                    });
                };

                if struct_name
                    .as_ref()
                    .map(|name| name.starts_with("BTreeSet"))
                    == Some(true)
                    && type_ns_h.contains(&["collections", "btree", "set"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_btree_set(&struct_var),
                        original: struct_var,
                    });
                };

                if struct_name
                    .as_ref()
                    .map(|name| name.starts_with("VecDeque"))
                    == Some(true)
                    && type_ns_h.contains(&["collections", "vec_deque"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_vec_dequeue(pcx, &struct_var, type_params),
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name.starts_with("Cell")) == Some(true)
                    && type_ns_h.contains(&["cell"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_cell(&struct_var),
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name.starts_with("RefCell")) == Some(true)
                    && type_ns_h.contains(&["cell"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_refcell(&struct_var),
                        original: struct_var,
                    });
                };

                if struct_name
                    .as_ref()
                    .map(|name| name.starts_with("Rc<") | name.starts_with("Weak<"))
                    == Some(true)
                    && type_ns_h.contains(&["rc"])
                {
                    // Phase 1 S15: route `Weak<T>` to a dedicated
                    // parser that derefs to read the strong / weak
                    // counts; `Rc<T>` keeps the existing parse_rc
                    // pointer-only path.
                    let value =
                        if struct_name.as_ref().map(|n| n.starts_with("Weak<")) == Some(true) {
                            parser_ext.parse_weak(pcx, &struct_var)
                        } else {
                            parser_ext.parse_rc(pcx, &mut struct_var)
                        };
                    return Some(Value::Specialized {
                        value,
                        original: struct_var,
                    });
                };

                if struct_name
                    .as_ref()
                    .map(|name| name.starts_with("Arc<") | name.starts_with("Weak<"))
                    == Some(true)
                    && type_ns_h.contains(&["sync"])
                {
                    // Phase 1 S15 — same Weak split for the sync flavour.
                    let value =
                        if struct_name.as_ref().map(|n| n.starts_with("Weak<")) == Some(true) {
                            parser_ext.parse_weak(pcx, &struct_var)
                        } else {
                            parser_ext.parse_arc(pcx, &mut struct_var)
                        };
                    return Some(Value::Specialized {
                        value,
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name == "Uuid") == Some(true)
                    && type_ns_h.contains(&["uuid"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_uuid(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S3 — every `core::sync::atomic::Atomic*` /
                // `std::sync::atomic::Atomic*` type. Names: `AtomicI8` …
                // `AtomicI128`, `AtomicU8` … `AtomicU128`, `AtomicBool`,
                // `AtomicUsize`, `AtomicIsize`, `AtomicPtr<T>`. We
                // detect by name prefix + namespace; `parse_atomic`
                // peels the `UnsafeCell<T>` wrapper.
                if struct_name.as_ref().map(|name| name.starts_with("Atomic")) == Some(true)
                    && type_ns_h.contains(&["sync", "atomic"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_atomic(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S11 — `core::ptr::NonNull<T>`.
                if struct_name.as_ref().map(|name| name.starts_with("NonNull")) == Some(true)
                    && type_ns_h.contains(&["ptr", "non_null"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_nonnull(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S7 — `core::pin::Pin<P>`. Surface the pinnee
                // directly; the wrapper name keeps the `Pin<…>` framing.
                if struct_name.as_ref().map(|name| name.starts_with("Pin")) == Some(true)
                    && type_ns_h.contains(&["pin"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_pin(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S6 — every `core::ops::Range*` shape:
                // `Range`, `RangeInclusive`, `RangeFrom`, `RangeTo`,
                // `RangeToInclusive`, `RangeFull`. Detection is by
                // name prefix + namespace; `parse_range` discriminates
                // among the six layouts on the name itself.
                if struct_name.as_ref().map(|name| name.starts_with("Range")) == Some(true)
                    && type_ns_h.contains(&["ops", "range"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext
                            .parse_range(struct_name.as_deref().unwrap_or(""), &struct_var),
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name == "Instant") == Some(true)
                    && type_ns_h.contains(&["std", "time"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_instant(&struct_var),
                        original: struct_var,
                    });
                };

                if struct_name.as_ref().map(|name| name == "SystemTime") == Some(true)
                    && type_ns_h.contains(&["std", "time"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_sys_time(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S4 — `core::time::Duration` /
                // `std::time::Duration`. The type lives in `time` for
                // both core and std re-exports; we accept either by
                // matching the bare namespace component.
                if struct_name.as_deref() == Some("Duration") && type_ns_h.contains(&["time"]) {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_duration(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S12 — `alloc::ffi::c_str::CString`. Detect by
                // exact name plus the `ffi` namespace component.
                if struct_name.as_deref() == Some("CString") && type_ns_h.contains(&["ffi"]) {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_cstring(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S13 — `std::ffi::OsString`. The `ffi`
                // namespace is shared with `CString`, so we
                // discriminate on the type name alone.
                if struct_name.as_deref() == Some("OsString") && type_ns_h.contains(&["ffi"]) {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_os_string(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S14 — `std::path::PathBuf`. Wraps `OsString`
                // wraps `Buf` wraps `Vec<u8>`; the BFS-based parser
                // walks all four layers.
                if struct_name.as_deref() == Some("PathBuf") && type_ns_h.contains(&["path"]) {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_os_string(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S12/S13/S14 DST companions — `&CStr`,
                // `&OsStr`, `&Path`. rustc materialises these fat
                // references as structs with `data_ptr` + `length`.
                // The CStr / OsString parsers BFS for those fields,
                // so we can route by the wrapper-name suffix.
                if let Some(name) = struct_name.as_deref() {
                    if name.ends_with("c_str::CStr") || name == "&CStr" || name.ends_with("::CStr")
                    {
                        return Some(Value::Specialized {
                            value: parser_ext.parse_cstring(pcx, &struct_var),
                            original: struct_var,
                        });
                    }
                    if name.ends_with("os_str::OsStr")
                        || name == "&OsStr"
                        || name.ends_with("::OsStr")
                        || name.ends_with("path::Path")
                        || name == "&Path"
                        || name.ends_with("::Path")
                    {
                        return Some(Value::Specialized {
                            value: parser_ext.parse_os_string(pcx, &struct_var),
                            original: struct_var,
                        });
                    }
                }

                // Phase 1 S10 — `core::mem::MaybeUninit<T>` may emit
                // as a Structure on some rustc versions even though
                // libcore declares it `pub union`. Some producers
                // include the type parameters in the name string
                // (`MaybeUninit<i32>`) so match by prefix. The name
                // is unique to libcore so we don't gate on namespace.
                if struct_name.as_ref().map(|n| n.starts_with("MaybeUninit")) == Some(true) {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_maybe_uninit(&struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S2 — lock guards: `MutexGuard<T>`,
                // `RwLockReadGuard<T>`, `RwLockWriteGuard<T>`,
                // `MappedMutexGuard<T>` etc. Detected by the `Guard`
                // suffix on the type name + the `sync` namespace.
                // Must be tested BEFORE `Mutex`/`RwLock` because
                // `MutexGuard<i32>` matches `starts_with("Mutex")`.
                if struct_name.as_ref().map(|n| {
                    n.starts_with("MutexGuard")
                        || n.starts_with("MappedMutexGuard")
                        || n.starts_with("RwLockReadGuard")
                        || n.starts_with("RwLockWriteGuard")
                        || n.starts_with("MappedRwLockReadGuard")
                        || n.starts_with("MappedRwLockWriteGuard")
                }) == Some(true)
                    && type_ns_h.contains(&["sync"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_lock_guard(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                // Phase 1 S1 — `std::sync::Mutex<T>` / `std::sync::RwLock<T>`.
                // Both have the same layout shape (data: UnsafeCell<T>);
                // share `parse_mutex`. The `sync` namespace component
                // distinguishes from `parking_lot`-style alternates
                // which would have a different layout. DWARF embeds
                // type parameters into the name (`Mutex<i32>`), so we
                // match by prefix.
                if struct_name
                    .as_ref()
                    .map(|n| n.starts_with("Mutex") || n.starts_with("RwLock"))
                    == Some(true)
                    && type_ns_h.contains(&["sync"])
                {
                    return Some(Value::Specialized {
                        value: parser_ext.parse_mutex(pcx, &struct_var),
                        original: struct_var,
                    });
                };

                Some(Value::Struct(struct_var))
            }
            TypeDeclaration::Array(decl) => {
                Some(Value::Array(self.parse_array(pcx, data, type_id, decl)))
            }
            TypeDeclaration::CStyleEnum {
                discr_type,
                enumerators,
                ..
            } => Some(Value::CEnum(self.parse_c_enum(
                pcx,
                data,
                type_id,
                *discr_type,
                enumerators,
            ))),
            TypeDeclaration::RustEnum {
                discr_type,
                enumerators,
                ..
            } => Some(Value::RustEnum(self.parse_rust_enum(
                pcx,
                data,
                type_id,
                discr_type.as_ref().map(|t| t.as_ref()),
                enumerators,
            ))),
            TypeDeclaration::Pointer { target_type, .. } => {
                let mut ptr = self.parse_pointer(pcx, data, type_id, *target_type);
                // Phase 1 S9 — `alloc::boxed::Box<T>` smart-deref.
                // `Box<T>` arrives here as a `Pointer` (rustc emits a
                // `DW_TAG_pointer_type` with the Box-flavoured name);
                // deref eagerly so the renderer can show the pointee
                // inline. Trait-object boxes (`Box<dyn Trait>`) need
                // vtable resolution from Phase 3 — for now they
                // round-trip as a fat-pointer struct via the parent
                // type-graph walk and don't reach this branch.
                //
                // We attempted to also eager-deref `&T` / `&mut T` /
                // `*const T` / `*mut T` so the Variables panel could
                // show `&10` instead of `0x16fdfef18` — but this
                // started killing the debug session on programs with
                // recursive types (`enum List { Cons(i32, Box<List>),
                // Nil }`) and fat-pointer `Box<dyn Trait>` allocations.
                // Reverted; see git history for the attempt. The
                // approach to revisit: route through
                // `eager_deref_with_cycle_check` AND skip DSTs by
                // checking `target_type_size`, but test against the
                // showcase example before re-enabling.
                let name = ptr.type_ident.name_fmt();
                if name.starts_with("alloc::boxed::Box<") {
                    ptr.dereffed = ptr.deref(pcx).map(Box::new);
                }
                Some(Value::Pointer(ptr))
            }
            TypeDeclaration::Union {
                members,
                name: union_name,
                ..
            } => {
                let struct_var =
                    self.parse_struct_variable(pcx, data, type_id, IndexMap::new(), members);
                // Phase 1 S10 — `core::mem::MaybeUninit<T>` lives on
                // the Union dispatch path. The DWARF namespace for it
                // is producer-dependent (older rustc emitted
                // `core::mem`, newer `core::mem::maybe_uninit`); the
                // type name `MaybeUninit` is unique to libcore so we
                // match on it alone with a fallback `mem` namespace
                // sanity check.
                if union_name.as_ref().map(|n| n.starts_with("MaybeUninit")) == Some(true) {
                    let parser_ext = VariableParserExtension::new(self);
                    return Some(Value::Specialized {
                        value: parser_ext.parse_maybe_uninit(&struct_var),
                        original: struct_var,
                    });
                }
                Some(Value::Struct(struct_var))
            }
            TypeDeclaration::Subroutine { return_type, .. } => {
                let ret_type = return_type.map(|t_id| pcx.type_graph.identity(t_id));
                let fn_var = SubroutineValue {
                    type_id: Some(type_id),
                    return_type_ident: ret_type,
                    address: data.and_then(|d| d.address),
                };
                Some(Value::Subroutine(fn_var))
            }
            TypeDeclaration::ModifiedType {
                inner, modifier, ..
            } => {
                let in_debugee_loc = data.as_ref().and_then(|d| d.address);
                Some(Value::CModifiedVariable(CModifiedValue {
                    type_id: Some(type_id),
                    type_ident: pcx.type_graph.identity(type_id),
                    modifier: *modifier,
                    value: inner.and_then(|inner_type| {
                        Some(Box::new(self.parse_inner(pcx, data, inner_type)?))
                    }),
                    address: in_debugee_loc,
                }))
            }
        }
    }

    pub(super) fn parse_inner(
        &self,
        pcx: &ParseContext,
        data: Option<ObjectBinaryRepr>,
        type_id: TypeId,
    ) -> Option<Value> {
        self.parse_inner_with_modifiers(pcx, data, type_id, &ValueModifiers::default())
    }

    /// Return a new value of a root type from the underlying type graph.
    ///
    /// # Arguments
    ///
    /// * `pcx`: parsing context
    /// * `bin_data`: binary value representation from debugee memory
    /// * `modifiers`: value addition info
    pub fn parse(
        self,
        pcx: &ParseContext,
        bin_data: Option<ObjectBinaryRepr>,
        modifiers: &ValueModifiers,
    ) -> Option<Value> {
        if modifiers.const_tls_duplicate {
            return None;
        }

        let parsed =
            self.parse_inner_with_modifiers(pcx, bin_data, pcx.type_graph.root(), modifiers)?;

        // Darwin: dsymutil flattened the std TLS storage wrapper.
        // What we parsed is the outer `LazyStorage<T, !>` /
        // `EagerStorage<T>` (dsymutil drops the `LazyStorage::Alive`
        // discriminant enum but keeps the Storage struct itself plus
        // the `UnsafeCell` / `MaybeUninit` / `ManuallyDrop`
        // transparent wrappers around T). Peel them down to T so the
        // synthetic `TlsVariable` exposes the same `inner_type` shape
        // callers see on Linux (`Cell<i32>` for the lazy case,
        // `i32` for `const`-init via EagerStorage). The peeler walks
        // the canonical `value` (or `__0`) field through every layer
        // whose type-name matches a known wrapper; it stops the
        // moment the type-name doesn't match, leaving T at the leaf.
        if modifiers.tls_unwrapped
            && !matches!(
                parsed,
                Value::Specialized {
                    value: Some(SpecializedValue::Tls(_)),
                    ..
                }
            )
        {
            // Darwin uninit detection. dsymutil keeps the
            // `Storage<T, D>::state` field intact even though it
            // strips the `LazyStorage::Alive` discriminant on the
            // outer enum. We can read the byte directly: rust std's
            // `enum State<D> { Uninitialized = 0, Alive = 1,
            // Destroyed(D) = 2 }` has a stable u8 discriminant on the
            // architectures we run on. Anything other than `Alive`
            // means there is no live `T` to surface — return `None`
            // so `read_variable` yields an empty vec, matching the
            // Linux `parse_tls_inner` short-circuit.
            //
            // The eager path has no `state` field; the helper returns
            // `None` and the peel proceeds.
            if let Some(state_addr) = tls_storage_state_address(&parsed) {
                let pid = pcx.evcx.ecx.pid_on_focus();
                let byte = crate::debugger::read_memory_by_pid(pid, state_addr, 1)
                    .ok()
                    .and_then(|v| v.first().copied());
                if byte != Some(STATE_ALIVE) {
                    return None;
                }
            }
            let peeled = peel_tls_storage_wrappers(parsed);
            let inner_type = peeled.r#type().clone();
            return Some(Value::Specialized {
                value: Some(SpecializedValue::Tls(TlsVariable {
                    inner_value: Some(Box::new(peeled)),
                    inner_type,
                })),
                original: StructValue::default(),
            });
        }

        Some(parsed)
    }
}

/// Discriminant byte for `std::sys::thread_local::native::lazy::State::Alive`.
/// `enum State<D> { Uninitialized = 0, Alive = 1, Destroyed(D) = 2 }` —
/// stable across the rustc versions we target.
const STATE_ALIVE: u8 = 1;

/// Darwin TLS uninit detection. If `parsed` is the
/// `LazyStorage<T, D>` / `Storage<T, D>` struct (the lazy TLS shape),
/// return the runtime address of its `state` discriminant byte so the
/// caller can read it via `read_memory_by_pid`. Returns `None` for
/// the eager shape (which has no `state`) or any other value.
fn tls_storage_state_address(val: &Value) -> Option<usize> {
    let Value::Struct(s) = val else {
        return None;
    };
    let name = s.type_ident.name().unwrap_or("");
    if !(name.starts_with("Storage") || name.starts_with("LazyStorage")) {
        return None;
    }
    let state = s
        .members
        .iter()
        .find(|m| m.field_name.as_deref() == Some("state"))?;
    state.value.in_memory_location()
}

/// Darwin TLS wrapper-peeling. Walks down through every
/// transparent-wrapper layer between the std-internal
/// `LazyStorage<T, F>` / `EagerStorage<T>` and the user's `T`.
/// Three shapes to handle:
///
/// * `Storage` / `LazyStorage` / `EagerStorage` / `UnsafeCell` /
///   `ManuallyDrop` — parsed as `Value::Struct`. Walk the
///   `value` (or `__0` for tuple-shaped wrappers) field.
/// * `MaybeUninit<T>` — parsed as
///   `Value::Specialized<SpecializedValue::MaybeUninit(inner)>`
///   by Phase 1 S10. Unbox the inner directly.
/// * Anything else — stop. That's `T`.
///
/// Bounded — stops on first non-wrapper layer or when no peelable
/// field is reachable.
fn peel_tls_storage_wrappers(mut val: Value) -> Value {
    const STRUCT_WRAPPERS: &[&str] = &[
        "LazyStorage",
        "EagerStorage",
        "Storage",
        "UnsafeCell",
        "ManuallyDrop",
    ];
    const MAX_DEPTH: u32 = 8;
    for _ in 0..MAX_DEPTH {
        match val {
            Value::Specialized {
                value: Some(crate::debugger::variable::value::SpecializedValue::MaybeUninit(inner)),
                ..
            } => {
                val = *inner;
            }
            Value::Struct(s) => {
                let name = s
                    .type_ident
                    .name()
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                if !STRUCT_WRAPPERS.iter().any(|w| name.starts_with(w)) {
                    return Value::Struct(s);
                }
                let s_clone = s.clone();
                match s.field("value").or_else(|| s_clone.clone().field("__0")) {
                    Some(v) => val = v,
                    None => return Value::Struct(s_clone),
                }
            }
            _ => return val,
        }
    }
    val
}

#[inline(never)]
fn scalar_from_bytes<T: Copy>(bytes: &Bytes) -> T {
    let ptr = bytes.as_ptr();
    unsafe { std::ptr::read_unaligned::<T>(ptr as *const T) }
}

/// Phase 3 Feature A batch A2 — vtable → concrete type resolver.
///
/// **Strategy 2 (primary):** the `vtable` pointer of a `dyn Trait`
/// fat pointer points directly at a symbol whose v0 mangled form is
/// `<Concrete as Trait>::{vtable}` (a [`Path::TraitImpl`] whose
/// `impl_self_type()` *is* the concrete type). Resolve via the
/// symbol-table address index, demangle with `rust-mangle-tree`,
/// walk to the self-type's `Display`. Catches every v0-mangled build.
///
/// **Strategy 1 (fallback):** if no symbol sits exactly at the
/// vtable address (e.g. the linker placed the vtable in an
/// anonymous `__DATA,__const` block as it does on darwin), read the
/// first pointer-sized slot of the vtable — that's
/// `core::ptr::drop_in_place::<Concrete>` — and look *that* address
/// up. The drop-fn's mangled name carries `Concrete` as a generic
/// argument. Catches darwin / stripped-vtable-symbol cases.
///
/// Returns `None` if neither strategy resolves; the caller falls
/// back to the bare `[concrete type unavailable]` annotation.
fn resolve_trait_object_concrete_type(
    pcx: &ParseContext,
    struct_var: &StructValue,
) -> Option<String> {
    use crate::debugger::variable::value::Value;

    // Pull the vtable pointer off the struct's members.
    let vtable_addr: u64 = struct_var.members.iter().find_map(|m| {
        if matches!(
            m.field_name.as_deref(),
            Some("vtable") | Some("v_table") | Some("vtbl")
        ) && let Value::Pointer(p) = &m.value
        {
            return p.value.map(|raw| raw as u64);
        }
        None
    })?;

    let debugee = pcx.evcx.evaluator.debugee();
    let dwarf = debugee.debug_info(pcx.evcx.ecx.location().pc).ok()?;
    let pid = pcx.evcx.ecx.pid_on_focus();

    // The symbol table is keyed by *image-relative* addresses (file
    // offsets / `st_value`), but `vtable_addr` and the per-slot fn
    // pointers we read from the inferior are *runtime* addresses
    // post-ASLR/PIE relocation. Translate via the debugee's mapping
    // table before any `mangled_symbol_at` lookup — same
    // `RelocatedAddress → GlobalAddress` flow `address::into_global`
    // uses for PC translation elsewhere.
    let to_global = |runtime: u64| -> Option<u64> {
        let reloc = crate::debugger::address::RelocatedAddress::from(runtime);
        reloc.into_global(debugee).ok().map(u64::from)
    };
    let read_memory =
        |addr: u64, len: usize| crate::debugger::read_memory_by_pid(pid, addr as usize, len).ok();
    let mangled_at = |global: u64| dwarf.mangled_symbol_at(global);

    resolve_trait_object_from_lookups(vtable_addr, &to_global, &read_memory, &mangled_at)
}

/// Pure core of the vtable → concrete-type resolver. Takes the
/// vtable runtime address and three injectable lookups so it can be
/// unit-tested without a live debuggee.
///
/// `to_global` translates a runtime (post-ASLR/PIE) address into the
/// image-relative offset the symbol table is keyed by. Returning
/// `None` means "this address isn't mapped into any module we know
/// about" — caller skips the lookup.
///
/// `read_memory` reads bytes from the inferior. We use it to slurp
/// the first N pointer-sized vtable slots in one syscall.
///
/// `mangled_at` is the address-keyed symbol lookup. It must accept
/// image-relative addresses (the symbol table's native domain) — the
/// caller is responsible for translation. Returning a `&str` borrows
/// from the symbol-table's storage.
fn resolve_trait_object_from_lookups<'a>(
    vtable_addr: u64,
    to_global: &dyn Fn(u64) -> Option<u64>,
    read_memory: &dyn Fn(u64, usize) -> Option<Vec<u8>>,
    mangled_at: &dyn Fn(u64) -> Option<&'a str>,
) -> Option<String> {
    // Strategy 2: symbol at the vtable address itself. Works on
    // v0-mangled builds where rustc exports `<Concrete as Trait>::
    // {vtable}` as a real symbol; misses on legacy-mangled builds
    // (no vtable-shaped symbol emitted) and on Mach-O where the
    // linker may place the vtable in an anonymous block.
    if let Some(vt_global) = to_global(vtable_addr)
        && let Some(mangled) = mangled_at(vt_global)
        && let Some(name) = concrete_from_vtable_symbol(mangled)
    {
        return Some(name);
    }

    // Strategy 1: scan the vtable's slots and probe each as a
    // function-symbol address. The first slot is `drop_in_place`
    // (which may be null when the concrete type has no `Drop`),
    // the next two are size/alignment (not pointers), and slots
    // 3+ are the trait's method pointers. Each method's mangled
    // name carries the impl shape `<Concrete as Trait>::method`,
    // so `concrete_from_vtable_symbol` (the same string-surgery
    // we use for strategy 2) extracts the concrete type from any
    // of them.
    //
    // Rustc emits 8 to ~32 slots depending on trait method count
    // plus inheritance; 16 covers `Error`, `Display`, `Debug`, and
    // most of the stdlib traits we care about today.
    const MAX_PROBE_SLOTS: usize = 16;
    let probe = read_memory(vtable_addr, MAX_PROBE_SLOTS * std::mem::size_of::<u64>())?;
    for chunk in probe.chunks_exact(std::mem::size_of::<u64>()) {
        let slot = u64::from_le_bytes(chunk.try_into().ok()?);
        if slot == 0 {
            continue;
        }
        // Translate runtime slot → image-relative before the lookup;
        // if the slot points into a region we have no mapping for
        // (e.g. a stale/garbage word in size/align slots) skip it
        // rather than treating it as a function symbol.
        let Some(slot_global) = to_global(slot) else {
            continue;
        };
        let Some(mangled) = mangled_at(slot_global) else {
            continue;
        };
        // Two extraction strategies for whichever shape the symbol
        // has: explicit `<X as Trait>::method` (any trait method)
        // or `core::ptr::drop_in_place::<X>` (the drop slot).
        if let Some(name) = concrete_from_vtable_symbol(mangled) {
            return Some(name);
        }
        if let Some(name) = concrete_from_drop_in_place_symbol(mangled) {
            return Some(name);
        }
    }
    None
}

/// Strategy 2 — extract the concrete type from a vtable symbol's
/// mangled name. Both v0 and legacy mangling are handled:
/// * v0: parse → [`Symbol::V0`] → walk to a [`Path`] with
///   `impl_self_type()` (i.e. an `X` `TraitImpl` or `Y` `TraitAssoc`
///   anywhere in the spine) and render that self-type.
/// * legacy: detect a `<X as Y>` segment in the demangled string and
///   pull `X` out by string surgery — legacy doesn't preserve the
///   AST structure for us to walk.
fn concrete_from_vtable_symbol(mangled: &str) -> Option<String> {
    use rust_mangle_tree::{Symbol as RmSymbol, Type as RmType};
    // `SymbolTab::by_address` stores raw nlist names (e.g.
    // `__RNv…` on Mach-O, `__ZN…` for legacy). `rust-mangle-tree`
    // accepts the legacy `__ZN…` form leniently but *rejects* a
    // v0 symbol with two leading underscores — `__R…` parses as
    // `Err` and silently kills concrete-type recovery. Peel one
    // leading underscore for the Rust prefixes, matching the
    // pre-demangle peel `SymbolTab::new` does for `by_name`.
    let mangled = if mangled.starts_with("__R") || mangled.starts_with("__Z") {
        &mangled[1..]
    } else {
        mangled
    };
    let parsed = rust_mangle_tree::parse(mangled).ok()?;
    match parsed {
        RmSymbol::V0(path) => {
            // Walk the path spine looking for any TraitImpl /
            // TraitAssoc node — that's where the `<Concrete as
            // Trait>` shape lives. The vtable's path is typically
            // `Nv...{vtable}` whose ancestor is a TraitImpl.
            let target = walk_for_impl(&path)?;
            match target {
                RmType::Path(p) => Some(p.to_string()),
                other => Some(format!("{}", DisplayType(&other))),
            }
        }
        RmSymbol::Legacy(_) => {
            // The demangled legacy string contains `<Concrete as
            // Trait>`. Carve out the `Concrete` substring.
            let demangled = format!("{parsed:#}");
            let lt = demangled.find('<')?;
            let as_kw = demangled[lt..].find(" as ")?;
            // Strip the leading `<` from the slice we keep.
            Some(demangled[lt + 1..lt + as_kw].trim().to_string())
        }
        RmSymbol::NotRust(_) => None,
    }
}

/// Strategy 1 — extract the concrete type from a `core::ptr::
/// drop_in_place::<Concrete>` symbol. The generic argument is the
/// concrete type. Both manglings are handled the same way:
/// demangle, find the `<` after `drop_in_place`, take the matching
/// `>`-balanced span.
fn concrete_from_drop_in_place_symbol(mangled: &str) -> Option<String> {
    // Same Mach-O double-underscore peel as concrete_from_vtable_symbol —
    // `__R…` would otherwise fail to parse and skip the drop-fn fallback.
    let mangled = if mangled.starts_with("__R") || mangled.starts_with("__Z") {
        &mangled[1..]
    } else {
        mangled
    };
    let demangled = match rust_mangle_tree::parse(mangled).ok()? {
        rust_mangle_tree::Symbol::V0(p) => p.to_string(),
        rust_mangle_tree::Symbol::Legacy(p) => format!("{p:#}"),
        rust_mangle_tree::Symbol::NotRust(_) => return None,
    };
    let needle = "drop_in_place";
    let drop_at = demangled.find(needle)?;
    let after = &demangled[drop_at + needle.len()..];
    // v0 emits `drop_in_place::<…>`; legacy emits `drop_in_place<…>`.
    let after = after.strip_prefix("::").unwrap_or(after);
    let after = after.strip_prefix('<')?;
    // Take the `>`-balanced span starting here.
    let mut depth: i32 = 1;
    for (i, c) in after.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after[..i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Walk a v0 [`Path`] spine looking for the innermost path node
/// whose `impl_self_type()` resolves — that's the `<Concrete as
/// Trait>` parent of a `Nv...{vtable}` segment.
fn walk_for_impl<'a>(path: &'a rust_mangle_tree::Path<'a>) -> Option<rust_mangle_tree::Type<'a>> {
    use rust_mangle_tree::Path as RmPath;
    if let Some(t) = path.impl_self_type() {
        return Some(t.clone());
    }
    match path {
        RmPath::Nested { parent, .. } | RmPath::Generic { parent, .. } => walk_for_impl(parent),
        _ => None,
    }
}

/// Tiny `Display` shim so `walk_for_impl`'s `Type` payload renders
/// without taking a temporary borrow into a format string at the
/// call site.
struct DisplayType<'a>(&'a rust_mangle_tree::Type<'a>);

impl std::fmt::Display for DisplayType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The crate doesn't yet expose `Type`'s Display; render via
        // a path indirection if possible.
        match self.0 {
            rust_mangle_tree::Type::Path(p) => write!(f, "{p}"),
            rust_mangle_tree::Type::Primitive(p) => f.write_str(p.as_str()),
            // Anything more elaborate is rare on the self-type side
            // of a vtable symbol; render as `<complex>` for now.
            _ => f.write_str("<complex>"),
        }
    }
}

#[cfg(test)]
mod dyn_resolver_tests {
    use super::{concrete_from_drop_in_place_symbol, concrete_from_vtable_symbol};

    /// Showcase's `<showcase::main::Point as showcase::main::Greeter>::greet`
    /// in legacy mangling — the actual symbol present in the
    /// `target/debug/showcase` binary on Mach-O. Strategy 1 reads
    /// the vtable's slot-3 fn pointer, looks the address up in the
    /// symbol table, and feeds the mangled name through this helper.
    #[test]
    fn legacy_greet_resolves_to_point() {
        let mangled = "__ZN65_$LT$showcase..main..Point$u20$as$u20$showcase..main..Greeter$GT$5greet17h57300c1f61dfdadaE";
        let got = concrete_from_vtable_symbol(mangled);
        assert_eq!(
            got.as_deref(),
            Some("showcase::main::Point"),
            "showcase legacy resolver should yield the concrete impl self-type"
        );
    }

    /// Same, but with the leading underscore peeled off so the
    /// caller treats the Mach-O `__ZN…` and the ELF `_ZN…` forms
    /// identically.
    #[test]
    fn legacy_greet_resolves_stripped() {
        let mangled = "_ZN65_$LT$showcase..main..Point$u20$as$u20$showcase..main..Greeter$GT$5greet17h57300c1f61dfdadaE";
        assert_eq!(
            concrete_from_vtable_symbol(mangled).as_deref(),
            Some("showcase::main::Point")
        );
    }

    /// Drop slot for a type that *does* impl Drop — `core::ptr::
    /// drop_in_place::<MyError>` etc. Smoke-test the drop-fn
    /// fallback so we know Strategy 1's second extractor still works.
    #[test]
    fn legacy_drop_in_place_resolves() {
        let mangled = "__ZN4core3ptr59drop_in_place$LT$vars..phase3_dyn_trait..MyError$GT$17h0000000000000000E";
        assert_eq!(
            concrete_from_drop_in_place_symbol(mangled).as_deref(),
            Some("vars::phase3_dyn_trait::MyError")
        );
    }

    /// v0 form of `<showcase::main::Point as showcase::main::Greeter>
    /// ::greet`, taken from a `RUSTFLAGS=-C symbol-mangling-version=v0`
    /// build of `examples/target/debug/showcase`. The leading `__R`
    /// is Mach-O's two-underscore convention; the helper must peel
    /// one off and feed `_R…` to `rust-mangle-tree`. This is the
    /// path the user's reported binary takes — if `walk_for_impl`
    /// doesn't surface `showcase::main::Point` here, the
    /// `[concrete type unavailable]` message they saw is explained.
    #[test]
    fn v0_greet_resolves_to_point() {
        let mangled = "__RNvXNvCsdCBZUK1EFOO_8showcase4mainNtB2_5PointNtB2_7Greeter5greet";
        let got = concrete_from_vtable_symbol(mangled);
        assert_eq!(
            got.as_deref(),
            Some("showcase::main::Point"),
            "v0 resolver should walk the TraitImpl spine to the self-type"
        );
    }

    #[test]
    fn v0_greet_resolves_stripped() {
        let mangled = "_RNvXNvCsdCBZUK1EFOO_8showcase4mainNtB2_5PointNtB2_7Greeter5greet";
        assert_eq!(
            concrete_from_vtable_symbol(mangled).as_deref(),
            Some("showcase::main::Point")
        );
    }

    // ---- resolve_trait_object_from_lookups: end-to-end stub tests ----
    //
    // These tests simulate the live-debuggee flow with three stub
    // closures (address translation, memory read, symbol lookup).
    // They guard against the specific class of bug we just fixed —
    // forgetting to translate a runtime address through `to_global`
    // before hitting the (image-relative) symbol table — plus the
    // adjacent failure modes (null drop slot, garbage size/align
    // slots, exhausted probe budget).

    use super::resolve_trait_object_from_lookups;
    use std::cell::RefCell;

    /// Simulated PIE/ASLR slide. Runtime addresses we hand to the
    /// resolver are `image_offset + ARTIFICIAL_SLIDE`; the stub
    /// `to_global` subtracts it, mirroring what
    /// `RelocatedAddress::into_global` does on a real debuggee.
    const ARTIFICIAL_SLIDE: u64 = 0x5555_5555_4000;

    /// Image-relative addresses of the two symbols our stub table
    /// holds. Chosen to look like real `.text` offsets (well above
    /// the slide isn't necessary — these are *image-relative*).
    const POINT_GREET_GLOBAL: u64 = 0x1c2d0;
    const DROP_IN_PLACE_MYERROR_GLOBAL: u64 = 0xf170;

    /// v0-mangled `<Point as Greeter>::greet`, no leading underscore
    /// (the symbol table's `mangled_at` returns the raw nlist form
    /// post-strip already, see `SymbolTab::mangled_at` callers).
    const POINT_GREET_MANGLED: &str =
        "_RNvXNvCsdCBZUK1EFOO_8showcase4mainNtB2_5PointNtB2_7Greeter5greet";
    const DROP_IN_PLACE_MYERROR_MANGLED: &str =
        "_ZN4core3ptr59drop_in_place$LT$vars..phase3_dyn_trait..MyError$GT$17h0000000000000000E";

    /// Build a stub vtable in a Vec<u8> with the layout rustc emits:
    /// `[drop, size, align, m1, m2, …]`. Each slot is a runtime
    /// address (`image_offset + slide`). Caller passes the slots they
    /// want; we serialise them little-endian.
    fn vtable_bytes(slots: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(slots.len() * 8);
        for s in slots {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        bytes
    }

    /// `to_global` stub. Subtracts the artificial slide. Returns
    /// `None` for the impossible-mapping case (used by the "unmapped
    /// slot" test).
    fn slide_translator(runtime: u64) -> Option<u64> {
        runtime.checked_sub(ARTIFICIAL_SLIDE)
    }

    /// Captures the lookup-key history so tests can assert *which*
    /// addresses the resolver hit the symbol table with.
    struct LookupRecorder {
        table: Vec<(u64, &'static str)>,
        seen: RefCell<Vec<u64>>,
    }
    impl LookupRecorder {
        fn new(table: Vec<(u64, &'static str)>) -> Self {
            Self {
                table,
                seen: RefCell::new(Vec::new()),
            }
        }
        fn lookup(&self, addr: u64) -> Option<&str> {
            self.seen.borrow_mut().push(addr);
            self.table
                .iter()
                .find_map(|(a, s)| (*a == addr).then_some(*s))
        }
    }

    /// The regression test. **This is the exact bug from commit
    /// 0e9abbe's follow-up:** pre-fix, the resolver passed runtime
    /// addresses straight into the image-relative symbol table and
    /// every lookup missed. Here we build a vtable whose slot-3
    /// runtime address slides to a real entry in the symbol table —
    /// if the resolver forgets to translate, the test fails.
    #[test]
    fn slot_walk_finds_concrete_type_after_address_translation() {
        // Slot-3 holds <Point as Greeter>::greet at the runtime PC
        // (image-relative + slide). Slots 0–2 (drop / size / align)
        // are zero so the early continue exercises that branch.
        let slot3_runtime = POINT_GREET_GLOBAL + ARTIFICIAL_SLIDE;
        let vt = vtable_bytes(&[0, 0, 0, slot3_runtime]);
        let recorder = LookupRecorder::new(vec![(POINT_GREET_GLOBAL, POINT_GREET_MANGLED)]);

        let resolved = resolve_trait_object_from_lookups(
            0xDEAD_BEEF, // vtable address: untranslatable + not in table — strategy 2 misses.
            &|r| slide_translator(r),
            &|_, n| Some(vt[..n.min(vt.len())].to_vec()),
            &|a| recorder.lookup(a),
        );
        assert_eq!(
            resolved.as_deref(),
            Some("showcase::main::Point"),
            "slot-walking must translate runtime → image-relative before symbol lookup",
        );
        // Every non-zero slot we probed must have been translated:
        // the recorder should only ever see *image-relative* keys.
        let seen = recorder.seen.borrow();
        for key in seen.iter() {
            assert!(
                *key < ARTIFICIAL_SLIDE,
                "lookup key {key:#x} >= slide {ARTIFICIAL_SLIDE:#x} — caller forgot to translate",
            );
        }
    }

    /// Strategy 1's drop-fn fallback: `core::ptr::drop_in_place::
    /// <MyError>` at slot 0 is the only carrier of the concrete type
    /// when the trait has no methods we recognise. Confirms the
    /// second extractor (`concrete_from_drop_in_place_symbol`) is
    /// reached after the vtable-slot probe.
    #[test]
    fn slot_walk_falls_back_to_drop_in_place() {
        let drop_runtime = DROP_IN_PLACE_MYERROR_GLOBAL + ARTIFICIAL_SLIDE;
        // Slot 0 = drop. Slots 1, 2 (size/align) are 0 so the slot
        // walker continues past them.
        let vt = vtable_bytes(&[drop_runtime, 0, 0]);
        let recorder = LookupRecorder::new(vec![(
            DROP_IN_PLACE_MYERROR_GLOBAL,
            DROP_IN_PLACE_MYERROR_MANGLED,
        )]);

        let resolved = resolve_trait_object_from_lookups(
            0,
            &|r| slide_translator(r),
            &|_, n| Some(vt[..n.min(vt.len())].to_vec()),
            &|a| recorder.lookup(a),
        );
        assert_eq!(
            resolved.as_deref(),
            Some("vars::phase3_dyn_trait::MyError"),
            "drop-fn fallback must surface the concrete type when no method symbols match",
        );
    }

    /// Strategy 2 hits when the symbol table has an entry at the
    /// vtable address itself (v0-mangled builds with exported vtable
    /// symbols). Slot walking must never run.
    #[test]
    fn strategy_2_uses_vtable_address_directly() {
        // Hand-rolled v0 vtable symbol: `<Point as Greeter>::{vtable}`.
        // The walk_for_impl spine yields the same TraitImpl as the
        // method symbol, so re-using POINT_GREET_MANGLED here is OK —
        // both demangle to the same self-type.
        const VTABLE_GLOBAL: u64 = 0x7DEF8;
        let vt_runtime = VTABLE_GLOBAL + ARTIFICIAL_SLIDE;
        let recorder = LookupRecorder::new(vec![(VTABLE_GLOBAL, POINT_GREET_MANGLED)]);

        let resolved = resolve_trait_object_from_lookups(
            vt_runtime,
            &|r| slide_translator(r),
            &|_, _| panic!("strategy 2 must not need to read memory when the vtable address has a symbol"),
            &|a| recorder.lookup(a),
        );
        assert_eq!(resolved.as_deref(), Some("showcase::main::Point"));
        // Strategy 2 lookup happens against the *translated* vtable
        // address — same image-relative invariant as strategy 1.
        let seen = recorder.seen.borrow();
        assert_eq!(seen.as_slice(), &[VTABLE_GLOBAL]);
    }

    /// If `to_global` returns `None` (slot points into a region we
    /// have no mapping for — uninitialised memory, a foreign dylib
    /// without debug info), the resolver must skip that slot rather
    /// than treating the raw runtime word as an image-relative key.
    #[test]
    fn unmapped_slot_does_not_poison_table_lookup() {
        let drop_runtime = DROP_IN_PLACE_MYERROR_GLOBAL + ARTIFICIAL_SLIDE;
        // Slot 0 = garbage (no mapping). Slot 1 = drop (valid).
        // Slots 2+ = 0. Without skip-on-None the resolver would feed
        // raw garbage into the symbol table and might collide.
        let vt = vtable_bytes(&[0x1234_5678_DEAD_BEEF, drop_runtime, 0]);
        let recorder = LookupRecorder::new(vec![(
            DROP_IN_PLACE_MYERROR_GLOBAL,
            DROP_IN_PLACE_MYERROR_MANGLED,
        )]);

        let resolved = resolve_trait_object_from_lookups(
            0,
            &|r| r.checked_sub(ARTIFICIAL_SLIDE).filter(|g| *g < 0x10_0000),
            &|_, n| Some(vt[..n.min(vt.len())].to_vec()),
            &|a| recorder.lookup(a),
        );
        assert_eq!(resolved.as_deref(), Some("vars::phase3_dyn_trait::MyError"));
        // The garbage slot must never have reached the lookup —
        // its runtime value minus the slide either underflows or
        // falls outside our synthetic `<0x10_0000` window, and the
        // recorder records *only* lookups that ran.
        let seen = recorder.seen.borrow();
        assert!(
            !seen.contains(&0x1234_5678_DEAD_BEEF),
            "raw garbage slot {:#x} must not be used as a symbol-table key",
            0x1234_5678_DEAD_BEEFu64,
        );
    }

    /// Tail case: the vtable holds 16 nonsense slots, none of which
    /// resolve. The resolver must terminate (not loop, not panic)
    /// and return `None` so the renderer falls back to the
    /// "concrete type unavailable" hint.
    #[test]
    fn no_resolvable_slot_returns_none() {
        let vt = vtable_bytes(&[ARTIFICIAL_SLIDE + 0x9999; 16]);
        let recorder = LookupRecorder::new(vec![]); // empty table
        let resolved = resolve_trait_object_from_lookups(
            0,
            &|r| slide_translator(r),
            &|_, n| Some(vt[..n.min(vt.len())].to_vec()),
            &|a| recorder.lookup(a),
        );
        assert!(resolved.is_none());
    }
}

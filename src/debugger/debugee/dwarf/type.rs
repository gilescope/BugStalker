// SPDX-License-Identifier: MIT
use crate::debugger::debugee::dwarf::eval::{AddressKind, EvaluationContext};
use crate::debugger::debugee::dwarf::unit::DieAddr;
use crate::debugger::debugee::dwarf::unit::die::Die;
use crate::debugger::debugee::dwarf::unit::die_ref::Hint;
use crate::debugger::debugee::dwarf::{EndianArcSlice, FatDieRef, NamespaceHierarchy, eval};
use crate::debugger::error::Error;
use crate::debugger::variable::ObjectBinaryRepr;
use crate::weak_error;
use bytes::Bytes;
use gimli::{AttributeValue, DwAte, Expression};
use indexmap::IndexMap;
use log::warn;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::{Display, Formatter};
use std::mem;
use std::rc::Rc;
use strum_macros::Display;
use uuid::Uuid;

/// Type identifier.
pub type TypeId = DieAddr;

/// Type name with namespace.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct TypeIdentity {
    namespace: NamespaceHierarchy,
    name: Option<String>,
}

impl TypeIdentity {
    /// Create type identity with empty namespace.
    #[inline(always)]
    pub fn no_namespace(name: impl ToString) -> Self {
        Self {
            namespace: Default::default(),
            name: Some(name.to_string()),
        }
    }

    /// Create type identity for an unknown type.
    #[inline(always)]
    pub fn unknown() -> Self {
        Self {
            namespace: Default::default(),
            name: None,
        }
    }

    /// True whether a type is unknown.
    #[inline(always)]
    pub fn is_unknown(&self) -> bool {
        self.name.is_none()
    }

    #[inline(always)]
    pub fn namespace(&self) -> &NamespaceHierarchy {
        &self.namespace
    }

    #[inline(always)]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Return formatted type name.
    #[inline(always)]
    pub fn name_fmt(&self) -> &str {
        self.name().unwrap_or("unknown")
    }

    /// Replace the name in place — used by Phase 3 Feature A's
    /// trait-object resolver to splice the recovered concrete type
    /// into the displayed identity.
    #[inline(always)]
    pub fn set_name(&mut self, name: String) {
        self.name = Some(name);
    }

    /// Create address type name.
    #[inline(always)]
    pub fn as_address_type(&self) -> TypeIdentity {
        TypeIdentity {
            namespace: self.namespace.clone(),
            name: Some(format!("&{}", self.name_fmt())),
        }
    }

    /// Create dereferenced type name.
    #[inline(always)]
    pub fn as_deref_type(&self) -> TypeIdentity {
        TypeIdentity {
            namespace: self.namespace.clone(),
            name: Some(format!("*{}", self.name_fmt())),
        }
    }

    /// Create array type name from element type name.
    #[inline(always)]
    pub fn as_array_type(&self) -> TypeIdentity {
        TypeIdentity::no_namespace(format!("[{}]", self.name_fmt()))
    }
}

impl Display for TypeIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // TODO this may be a bad idea
        if self.namespace.is_empty() {
            return f.write_str(self.name_fmt());
        }

        f.write_fmt(format_args!(
            "{}::{}",
            self.namespace.as_parts().join("::"),
            self.name_fmt()
        ))
    }
}

#[derive(Clone, Debug)]
pub struct MemberLocationExpression {
    expr: Expression<EndianArcSlice>,
}

impl MemberLocationExpression {
    fn base_addr(&self, evcx: &EvaluationContext, entity_addr: usize) -> Result<usize, Error> {
        evcx.evaluator
            .evaluate_with_resolver(
                eval::ExternalRequirementsResolver::new()
                    .with_at_location(entity_addr.to_ne_bytes()),
                evcx.ecx,
                self.expr.clone(),
            )?
            .into_scalar::<usize>(AddressKind::Value)
    }
}

#[derive(Clone, Debug)]
pub enum MemberLocation {
    Offset(i64),
    Expr(MemberLocationExpression),
}

#[derive(Clone, Debug)]
pub struct StructureMember {
    pub in_struct_location: Option<MemberLocation>,
    pub name: Option<String>,
    pub type_ref: Option<TypeId>,
    /// Phase 3 Feature D — `(file_index, line)` pair from the member's
    /// `DW_AT_decl_file`/`DW_AT_decl_line` attributes, when present.
    /// rustc emits these on the member fields of a coroutine state-
    /// machine enum's variants, where they encode the source location
    /// of the corresponding `.await` point. The renderer uses them to
    /// drive Phase 3 D's await-trace.
    pub decl_file_line: Option<(u64, u64)>,
}

impl StructureMember {
    /// Return binary representation of structure member data.
    ///
    /// # Arguments
    ///
    /// * `evcx`: evaluation context
    /// * `r#type`: member data type
    /// * `base_data`: binary representation of the parent structure
    pub fn value(
        &self,
        evcx: &EvaluationContext,
        r#type: &ComplexType,
        base_data: &ObjectBinaryRepr,
    ) -> Option<ObjectBinaryRepr> {
        let type_size = r#type.type_size_in_bytes(evcx, self.type_ref?)? as usize;

        let base_entity_addr = base_data.raw_data.as_ptr() as usize;
        let addr = match self.in_struct_location.as_ref()? {
            MemberLocation::Offset(offset) => {
                Some((base_entity_addr as isize + (*offset as isize)) as usize)
            }
            MemberLocation::Expr(expr) => {
                weak_error!(expr.base_addr(evcx, base_entity_addr))
            }
        }? as *const u8;

        let offset = addr as isize - base_entity_addr as isize;
        let new_in_debugee_addr = base_data
            .address
            .map(|addr| (addr as isize + offset) as usize);

        let raw_data = Bytes::from(unsafe { std::slice::from_raw_parts(addr, type_size) });

        Some(ObjectBinaryRepr {
            raw_data,
            address: new_in_debugee_addr,
            size: type_size,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ArrayBoundValueExpression {
    expr: Expression<EndianArcSlice>,
}

impl ArrayBoundValueExpression {
    fn bound(&self, evcx: &EvaluationContext) -> Result<i64, Error> {
        evcx.evaluator
            .evaluate(evcx.ecx, self.expr.clone())?
            .into_scalar::<i64>(AddressKind::MemoryAddress)
    }
}

#[derive(Clone, Debug)]
pub enum ArrayBoundValue {
    Const(i64),
    Expr(ArrayBoundValueExpression),
}

impl ArrayBoundValue {
    pub fn value(&self, evcx: &EvaluationContext) -> Result<i64, Error> {
        match self {
            ArrayBoundValue::Const(v) => Ok(*v),
            ArrayBoundValue::Expr(e) => e.bound(evcx),
        }
    }
}

#[derive(Clone, Debug)]
pub enum UpperBound {
    UpperBound(ArrayBoundValue),
    Count(ArrayBoundValue),
}

#[derive(Clone, Debug)]
pub struct ArrayType {
    pub namespaces: NamespaceHierarchy,
    byte_size: Option<u64>,
    element_type: Option<TypeId>,
    lower_bound: ArrayBoundValue,
    upper_bound: Option<UpperBound>,
    byte_size_memo: Cell<Option<u64>>,
    bounds_memo: Cell<Option<(i64, i64)>>,
}

impl ArrayType {
    pub fn new(
        namespaces: NamespaceHierarchy,
        byte_size: Option<u64>,
        element_type: Option<TypeId>,
        lower_bound: ArrayBoundValue,
        upper_bound: Option<UpperBound>,
    ) -> Self {
        Self {
            namespaces,
            byte_size,
            element_type,
            lower_bound,
            upper_bound,
            byte_size_memo: Cell::new(None),
            bounds_memo: Cell::new(None),
        }
    }

    #[inline(always)]
    pub fn element_type(&self) -> Option<TypeId> {
        self.element_type
    }

    pub fn byte_size_hint(&self) -> Option<u64> {
        self.byte_size
    }

    fn lower_bound(&self, evcx: &EvaluationContext) -> i64 {
        self.lower_bound.value(evcx).unwrap_or(0)
    }

    pub fn bounds(&self, evcx: &EvaluationContext) -> Option<(i64, i64)> {
        if self.bounds_memo.get().is_none() {
            let lb = self.lower_bound(evcx);
            let bounds = match self.upper_bound.as_ref()? {
                UpperBound::UpperBound(ub) => (lb, ub.value(evcx).ok()? - lb),
                UpperBound::Count(c) => (lb, c.value(evcx).ok()?),
            };
            self.bounds_memo.set(Some(bounds));
        }
        self.bounds_memo.get()
    }

    pub fn size_in_bytes(&self, evcx: &EvaluationContext, type_graph: &ComplexType) -> Option<u64> {
        if self.byte_size.is_some() {
            return self.byte_size;
        }

        if self.byte_size_memo.get().is_none() {
            let bounds = self.bounds(evcx)?;
            let inner_type_size = type_graph.type_size_in_bytes(evcx, self.element_type?)?;
            self.byte_size_memo
                .set(Some(array_byte_size(bounds.0, bounds.1, inner_type_size)?));
        }

        self.byte_size_memo.get()
    }
}

/// Total byte size of an array subrange, given the resolved
/// `(lower, span_end)` from [`ArrayType::bounds`] and the element
/// size. `span_end - lower` is the element count.
///
/// Some DWARF producers (C/`-sys` debug info, older toolchains) emit
/// `DW_AT_upper_bound = -1` for a zero- or unknown-length array, which
/// [`ArrayType::bounds`] surfaces as a negative span. The old inline
/// `element_size * (span_end - lower) as u64` then read that `-1` as a
/// `u64::MAX` element count and overflowed, producing a near-`u64::MAX`
/// byte size that panicked `BytesMut::with_capacity` ("capacity
/// overflow") the moment such a global was read — taking down the whole
/// debug session. Here a negative count clamps to `0` and the multiply
/// is checked, so a malformed bound yields `Some(0)`/`None` and the
/// caller degrades the variable to unreadable instead of crashing.
fn array_byte_size(lower: i64, span_end: i64, element_size: u64) -> Option<u64> {
    // A negative element count means a malformed / zero-length bound
    // (e.g. DW_AT_upper_bound = -1) — treat as empty rather than
    // wrapping into a giant unsigned count.
    let count = u64::try_from(span_end.checked_sub(lower)?).unwrap_or(0);
    element_size.checked_mul(count)
}

#[cfg(test)]
mod array_byte_size_tests {
    use super::array_byte_size;

    #[test]
    fn normal_count() {
        // `[i32; 3]` → lower 0, span_end 3, element 4 bytes.
        assert_eq!(array_byte_size(0, 3, 4), Some(12));
    }

    #[test]
    fn empty_array() {
        assert_eq!(array_byte_size(0, 0, 8), Some(0));
    }

    #[test]
    fn upper_bound_minus_one_clamps_to_zero() {
        // DW_AT_upper_bound = -1 → bounds() span_end = -1. Must NOT
        // wrap into a u64::MAX element count (the capacity-overflow
        // crash). Regression for the variables-view file-scope panic.
        assert_eq!(array_byte_size(0, -1, 8), Some(0));
    }

    #[test]
    fn element_count_times_size_overflow_is_none() {
        // A huge-but-positive span must saturate to None rather than
        // wrap, so callers degrade gracefully.
        assert_eq!(array_byte_size(0, i64::MAX, u64::MAX), None);
    }
}

#[derive(Clone, Debug)]
pub struct ScalarType {
    pub namespaces: NamespaceHierarchy,
    pub name: Option<String>,
    pub byte_size: Option<u64>,
    pub encoding: Option<DwAte>,
}

impl ScalarType {
    pub fn identity(&self) -> TypeIdentity {
        TypeIdentity {
            namespace: self.namespaces.clone(),
            name: self.name.clone(),
        }
    }
}

/// List of type modifiers
#[derive(Display, Clone, Copy, PartialEq, Debug)]
#[strum(serialize_all = "snake_case")]
pub enum CModifier {
    TypeDef,
    Const,
    Volatile,
    Atomic,
    Restrict,
}

#[derive(Clone, Debug)]
pub enum TypeDeclaration {
    Scalar(ScalarType),
    Array(ArrayType),
    CStyleEnum {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        byte_size: Option<u64>,
        discr_type: Option<TypeId>,
        enumerators: HashMap<i64, String>,
    },
    Pointer {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        target_type: Option<TypeId>,
    },
    Structure {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        byte_size: Option<u64>,
        members: Vec<StructureMember>,
        type_params: IndexMap<String, Option<TypeId>>,
        /// Phase 3 Feature A — true when this struct is the
        /// fat-pointer representation of a `dyn Trait`. Detected
        /// by name pattern (`<… dyn …>`) plus the canonical
        /// `pointer`/`vtable` member shape rustc emits. Used by
        /// the renderer to annotate the value and (eventually)
        /// drive vtable → concrete-type resolution.
        is_trait_object: bool,
    },
    Union {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        byte_size: Option<u64>,
        members: Vec<StructureMember>,
    },
    RustEnum {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        byte_size: Option<u64>,
        discr_type: Option<Box<StructureMember>>,
        /// key `None` is default enumerator
        enumerators: HashMap<Option<i64>, StructureMember>,
    },
    Subroutine {
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        return_type: Option<TypeId>,
    },
    ModifiedType {
        modifier: CModifier,
        namespaces: NamespaceHierarchy,
        name: Option<String>,
        inner: Option<TypeId>,
    },
}

/// Type representation. This is a graph of types where vertexes is a type declaration and edges
/// is a dependencies between types. Type linking implemented by `TypeId` references.
/// Root is id of a main type.
#[derive(Clone, Debug)]
pub struct ComplexType {
    pub types: HashMap<TypeId, TypeDeclaration>,
    root: TypeId,
}

impl ComplexType {
    /// Return root type id.
    #[inline(always)]
    pub fn root(&self) -> TypeId {
        self.root
    }

    /// Return name of some of a type existed in a complex type.
    pub fn identity(&self, typ: TypeId) -> TypeIdentity {
        let Some(r#type) = self.types.get(&typ) else {
            return TypeIdentity::unknown();
        };

        match r#type {
            TypeDeclaration::Scalar(s) => s.identity(),
            TypeDeclaration::Structure {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::Array(arr) => {
                let el_ident = arr.element_type.map(|t| self.identity(t));
                let name = format!(
                    "[{}]",
                    el_ident
                        .as_ref()
                        .map(|ident| ident.name_fmt())
                        .unwrap_or("unknown")
                );

                TypeIdentity {
                    namespace: arr.namespaces.clone(),
                    name: Some(name.to_string()),
                }
            }
            TypeDeclaration::CStyleEnum {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::RustEnum {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::Pointer {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::Union {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::Subroutine {
                name, namespaces, ..
            } => TypeIdentity {
                namespace: namespaces.clone(),
                name: name.as_ref().cloned(),
            },
            TypeDeclaration::ModifiedType {
                modifier,
                namespaces,
                name,
                inner,
                ..
            } => match name {
                None => {
                    let name = inner.map(|inner_id| {
                        let ident = self.identity(inner_id);
                        format!("{modifier} {name}", name = ident.name_fmt())
                    });

                    TypeIdentity {
                        namespace: namespaces.clone(),
                        name,
                    }
                }
                Some(n) => TypeIdentity {
                    namespace: namespaces.clone(),
                    name: Some(n.to_string()),
                },
            },
        }
    }

    /// Return size of a type existed from a complex type.
    pub fn type_size_in_bytes(&self, evcx: &EvaluationContext, typ: TypeId) -> Option<u64> {
        match &self.types.get(&typ)? {
            TypeDeclaration::Scalar(s) => s.byte_size,
            TypeDeclaration::Structure { byte_size, .. } => *byte_size,
            TypeDeclaration::Array(arr) => arr.size_in_bytes(evcx, self),
            TypeDeclaration::CStyleEnum { byte_size, .. } => *byte_size,
            TypeDeclaration::RustEnum { byte_size, .. } => *byte_size,
            TypeDeclaration::Pointer { .. } => Some(mem::size_of::<usize>() as u64),
            TypeDeclaration::Union { byte_size, .. } => *byte_size,
            TypeDeclaration::Subroutine { .. } => Some(mem::size_of::<usize>() as u64),
            TypeDeclaration::ModifiedType { inner, .. } => {
                inner.and_then(|inner_id| self.type_size_in_bytes(evcx, inner_id))
            }
        }
    }

    /// Visit type children in bfs order, `start_at` - identity of root type.
    pub fn bfs_iterator(&self, start_at: TypeId) -> BfsIterator<'_> {
        BfsIterator {
            complex_type: self,
            queue: VecDeque::from([start_at]),
        }
    }
}

/// Bfs iterator over related types.
/// Note that this iterator may be infinite.
pub struct BfsIterator<'a> {
    complex_type: &'a ComplexType,
    queue: VecDeque<TypeId>,
}

impl<'a> Iterator for BfsIterator<'a> {
    type Item = &'a TypeDeclaration;

    fn next(&mut self) -> Option<Self::Item> {
        let el_id = self.queue.pop_back()?;
        let type_decl = &self.complex_type.types[&el_id];
        match type_decl {
            TypeDeclaration::Scalar(_) => {}
            TypeDeclaration::Array(arr) => {
                if let Some(el) = arr.element_type {
                    self.queue.push_front(el);
                }
            }
            TypeDeclaration::CStyleEnum { discr_type, .. } => {
                if let Some(el) = discr_type {
                    self.queue.push_front(*el);
                }
            }
            TypeDeclaration::Pointer { target_type, .. } => {
                if let Some(el) = target_type {
                    self.queue.push_front(*el);
                }
            }
            TypeDeclaration::Structure {
                members,
                type_params,
                ..
            } => {
                members.iter().for_each(|m| {
                    if let Some(el) = m.type_ref {
                        self.queue.push_front(el);
                    }
                });
                type_params.values().for_each(|t| {
                    if let Some(el) = t {
                        self.queue.push_front(*el);
                    }
                });
            }
            TypeDeclaration::Union { members, .. } => {
                members.iter().for_each(|m| {
                    if let Some(el) = m.type_ref {
                        self.queue.push_front(el);
                    }
                });
            }
            TypeDeclaration::RustEnum {
                discr_type,
                enumerators,
                ..
            } => {
                if let Some(el) = discr_type.as_ref().and_then(|member| member.type_ref) {
                    self.queue.push_front(el);
                }
                enumerators.values().for_each(|member| {
                    if let Some(el) = member.type_ref {
                        self.queue.push_front(el);
                    }
                });
            }
            TypeDeclaration::Subroutine { .. } => {}
            TypeDeclaration::ModifiedType { inner, .. } => {
                if let Some(type_id) = inner {
                    self.queue.push_front(*type_id);
                }
            }
        }

        Some(type_decl)
    }
}

/// DWARF DIE parser.
pub struct TypeParser {
    known_type_ids: HashSet<TypeId>,
    processed_types: HashMap<TypeId, TypeDeclaration>,
}

impl TypeParser {
    /// Creates new type parser.
    pub fn new() -> Self {
        Self {
            known_type_ids: HashSet::new(),
            processed_types: HashMap::new(),
        }
    }

    /// Parse a `ComplexType` from a DIEs.
    pub fn parse<'dbg, H: Hint>(self, die_ref: FatDieRef<'dbg, H>, root_id: TypeId) -> ComplexType {
        let mut this = self;
        this.parse_inner(die_ref, root_id);
        ComplexType {
            types: this.processed_types,
            root: root_id,
        }
    }

    fn parse_inner<H: Hint>(&mut self, die_fref: FatDieRef<'_, H>, type_addr: DieAddr) {
        // guard from recursion types parsing
        if self.known_type_ids.contains(&type_addr) {
            return;
        }
        self.known_type_ids.insert(type_addr);

        let mb_unit = match type_addr {
            DieAddr::Unit(_) => Some(die_fref.unit()),
            DieAddr::Global(debug_info_offset) => die_fref.debug_info.find_unit(debug_info_offset),
        };

        let mb_type_die_offset = mb_unit.map(|unit| {
            let offset = type_addr.unit_offset(unit);
            let fref = FatDieRef::new_no_hint(die_fref.debug_info, unit.idx(), offset);
            let tag = fref.deref_ensure().tag();
            (unit, offset, tag)
        });

        let type_decl = mb_type_die_offset.and_then(|(unit, offset, tag)| {
            let die_ref = FatDieRef::new_no_hint(die_fref.debug_info, unit.idx(), offset);

            match tag {
                gimli::DW_TAG_base_type => Some(self.parse_base_type(die_ref)),
                gimli::DW_TAG_structure_type => Some(self.parse_struct(die_ref)),
                gimli::DW_TAG_array_type => Some(self.parse_array(die_ref)),
                gimli::DW_TAG_enumeration_type => Some(self.parse_enum(die_ref)),
                gimli::DW_TAG_pointer_type => Some(self.parse_pointer(die_ref)),
                // F1 (Phase 1): reference and rvalue-reference type DIEs are
                // pointer-shaped. rustc currently emits `DW_TAG_pointer_type`
                // for `&T` / `&mut T`, but reference-type DIEs surface from
                // C++ debug info reachable via FFI, from non-default rustc
                // codegen flavours, and from custom DWARF producers.
                // Treating them as pointers keeps the parser silent and the
                // rendered type name (`&T` / `&mut T`) preserved as written
                // by the producer in `DW_AT_name`.
                gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => {
                    Some(self.parse_pointer(die_ref))
                }
                gimli::DW_TAG_union_type => Some(self.parse_union(die_ref)),
                gimli::DW_TAG_subrange_type => Some(self.parse_subroutine(die_ref)),
                gimli::DW_TAG_typedef => Some(self.parse_typedef(die_ref)),
                gimli::DW_TAG_const_type => Some(self.parse_const_type(die_ref)),
                gimli::DW_TAG_atomic_type => Some(self.parse_atomic(die_ref)),
                gimli::DW_TAG_volatile_type => Some(self.parse_volatile(die_ref)),
                gimli::DW_TAG_restrict_type => Some(self.parse_restrict(die_ref)),
                gimli::DW_TAG_subroutine_type => Some(self.parse_subroutine(die_ref)),
                _ => {
                    warn!("unsupported type die: {tag} at offset {offset:?}");
                    None
                }
            }
        });
        if let Some(type_decl) = type_decl {
            self.processed_types.insert(type_addr, type_decl);
        }
    }

    fn parse_base_type(&mut self, die_ref: FatDieRef<'_>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();
        TypeDeclaration::Scalar(ScalarType {
            namespaces: die_ref.namespace(),
            name,
            byte_size: die.byte_size(),
            encoding: die.encoding(),
        })
    }

    fn parse_array<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let mb_type_ref = die.type_ref();
        if let Some(reference) = mb_type_ref {
            self.parse_inner(die_ref, reference);
        }

        let subrange = die.for_each_children_t(|child| {
            (child.tag() == gimli::DW_TAG_subrange_type).then_some(child)
        });

        let lower_bound = subrange
            .as_ref()
            .map(|sr| {
                let lower_bound = sr.lower_bound().as_ref().map(|lb| lb.value());

                if let Some(bound) = lower_bound.as_ref().and_then(|l| l.sdata_value()) {
                    ArrayBoundValue::Const(bound)
                } else if let Some(AttributeValue::Exprloc(ref expr)) = lower_bound {
                    ArrayBoundValue::Expr(ArrayBoundValueExpression { expr: expr.clone() })
                } else {
                    // rust default lower bound
                    ArrayBoundValue::Const(0)
                }
            })
            .unwrap_or(ArrayBoundValue::Const(0));

        let upper_bound = subrange.and_then(|sr| {
            if let Some(ref count) = sr.count() {
                return if let Some(cnt) = count.value().sdata_value() {
                    Some(UpperBound::Count(ArrayBoundValue::Const(cnt)))
                } else if let AttributeValue::Exprloc(ref expr) = count.value() {
                    Some(UpperBound::Count(ArrayBoundValue::Expr(
                        ArrayBoundValueExpression { expr: expr.clone() },
                    )))
                } else {
                    None
                };
            }

            if let Some(ref bound) = sr.upper_bound() {
                if let Some(bound) = bound.value().sdata_value() {
                    return Some(UpperBound::UpperBound(ArrayBoundValue::Const(bound)));
                } else if let AttributeValue::Exprloc(ref expr) = bound.value() {
                    return Some(UpperBound::UpperBound(ArrayBoundValue::Expr(
                        ArrayBoundValueExpression { expr: expr.clone() },
                    )));
                };
            }

            None
        });

        TypeDeclaration::Array(ArrayType::new(
            die_ref.namespace(),
            die.byte_size(),
            mb_type_ref,
            lower_bound,
            upper_bound,
        ))
    }

    /// Convert DW_TAG_structure_type into TypeDeclaration.
    /// In rust DW_TAG_structure_type DIE can be interpreter as enum, see https://github.com/rust-lang/rust/issues/32920
    fn parse_struct<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();

        let is_enum = die
            .for_each_children_t(|child| {
                (child.tag() == gimli::DW_TAG_variant_part).then_some(child)
            })
            .is_some();

        if is_enum {
            self.parse_struct_enum(die_ref)
        } else {
            self.parse_struct_struct(die_ref)
        }
    }

    fn parse_struct_struct(&mut self, die_ref: FatDieRef<'_>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();

        let members = die.for_each_children_filter_collect(|child| {
            if child.tag() == gimli::DW_TAG_member {
                Some(self.parse_member(FatDieRef::new_no_hint(
                    die_ref.debug_info,
                    die_ref.unit().idx(),
                    child.offset(),
                )))
            } else {
                None
            }
        });

        let type_params = die
            .for_each_children_filter_collect(|child| {
                if child.tag() == gimli::DW_TAG_template_type_parameter {
                    let name = child.name()?;
                    let type_ref = child.type_ref();
                    self.parse_inner(die_ref, type_ref?);
                    Some((name, type_ref))
                } else {
                    None
                }
            })
            .into_iter()
            .collect::<IndexMap<_, _>>();

        let is_trait_object = looks_like_trait_object(name.as_deref(), &members);
        TypeDeclaration::Structure {
            namespaces: die_ref.namespace(),
            name,
            byte_size: die.byte_size(),
            members,
            type_params,
            is_trait_object,
        }
    }

    fn parse_member<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> StructureMember {
        let die = die_ref.deref_ensure();
        let loc = die.data_member_location().as_ref().map(|attr| attr.value());
        let in_struct_location = if let Some(offset) = loc.as_ref().and_then(|l| l.sdata_value()) {
            Some(MemberLocation::Offset(offset))
        } else if let Some(AttributeValue::Exprloc(ref expr)) = loc {
            Some(MemberLocation::Expr(MemberLocationExpression {
                expr: expr.clone(),
            }))
        } else {
            None
        };

        let mb_type_ref = die.type_ref();
        if let Some(reference) = mb_type_ref {
            self.parse_inner(die_ref, reference);
        }

        StructureMember {
            in_struct_location,
            name: die.name(),
            type_ref: mb_type_ref,
            decl_file_line: die.decl_file_line(),
        }
    }

    fn parse_struct_enum(&mut self, die_ref: FatDieRef<'_>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();

        let variant_part = die.for_each_children_t(|child| {
            (child.tag() == gimli::DW_TAG_variant_part).then_some(child)
        });

        let mut member_from_ref = |type_ref: DieAddr| -> Option<StructureMember> {
            let unit = match type_ref {
                DieAddr::Unit(_) => Some(die_ref.unit()),
                DieAddr::Global(debug_info_offset) => {
                    die_ref.debug_info.find_unit(debug_info_offset)
                }
            }?;
            let offset = type_ref.unit_offset(unit);

            let fref = FatDieRef::new_no_hint(die_ref.debug_info, unit.idx(), offset);
            let die = fref.deref_ensure();

            if die.tag() == gimli::DW_TAG_member {
                return Some(self.parse_member(FatDieRef::new_no_hint(
                    die_ref.debug_info,
                    unit.idx(),
                    offset,
                )));
            }
            None
        };

        let discr_type = variant_part.as_ref().and_then(|variant| {
            variant
                .discr_ref()
                .and_then(&mut member_from_ref)
                .or_else(|| variant.type_ref().and_then(&mut member_from_ref))
        });

        let variants = variant_part
            .map(|vp| {
                let variant_offsets = vp.for_each_children_filter_collect(|child| {
                    if child.tag() == gimli::DW_TAG_variant {
                        Some(child.offset())
                    } else {
                        None
                    }
                });

                variant_offsets
                    .into_iter()
                    .filter_map(|off| weak_error!(Die::new(die_ref.dcx(), off)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let enumerators = variants
            .iter()
            .filter_map(|variant| {
                let member = variant.for_each_children_t(|child| {
                    if child.tag() == gimli::DW_TAG_member {
                        let die_ref = FatDieRef::new_no_hint(
                            die_ref.debug_info,
                            die_ref.unit().idx(),
                            child.offset(),
                        );
                        Some(self.parse_member(die_ref))
                    } else {
                        None
                    }
                });

                Some((variant.discr_value(), member?))
            })
            .collect::<HashMap<_, _>>();

        TypeDeclaration::RustEnum {
            namespaces: die_ref.namespace(),
            name,
            byte_size: die.byte_size(),
            discr_type: discr_type.map(Box::new),
            enumerators,
        }
    }

    fn parse_enum<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();

        let mb_discr_type = die.type_ref();
        if let Some(reference) = mb_discr_type {
            self.parse_inner(die_ref, reference);
        }

        let enumerators = die
            .for_each_children_filter_collect(|child| {
                if child.tag() == gimli::DW_TAG_enumerator {
                    Some((child.const_value()?, child.name()?))
                } else {
                    None
                }
            })
            .into_iter()
            .collect::<HashMap<_, _>>();

        TypeDeclaration::CStyleEnum {
            namespaces: die_ref.namespace(),
            name,
            byte_size: die.byte_size(),
            discr_type: mb_discr_type,
            enumerators,
        }
    }

    fn parse_union(&mut self, die_ref: FatDieRef<'_>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();

        let members_refs = die.for_each_children_filter_collect(|child| {
            if child.tag() == gimli::DW_TAG_member {
                Some(FatDieRef::new_no_hint(
                    die_ref.debug_info,
                    die_ref.unit().idx(),
                    child.offset(),
                ))
            } else {
                None
            }
        });

        let members = members_refs
            .into_iter()
            .map(|r| self.parse_member(r))
            .collect::<Vec<_>>();

        TypeDeclaration::Union {
            namespaces: die_ref.namespace(),
            name,
            byte_size: die.byte_size(),
            members,
        }
    }

    fn parse_pointer<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();
        let mb_type_ref = die.type_ref();
        if let Some(reference) = mb_type_ref {
            self.parse_inner(die_ref, reference);
        }

        TypeDeclaration::Pointer {
            namespaces: die_ref.namespace(),
            name,
            target_type: mb_type_ref,
        }
    }

    fn parse_subroutine<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
        let die = die_ref.deref_ensure();
        let name = die.name();
        let mb_ret_type_ref = die.type_ref();
        if let Some(reference) = mb_ret_type_ref {
            self.parse_inner(die_ref, reference);
        }

        TypeDeclaration::Subroutine {
            namespaces: die_ref.namespace(),
            name,
            return_type: mb_ret_type_ref,
        }
    }
}

macro_rules! parse_modifier_fn {
    ($fn_name: ident, $die: ty, $modifier: expr) => {
        fn $fn_name<'dbg>(&mut self, die_ref: FatDieRef<'dbg>) -> TypeDeclaration {
            let d = die_ref.deref_ensure();
            let name = d.name();
            let mb_type_ref = d.type_ref();
            if let Some(inner_type) = mb_type_ref {
                self.parse_inner(die_ref, inner_type);
            }

            TypeDeclaration::ModifiedType {
                namespaces: die_ref.namespace(),
                modifier: $modifier,
                name,
                inner: mb_type_ref,
            }
        }
    };
}

// create parsers for type modifiers
impl TypeParser {
    parse_modifier_fn!(parse_typedef, TypeDefDie, CModifier::TypeDef);
    parse_modifier_fn!(parse_const_type, ConstTypeDie, CModifier::Const);
    parse_modifier_fn!(parse_atomic, AtomicDie, CModifier::Atomic);
    parse_modifier_fn!(parse_volatile, VolatileDie, CModifier::Volatile);
    parse_modifier_fn!(parse_restrict, RestrictDie, CModifier::Restrict);
}

/// A cache structure for types.
/// Every type identified by its `TypeId` and DWARF unit uuid.
pub type TypeCache = HashMap<(Uuid, TypeId), Rc<ComplexType>>;

/// Phase 3 Feature A — heuristic detector for the rustc
/// fat-pointer representation of a `dyn Trait`.
///
/// Two independent signals; either is sufficient:
///
/// * **Name pattern** — rustc emits the wrapping struct's
///   `DW_AT_name` containing `dyn ` (e.g.
///   `alloc::boxed::Box<dyn core::error::Error, alloc::alloc::Global>`).
/// * **Member shape** — exactly two members named `pointer` and
///   `vtable` (or `data_ptr` and `vtable` in some versions).
///
/// The two-signal approach is robust against rustc renaming the
/// struct on us — if the name changes, the member shape still
/// catches it; if the member shape changes, the name still does.
fn looks_like_trait_object(name: Option<&str>, members: &[StructureMember]) -> bool {
    if name.is_some_and(|n| n.contains("dyn ")) {
        return true;
    }
    if members.len() == 2 {
        let m0 = members[0].name.as_deref();
        let m1 = members[1].name.as_deref();
        let pair = (m0, m1);
        return matches!(
            pair,
            (Some("pointer"), Some("vtable"))
                | (Some("data_ptr"), Some("vtable"))
                | (Some("vtable"), Some("pointer"))
                | (Some("vtable"), Some("data_ptr"))
        );
    }
    false
}

/// Phase 3 Feature D step 1 — heuristic detector for the rustc
/// coroutine state-machine type. The synthesised type name for the
/// body of an `async fn` (or any generator/coroutine) carries one of
/// three suffixes depending on rustc version:
///
/// * `{async_fn_env#N}` — historical
/// * `{coroutine_env#N}` — current (post `coroutine` rename)
/// * `{generator_env#N}` — pre-`coroutine` legacy
///
/// where `N` is a per-compilation-unit disambiguator. This helper is
/// used for intent-clear prefilters and richer diagnostics — the
/// existing `AsyncFnFuture::try_from` already pattern-matches on the
/// active variant's *state* name (`Suspend{N}` / `Unresumed` / …),
/// which is what actually drives correctness; the type-name check
/// is the diagnostics layer that lets the renderer say "this is a
/// coroutine but its state name didn't decode" rather than
/// silently returning a parse error.
pub fn looks_like_coroutine_type_name(name: &str) -> bool {
    name.contains("{async_fn_env#")
        || name.contains("{coroutine_env#")
        || name.contains("{generator_env#")
}

#[cfg(test)]
mod coroutine_type_name_tests {
    use super::looks_like_coroutine_type_name;

    #[test]
    fn matches_async_fn_env_pattern() {
        assert!(looks_like_coroutine_type_name(
            "tokio_simple_await::worker_task::{async_fn_env#0}"
        ));
        assert!(looks_like_coroutine_type_name(
            "my_app::nested::module::deep::handler::{async_fn_env#7}"
        ));
    }

    #[test]
    fn matches_coroutine_env_pattern() {
        // Current rustc post the `coroutine` rename.
        assert!(looks_like_coroutine_type_name(
            "my_app::handler::{coroutine_env#0}"
        ));
    }

    #[test]
    fn matches_generator_env_legacy_pattern() {
        // Pre-`coroutine` rustc.
        assert!(looks_like_coroutine_type_name(
            "my_app::handler::{generator_env#0}"
        ));
    }

    #[test]
    fn rejects_ordinary_enum_names() {
        assert!(!looks_like_coroutine_type_name("Option<i32>"));
        assert!(!looks_like_coroutine_type_name(
            "core::result::Result<u32, std::io::Error>"
        ));
        // Closure environment, not a coroutine — must not match.
        assert!(!looks_like_coroutine_type_name(
            "my_app::handler::{closure_env#0}"
        ));
    }

    #[test]
    fn rejects_partial_substring_matches() {
        // The marker uses braces; a stray substring without them
        // (very unlikely but cheap to guard against) should not
        // light up. Note: this is a sanity check; the contains()
        // check is intentionally permissive about surrounding
        // context because rustc may decorate the name (generic
        // instantiation, monomorphisation suffixes, …).
        assert!(!looks_like_coroutine_type_name("async_fn_env"));
        assert!(!looks_like_coroutine_type_name("coroutine_env_v2"));
    }
}

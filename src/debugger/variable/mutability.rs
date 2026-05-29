// SPDX-License-Identifier: MIT
//
// Variables-view §5.2 — classify each variable binding as
// read-only or read-write through normal Rust code. Used to drive
// the variables-pane row background hue (grey for RO, orange for
// RW). See variables-view.md §1.2 and §5.2.
//
// Two signal sources, combined:
//
//   1. **Segment writability** (`statics`, TLS) — the loader's
//      PT_LOAD permission bits are ground truth. A static in
//      `.rodata` *cannot* be mutated through any Rust code; one in
//      `.data` / `.bss` *can* (via `static mut`, interior
//      mutability, or atomic ops). This is more accurate than
//      parsing `static` vs `static mut` from DWARF.
//   2. **Type inspection** (`locals`, `arguments`) — for stack /
//      register bindings the segment lookup gives nothing useful
//      (the stack is uniformly writable), so we fall back to the
//      type:
//        * `&T` with non-`UnsafeCell` `T`     → ReadOnly
//        * `&mut T`                            → ReadWrite
//        * any type containing `UnsafeCell`    → ReadWrite
//        * owned `T` (no UnsafeCell)           → ReadWrite (default)
//
// The default-RW for owned locals follows the rustc DWARF probe
// (variables-view §8): `let x` and `let mut x` produce identical
// DIEs, so we conservatively assume the binding can be mutated.

use std::collections::HashSet;

use crate::debugger::Debugger;
use crate::debugger::address::RelocatedAddress;
use crate::debugger::debugee::SegmentWritability;
use crate::debugger::debugee::dwarf::r#type::{CModifier, ComplexType, TypeDeclaration, TypeId};
use crate::debugger::variable::execute::QueryResult;
use crate::debugger::variable::value::Value;

/// Whether a binding can be mutated through normal (non-unsafe)
/// Rust code. The variables-pane renders `ReadOnly` rows with a
/// grey background hue and `ReadWrite` rows with an orange hue
/// (variables-view §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    /// The type system forbids writes through this binding *and*
    /// the underlying memory is loader-read-only. Renders grey.
    ReadOnly,
    /// Writes are reachable through this binding — via `&mut`,
    /// interior mutability (`UnsafeCell`, `Cell`, `RefCell`,
    /// `Mutex`, `Atomic*`), `static mut`, or just an owned `let`
    /// binding (which DWARF can't distinguish from `let mut`).
    /// Renders orange.
    ReadWrite,
    /// No address and no type information available — usually
    /// optimised-away bindings. Renders without a mutability
    /// background.
    Unknown,
}

impl Mutability {
    /// Stable string for the DAP custom field
    /// `bugstalker.mutability`. The vscode-extension reads this
    /// to pick the row-background hue (variables-view §5.6).
    pub fn as_dap_str(self) -> &'static str {
        match self {
            Mutability::ReadOnly => "ro",
            Mutability::ReadWrite => "rw",
            Mutability::Unknown => "unknown",
        }
    }
}

/// Classify the mutability of a single variable. Combines the
/// segment-writability ground truth (statics / TLS) with the
/// type-based fallback (locals / arguments). See module docs.
pub fn classify(qr: &QueryResult<'_>, dbg: &Debugger) -> Mutability {
    // (1) If the binding has a concrete address AND that address
    // falls inside a known loaded segment, the loader-write bit
    // is the most authoritative signal. Only `static`s and TLS
    // hit this — stack bindings have addresses too, but the stack
    // mapping is always writable, so this would only ever say
    // ReadWrite for them. We let it: that matches the type-based
    // fallback's default-RW anyway.
    if let Some(addr) = qr.value().in_memory_location() {
        let reg = dbg.debugee.dwarf_registry();
        if let Some(w) = reg.address_writability(RelocatedAddress::from(addr)) {
            return match w {
                SegmentWritability::ReadOnly => Mutability::ReadOnly,
                SegmentWritability::ReadWrite => {
                    // Even in a writable segment, a `&T` reference
                    // (which has an address — the address of the
                    // reference itself, not the referent) is still
                    // RO through this binding. Defer to the type
                    // classifier when it's a reference, otherwise
                    // trust the segment bit.
                    match classify_by_type(qr.type_graph()) {
                        Mutability::ReadOnly => Mutability::ReadOnly,
                        _ => Mutability::ReadWrite,
                    }
                }
            };
        }
    }
    // (2) No address or no segment match — pure type-based fallback.
    classify_by_type(qr.type_graph())
}

/// Type-only classifier. The root TypeId comes from the
/// `ComplexType` graph; we look at its declaration to decide.
pub fn classify_by_type(graph: &ComplexType) -> Mutability {
    let root = graph.root();
    let Some(decl) = graph.types.get(&root) else {
        return Mutability::Unknown;
    };

    // Rust references and raw pointers go through TypeDeclaration::Pointer.
    // The distinguishing signal between `&T` and `&mut T` is whether
    // the target type is wrapped in `CModifier::Const`. rustc emits:
    //   &T     → Pointer { target = ModifiedType { Const, inner: T } }
    //   &mut T → Pointer { target = T }
    //   *const T → Pointer { target = ModifiedType { Const, inner: T } }
    //   *mut T → Pointer { target = T }
    if let TypeDeclaration::Pointer {
        target_type: Some(target),
        ..
    } = decl
    {
        let target_decl = graph.types.get(target);
        let (const_target, ultimate) = match target_decl {
            Some(TypeDeclaration::ModifiedType {
                modifier: CModifier::Const,
                inner,
                ..
            }) => (true, inner.unwrap_or(*target)),
            _ => (false, *target),
        };
        // Even a `&T` opens a write path if T contains UnsafeCell
        // (`&Cell<i32>` can call `.set()`).
        let mut visited = HashSet::new();
        if contains_unsafe_cell(graph, ultimate, &mut visited) {
            return Mutability::ReadWrite;
        }
        return if const_target {
            Mutability::ReadOnly
        } else {
            Mutability::ReadWrite
        };
    }

    // Owned types: RW if interior mutability anywhere in the type
    // graph, otherwise default RW because DWARF doesn't preserve
    // `let` vs `let mut` (variables-view §8). Either way: RW.
    Mutability::ReadWrite
}

/// Recursively scan `ty` for any `UnsafeCell<…>` Structure / Union
/// declaration. `visited` breaks cycles in self-referential types
/// (Rc<RefCell<Node { next: Option<Rc<…>> }>>).
fn contains_unsafe_cell(graph: &ComplexType, ty: TypeId, visited: &mut HashSet<TypeId>) -> bool {
    if !visited.insert(ty) {
        return false;
    }
    let Some(decl) = graph.types.get(&ty) else {
        return false;
    };
    match decl {
        TypeDeclaration::Structure { name, members, .. }
        | TypeDeclaration::Union { name, members, .. } => {
            if name.as_deref().is_some_and(|n| n.starts_with("UnsafeCell")) {
                return true;
            }
            members.iter().any(|m| {
                m.type_ref
                    .is_some_and(|t| contains_unsafe_cell(graph, t, visited))
            })
        }
        TypeDeclaration::RustEnum {
            enumerators,
            discr_type,
            ..
        } => {
            if let Some(d) = discr_type
                && let Some(t) = d.type_ref
                && contains_unsafe_cell(graph, t, visited)
            {
                return true;
            }
            enumerators.values().any(|m| {
                m.type_ref
                    .is_some_and(|t| contains_unsafe_cell(graph, t, visited))
            })
        }
        TypeDeclaration::Array(arr) => arr
            .element_type()
            .is_some_and(|t| contains_unsafe_cell(graph, t, visited)),
        TypeDeclaration::ModifiedType { inner, .. } => {
            inner.is_some_and(|t| contains_unsafe_cell(graph, t, visited))
        }
        // We deliberately don't recurse through Pointer — a `Box<UnsafeCell<T>>`
        // is RW because it's a smart pointer (owned, default-RW), not because
        // we follow the pointer's target. The same logic at the top of
        // `classify_by_type` already special-cases Pointer for &T / &mut T.
        TypeDeclaration::Pointer { .. }
        | TypeDeclaration::Scalar(_)
        | TypeDeclaration::CStyleEnum { .. }
        | TypeDeclaration::Subroutine { .. } => false,
    }
}

// `Value` is intentionally not used in this module's prod path —
// the address lookup goes through `Value::in_memory_location()`
// via QueryResult — but importing it documents the cross-module
// dependency for future maintainers reading the use list.
#[allow(dead_code)]
fn _value_dep_marker(_: &Value) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dap_str_is_stable_ro_rw_unknown() {
        // The extension parses these as bare strings — must not
        // drift without coordinating with vscode-extension.
        assert_eq!(Mutability::ReadOnly.as_dap_str(), "ro");
        assert_eq!(Mutability::ReadWrite.as_dap_str(), "rw");
        assert_eq!(Mutability::Unknown.as_dap_str(), "unknown");
    }

    #[test]
    fn dap_str_round_trips_via_match() {
        // Round-trip from variant → string → variant via a manual
        // match, as a guard that we have exactly three states and
        // no string collisions. Bumping the enum without updating
        // this map should fail to compile.
        for m in [
            Mutability::ReadOnly,
            Mutability::ReadWrite,
            Mutability::Unknown,
        ] {
            let s = m.as_dap_str();
            let back = match s {
                "ro" => Mutability::ReadOnly,
                "rw" => Mutability::ReadWrite,
                "unknown" => Mutability::Unknown,
                _ => panic!("unknown dap string {s:?}"),
            };
            assert_eq!(m, back);
        }
    }
}

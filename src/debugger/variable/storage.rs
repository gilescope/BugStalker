// SPDX-License-Identifier: MIT
//
// Variables-view §5.3 — classify each variable binding by where
// its storage lives. Drives the leading storage-class glyph in
// the variables pane (variables-view.md §1.3):
//
//   ⬛  Stack             — DW_OP_fbreg / DW_OP_breg* (frame-pointer relative)
//   🟦  Register          — DW_OP_reg* / DW_OP_regx
//   ⬜  StaticReadOnly    — DW_OP_addr in a loader-RO segment (.rodata)
//   🟧  StaticReadWrite   — DW_OP_addr in a loader-RW segment (.data, .bss)
//   🟣  ThreadLocal       — DW_OP_form_tls_address / DW_OP_GNU_push_tls_address
//   👻  OptimizedAway     — DW_OP_implicit_value / implicit_pointer / no location
//   (?) Unknown           — other / composite expressions we don't recognise yet
//
// Two signals:
//   1. The raw DWARF location expression's first significant
//      operation. This is the SOURCE — what rustc + LLVM committed
//      this variable to.
//   2. For Address-producing operations, the evaluated runtime
//      address gets cross-referenced with `DwarfRegistry`'s
//      segment-kind / segment-writability index (built in §5.2)
//      to split Static into RO / RW and to catch the rare case
//      where the address sits in `[stack]` or anon-rw rather than
//      a file-backed segment.

use crate::debugger::Debugger;
use crate::debugger::address::RelocatedAddress;
use crate::debugger::debugee::dwarf::EndianArcSlice;
use crate::debugger::debugee::{SegmentKind, SegmentWritability};
use crate::debugger::variable::value::Value;
use gimli::{Expression, Operation};

/// Where a variable binding's storage lives. See module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageClass {
    Stack,
    Register,
    StaticReadOnly,
    StaticReadWrite,
    ThreadLocal,
    OptimizedAway,
    /// The location expression uses opcodes we don't currently
    /// classify (DW_OP_pick + composite expression heads, etc.).
    /// Surfacing as Unknown rather than silently misclassifying.
    Unknown,
}

impl StorageClass {
    /// Stable string for the DAP custom field `bugstalker.storage`.
    /// The vscode-extension reads this to pick the leading glyph
    /// per variables-view.md §1.3. Bumping the enum without
    /// updating this map should fail the round-trip unit test.
    pub fn as_dap_str(self) -> &'static str {
        match self {
            StorageClass::Stack => "stack",
            StorageClass::Register => "register",
            StorageClass::StaticReadOnly => "static_ro",
            StorageClass::StaticReadWrite => "static_rw",
            StorageClass::ThreadLocal => "tls",
            StorageClass::OptimizedAway => "optimized",
            StorageClass::Unknown => "unknown",
        }
    }
}

/// Classify the storage of a variable from its raw DWARF location
/// expression + (optionally) the already-evaluated runtime address.
///
/// `expr` is `None` when the DIE has no `DW_AT_location` at all
/// (rustc / LLVM optimised the variable out of the binary).
///
/// `evaluated_addr` is the runtime address the expression evaluated
/// to, if any — passed in so we don't have to re-run the evaluator.
/// Used to split Static into RO / RW via the segment-writability
/// index and to recover the kind of mapping an Address-producing
/// expression resolved into.
///
/// `encoding` is needed by gimli to decode the Expression's
/// operations — it carries DWARF version / address size.
pub fn classify(
    expr: Option<&Expression<EndianArcSlice>>,
    evaluated_addr: Option<usize>,
    encoding: gimli::Encoding,
    dbg: &Debugger,
) -> StorageClass {
    let Some(expr) = expr else {
        return StorageClass::OptimizedAway;
    };

    // Walk operations. The FIRST non-trivial one tells us the source.
    // Composite expressions (a sequence terminated by DW_OP_piece)
    // are flattened: we take the first piece's classification, which
    // matches what the variables pane shows as the row's location.
    // `operations` consumes the Expression, so clone — Expressions
    // are cheap (Arc-backed slices over the .debug_loc bytes).
    let mut ops = expr.clone().operations(encoding);
    while let Ok(Some(op)) = ops.next() {
        match op {
            Operation::Register { .. } => return StorageClass::Register,
            Operation::FrameOffset { .. } | Operation::RegisterOffset { .. } => {
                // FrameOffset is unambiguously DW_OP_fbreg. RegisterOffset
                // is DW_OP_breg<N> — almost always a frame-pointer-relative
                // stack reference in Rust binaries (rustc rarely emits
                // bregN for non-stack purposes), so treating both as
                // Stack is correct in practice. If we later find a
                // counter-example we'll tighten this.
                return StorageClass::Stack;
            }
            Operation::TLS => return StorageClass::ThreadLocal,
            Operation::Address { .. } => {
                return classify_address(evaluated_addr, dbg);
            }
            Operation::ImplicitValue { .. } | Operation::ImplicitPointer { .. } => {
                return StorageClass::OptimizedAway;
            }
            // Skip non-locational operations (arithmetic, stack
            // manipulation) and look at the next opcode.
            _ => continue,
        }
    }
    // Ran out of operations without finding a classification opcode —
    // unusual. Fall back to address-based classification if we have one.
    match evaluated_addr {
        Some(_) => classify_address(evaluated_addr, dbg),
        None => StorageClass::Unknown,
    }
}

/// Classify an Address-producing DW_OP_* via the segment index.
/// Falls back through Static→Stack→Heap→AnonRw→Other depending on
/// what `DwarfRegistry::address_segment_kind` reports for the
/// containing mapping.
fn classify_address(evaluated_addr: Option<usize>, dbg: &Debugger) -> StorageClass {
    let Some(addr) = evaluated_addr else {
        // We saw DW_OP_addr in the opcode list but the evaluator
        // didn't give us a concrete address — odd. Default to
        // Unknown rather than guessing.
        return StorageClass::Unknown;
    };
    let reg = dbg.debugee.dwarf_registry();
    let raddr = RelocatedAddress::from(addr);
    match (reg.address_segment_kind(raddr), reg.address_writability(raddr)) {
        (Some(SegmentKind::Static), Some(SegmentWritability::ReadOnly)) => {
            StorageClass::StaticReadOnly
        }
        (Some(SegmentKind::Static), Some(SegmentWritability::ReadWrite)) => {
            StorageClass::StaticReadWrite
        }
        (Some(SegmentKind::Stack), _) => StorageClass::Stack,
        (Some(SegmentKind::Heap | SegmentKind::AnonRw), _) => {
            // Bindings rarely live in heap / anon-rw, but rustc can
            // produce DW_OP_addr pointing into anon TLS arenas etc.
            // Caller can flag with the §5.3 heap overlay if it's
            // actually heap-allocated data.
            StorageClass::StaticReadWrite
        }
        (Some(SegmentKind::Other), Some(SegmentWritability::ReadOnly)) => {
            StorageClass::StaticReadOnly
        }
        (Some(SegmentKind::Other), Some(SegmentWritability::ReadWrite)) => {
            StorageClass::StaticReadWrite
        }
        _ => StorageClass::Unknown,
    }
}

/// Variables-view §1.3 / §5.3: heap overlay. Returns `true` when
/// the given `Value` is a pointer-like binding whose pointee
/// address lives in a heap-ish mapping (`[heap]` or any anonymous
/// read-write region — Rust's default allocator overwhelmingly
/// uses mmap'd anon-rw rather than `brk`, so anon-rw is the more
/// common signal here).
///
/// The overlay layers on top of the storage glyph: a `Box<T>` on
/// the stack with its pointee on the heap renders as `⬛↗`.
pub fn value_points_to_heap(value: &Value, dbg: &Debugger) -> bool {
    let Some(addr) = pointee_address(value) else {
        return false;
    };
    let raddr = RelocatedAddress::from(addr);
    let kind = dbg.debugee.dwarf_registry().address_segment_kind(raddr);
    matches!(kind, Some(SegmentKind::Heap | SegmentKind::AnonRw))
}

/// Extract the pointee address from a pointer-like `Value`. Covers
/// raw `*const T` / `*mut T` / `&T` / `&mut T` plus smart pointers
/// (`Box`, `Rc`, `Arc`, `NonNull`, `Weak`). Returns `None` for
/// non-pointer values.
fn pointee_address(value: &Value) -> Option<usize> {
    use crate::debugger::variable::value::SpecializedValue;
    match value {
        Value::Pointer(p) => p.value.map(|p| p as usize),
        Value::Specialized {
            value: Some(spec), ..
        } => match spec {
            SpecializedValue::Rc(p) | SpecializedValue::Arc(p) | SpecializedValue::NonNull(p) => {
                p.value.map(|p| p as usize)
            }
            SpecializedValue::Weak { ptr, .. } => ptr.value.map(|p| p as usize),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dap_str_is_stable() {
        // The vscode-extension parses these as bare strings.
        // Drifting them without coordinating with the extension
        // breaks the §5.3 glyph rendering.
        assert_eq!(StorageClass::Stack.as_dap_str(), "stack");
        assert_eq!(StorageClass::Register.as_dap_str(), "register");
        assert_eq!(StorageClass::StaticReadOnly.as_dap_str(), "static_ro");
        assert_eq!(StorageClass::StaticReadWrite.as_dap_str(), "static_rw");
        assert_eq!(StorageClass::ThreadLocal.as_dap_str(), "tls");
        assert_eq!(StorageClass::OptimizedAway.as_dap_str(), "optimized");
        assert_eq!(StorageClass::Unknown.as_dap_str(), "unknown");
    }

    #[test]
    fn dap_str_round_trips_via_match() {
        for s in [
            StorageClass::Stack,
            StorageClass::Register,
            StorageClass::StaticReadOnly,
            StorageClass::StaticReadWrite,
            StorageClass::ThreadLocal,
            StorageClass::OptimizedAway,
            StorageClass::Unknown,
        ] {
            let txt = s.as_dap_str();
            let back = match txt {
                "stack" => StorageClass::Stack,
                "register" => StorageClass::Register,
                "static_ro" => StorageClass::StaticReadOnly,
                "static_rw" => StorageClass::StaticReadWrite,
                "tls" => StorageClass::ThreadLocal,
                "optimized" => StorageClass::OptimizedAway,
                "unknown" => StorageClass::Unknown,
                _ => panic!("unmapped storage string {txt:?}"),
            };
            assert_eq!(s, back);
        }
    }
}

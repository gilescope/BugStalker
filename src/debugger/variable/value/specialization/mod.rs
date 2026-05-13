// SPDX-License-Identifier: MIT
use crate::debugger::debugee::dwarf::r#type::{TypeId, TypeIdentity};
use crate::debugger::variable::ObjectBinaryRepr;
use crate::debugger::variable::render::RenderValue;
use crate::debugger::variable::value::AssumeError::{
    TypeParameterNotFound, TypeParameterTypeNotFound, UnexpectedType,
};
use crate::debugger::variable::value::ParsingError::Assume;
use crate::debugger::variable::value::parser::{MAX_RENDER_DEPTH, ParseContext, ValueParser};
use crate::debugger::variable::value::specialization::btree::BTreeReflection;
use crate::debugger::variable::value::specialization::hashbrown::HashmapReflection;
use crate::debugger::variable::value::{
    ArrayItem, ArrayValue, AssumeError, FieldOrIndex, Member, ParsingError, ScalarValue,
    SupportedScalar,
};
use crate::debugger::variable::value::{PointerValue, StructValue, Value};
use crate::version::RustVersion;
use crate::{debugger, version_switch, weak_error};
use AssumeError::{FieldNotFound, IncompleteInterp, UnknownSize};
use anyhow::Context;
use bytes::Bytes;
use fallible_iterator::FallibleIterator;
use indexmap::IndexMap;

mod btree;
mod hashbrown;

/// During program execution, the debugger may encounter uninitialized variables.
/// For example, look at this code:
/// ```rust
///    let res: Result<(), String> = Ok(());
///     if let Err(e) = res {
///         unreachable!();
///     }
/// ```
///
/// if stop debugger at line 2 and consider a variable `e` - capacity of this vector
/// may be over 9000, this is obviously not the size that user expects.
/// Therefore, artificial restrictions on size and capacity are introduced. This behavior may be
/// changed in the future.
/// Phase 1 F3 — Render budget. Bounds the per-collection items the
/// renderer reads out of inferior memory so a corrupted length field
/// can't drive a multi-gigabyte read. Defaults match the long-standing
/// `LEN_GUARD` / `CAP_GUARD` constants (10 000 each); future work will
/// expose this through the `bs/setRenderBudget` DAP request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderBudget {
    pub len: i64,
    pub cap: i64,
}

impl Default for RenderBudget {
    fn default() -> Self {
        Self {
            len: LEN_GUARD,
            cap: CAP_GUARD,
        }
    }
}

/// Default ceiling on per-collection items the renderer reads out of
/// inferior memory. A corrupted length field can claim a multi-gigabyte
/// allocation; clamping here keeps reads bounded. Override per-session
/// via [`RenderBudget::len`].
pub const LEN_GUARD: i64 = 10_000;
/// Default ceiling on per-collection capacity reported to the user (in
/// addition to [`LEN_GUARD`] on length). Override per-session via
/// [`RenderBudget::cap`].
pub const CAP_GUARD: i64 = 10_000;

/// Phase 1 S15 — find a member named `field` and BFS-extract the
/// first `usize` scalar from its sub-tree. Used to read `strong` /
/// `weak` reference counts out of `RcBox` / `ArcInner` regardless of
/// whether they're wrapped in `Cell<usize>` (Rc) or peeled from
/// `AtomicUsize` (Arc — already unwrapped to `usize` by the S3
/// dispatcher by the time we look at it).
fn read_named_usize(val: &Value, field: &'static str) -> Option<u64> {
    let member_val = val.bfs_iterator().find_map(|(field_or_idx, child)| {
        if field_or_idx == FieldOrIndex::Field(Some(field)) {
            return Some(child);
        }
        None
    })?;
    member_val
        .bfs_iterator()
        .find_map(|(_, child)| match child {
            Value::Scalar(s) => match s.value {
                Some(SupportedScalar::Usize(n)) => Some(n as u64),
                _ => None,
            },
            _ => None,
        })
}

/// Phase 3 Feature C — eagerly deref an `Rc<T>` / `Arc<T>` pointer
/// so the renderer can show the inner allocation inline, with two
/// bounded guards: a per-parse visited-set on inner allocation
/// addresses (catches cycles like
/// `Rc<RefCell<Node { next: Option<Rc<RefCell<Node>>> }>>`) and a
/// global recursion-depth cap (catches deep non-cyclic chains
/// before they blow the renderer's stack).
///
/// The two together let `var some_node` terminate gracefully on
/// any shape: cycles render with a `[cycle to 0x…]` leaf at the
/// re-visit, deep chains stop at depth-64 with a `[depth limit]`
/// leaf.
/// Returns `Some(marker_string)` when we bailed (cycle / depth /
/// null pointer), `None` after a successful deref. Caller splices
/// the marker into whichever `TypeIdentity` the renderer actually
/// reads (the outer `StructValue` for the `Specialized::Rc/Arc`
/// arm; the `PointerValue` itself for any future smart-pointer that
/// renders directly via the pointer's identity).
pub(crate) fn eager_deref_with_cycle_check(
    pcx: &ParseContext,
    ptr: &mut PointerValue,
) -> Option<String> {
    let addr = ptr.value?;
    let key = addr as usize;
    if !pcx.visited_allocations.borrow_mut().insert(key) {
        return Some(format!("[cycle to {addr:p}]"));
    }
    let depth = pcx.recursion_depth.get();
    if depth >= MAX_RENDER_DEPTH {
        return Some(format!("[depth limit {MAX_RENDER_DEPTH}]"));
    }
    pcx.recursion_depth.set(depth + 1);
    ptr.dereffed = ptr.deref(pcx).map(Box::new);
    pcx.recursion_depth.set(depth);
    None
}

fn guard_len(len: i64) -> i64 {
    if len > LEN_GUARD { LEN_GUARD } else { len }
}

fn guard_cap(cap: i64) -> i64 {
    if cap > CAP_GUARD { CAP_GUARD } else { cap }
}

/// Phase 1 F3 — guard a collection length and report how many
/// elements were elided. Returned tuple is `(clamped_len, elided)`
/// where `elided` is `Some(n)` when `len > LEN_GUARD` and `n` is the
/// number of items that won't be rendered; `None` otherwise.
///
/// Negative inputs clamp to `0`. A negative length is never valid —
/// when we see one it means the field we read came from a slot that
/// isn't actually a live `&str` / collection (uninitialised stack,
/// the wrong DIE picked from an inlined variant, etc.). Passing
/// `len as usize` downstream with the bits intact yields ~16 EiB
/// and panics the allocator with `capacity overflow`. Clamp to 0
/// so the caller renders an empty string / collection instead of
/// killing the adapter.
fn guard_len_with_truncation(len: i64) -> (i64, Option<u64>) {
    if len <= 0 {
        (0, None)
    } else if len > LEN_GUARD {
        (LEN_GUARD, Some((len - LEN_GUARD) as u64))
    } else {
        (len, None)
    }
}

#[cfg(test)]
mod render_budget_tests {
    use super::{LEN_GUARD, RenderBudget, guard_len_with_truncation};

    #[test]
    fn defaults_match_constants() {
        let b = RenderBudget::default();
        assert_eq!(b.len, LEN_GUARD);
        assert_eq!(b.cap, LEN_GUARD);
    }

    #[test]
    fn truncation_under_budget() {
        let (clamped, elided) = guard_len_with_truncation(42);
        assert_eq!(clamped, 42);
        assert_eq!(elided, None);
    }

    #[test]
    fn truncation_at_budget() {
        let (clamped, elided) = guard_len_with_truncation(LEN_GUARD);
        assert_eq!(clamped, LEN_GUARD);
        assert_eq!(elided, None);
    }

    #[test]
    fn truncation_over_budget() {
        let (clamped, elided) = guard_len_with_truncation(LEN_GUARD + 1234);
        assert_eq!(clamped, LEN_GUARD);
        assert_eq!(elided, Some(1234));
    }

    #[test]
    fn truncation_negative_clamped_to_zero() {
        // Defensive: negative lengths happen when we read a slot
        // that isn't actually a live `&str` / collection. Clamp to
        // 0 so the caller's `len as usize` doesn't sign-extend to
        // ~16 EiB and crash the allocator.
        let (clamped, elided) = guard_len_with_truncation(-5);
        assert_eq!(clamped, 0);
        assert_eq!(elided, None);
    }
}

/// Specialised view over `Vec<T>` / `VecDeque<T>`. The original struct
/// is retained on `structure` (so the user can still inspect raw fields
/// with `:debug`) and `elided` flags whether the renderer truncated the
/// inner array against `LEN_GUARD`.
#[derive(Clone, PartialEq)]
pub struct VecValue {
    pub structure: StructValue,
    /// Phase 1 F3 — non-`None` when `len` exceeded `LEN_GUARD`; the
    /// number reported is how many items the renderer omitted from
    /// the inner array.
    pub elided: Option<u64>,
}

impl VecValue {
    pub fn slice(&mut self, left: Option<usize>, right: Option<usize>) {
        debug_assert!(matches!(
            self.structure.members.get_mut(0).map(|m| &m.value),
            Some(Value::Array(_))
        ));

        if let Some(Member {
            value: Value::Array(array),
            ..
        }) = self.structure.members.get_mut(0)
        {
            array.slice(left, right);
        }
    }
}

/// Specialised view over an owned `String`. `value` is the rendered
/// form; `elided` is set when the source string length exceeded
/// `LEN_GUARD` and the renderer truncated.
#[derive(Clone, PartialEq)]
pub struct StringVariable {
    pub value: String,
    /// Phase 1 F3 — number of chars elided when the underlying length
    /// exceeded `LEN_GUARD`. `None` for strings rendered in full.
    pub elided: Option<u64>,
}

/// Specialised view over `HashMap<K, V>` and `BTreeMap<K, V>`. Carries
/// the wrapper's `TypeIdentity` for reporting and a flat list of
/// fully-rendered `(key, value)` pairs.
#[derive(Clone, PartialEq)]
pub struct HashMapVariable {
    pub type_ident: TypeIdentity,
    pub kv_items: Vec<(Value, Value)>,
    /// Phase 1 F3 — number of `(K, V)` pairs the renderer elided
    /// because the underlying length exceeded `LEN_GUARD`. `None`
    /// when the map fit within budget.
    pub elided: Option<u64>,
}

/// Specialised view over `HashSet<T>` and `BTreeSet<T>`. Same shape as
/// [`HashMapVariable`] but with a single-element list per entry.
#[derive(Clone, PartialEq)]
pub struct HashSetVariable {
    pub type_ident: TypeIdentity,
    pub items: Vec<Value>,
    /// Phase 1 F3 — number of items the renderer elided.
    pub elided: Option<u64>,
}

/// Specialised view over `&str` slices. `value` is the rendered form;
/// `elided` flags truncation against `LEN_GUARD`.
#[derive(Clone, PartialEq)]
pub struct StrVariable {
    pub value: String,
    /// Phase 1 F3 — number of bytes elided when the underlying length
    /// exceeded `LEN_GUARD`. `None` for slices rendered in full.
    pub elided: Option<u64>,
}

/// Specialised view over `&[T]` / `&mut [T]` slices (the non-string
/// kind). DWARF lowers a Rust slice to a struct with `data_ptr` +
/// `length` fields, which without specialisation surfaces in the
/// Variables panel as a fat-pointer view — useless for inspecting
/// the elements. The parser reads `length` items of element-type
/// size starting at `data_ptr` and stores them here; the renderer
/// then surfaces them through `IndexedList` so the user sees
/// `[20, 30]` for `let slice: &[i32] = &arr[1..3]`.
#[derive(Clone, PartialEq)]
pub struct SliceVariable {
    /// Original fat-pointer struct, retained so a `:debug` view can
    /// still surface `data_ptr` + `length`.
    pub structure: StructValue,
    /// Parsed slice elements (index + Value). Same shape as
    /// `ArrayValue::items` so the existing `IndexedList` render
    /// path drives the display.
    pub items: Vec<ArrayItem>,
    /// Number of items the renderer elided because length exceeded
    /// LEN_GUARD. `None` when the slice fits in budget.
    pub elided: Option<u64>,
}

#[derive(Clone, PartialEq)]
pub struct TlsVariable {
    pub inner_value: Option<Box<Value>>,
    pub inner_type: TypeIdentity,
}

/// A "Pythonic" pretty-printer view over a [`Value`]. The parser
/// dispatches by namespace + type name to produce one of these variants
/// when a stdlib type benefits from custom rendering (collections,
/// time, smart pointers, sync primitives, etc.). The renderer in
/// `crate::debugger::variable::render` consumes them.
///
/// The original raw [`Value`] is preserved on every variant — either
/// inline (e.g. `Atomic(Box<Value>)`) or as a sibling field (e.g.
/// `VecValue::structure`) — so that `vard` (`Debug` mode) can always
/// fall back to the underlying struct even when a specialisation
/// applies.
#[derive(Clone, PartialEq)]
pub enum SpecializedValue {
    Vector(VecValue),
    VecDeque(VecValue),
    HashMap(HashMapVariable),
    HashSet(HashSetVariable),
    BTreeMap(HashMapVariable),
    BTreeSet(HashSetVariable),
    String(StringVariable),
    Str(StrVariable),
    Slice(SliceVariable),
    Tls(TlsVariable),
    Cell(Box<Value>),
    RefCell(Box<Value>),
    Rc(PointerValue),
    Arc(PointerValue),
    Uuid([u8; 16]),
    SystemTime((i64, u32)),
    Instant((i64, u32)),
    /// Phase 1 S3: `core::sync::atomic::Atomic*`. The inner `Value` is
    /// the atomic's payload — a scalar for `AtomicI*` / `AtomicU*` /
    /// `AtomicBool` / `AtomicUsize` / `AtomicIsize`, or a `*T` for
    /// `AtomicPtr<T>`. We do not acquire any lock / fence; the value
    /// is read directly through the `UnsafeCell` field.
    Atomic(Box<Value>),
    /// Phase 1 S11: `core::ptr::NonNull<T>`. Renders as a plain `*T`.
    NonNull(PointerValue),
    /// Phase 1 S7: `core::pin::Pin<P>`. Layout is a one-field tuple
    /// struct around `P`; we surface the inner `P` so users see the
    /// pinnee directly. The wrapper type identity (`Pin<&mut T>` /
    /// `Pin<Box<T>>` / etc.) is kept on `Value::r#type()`. Critical
    /// path for async stack inspection in Phase 3.
    Pin(Box<Value>),
    /// Phase 1 S6: `core::ops::Range*` family.
    Range(RangeValue),
    /// Phase 1 S4: `core::time::Duration` / `std::time::Duration`.
    /// Stored as `(secs, nanos)` because `core::time::Duration`'s
    /// internal layout is `secs: u64, nanos: u32`. The renderer
    /// formats as `1h 2m 3.500ms` (or `0s` for the zero duration).
    Duration((u64, u32)),
    /// Phase 1 S12: `alloc::ffi::c_str::CString`. Carries the
    /// pre-rendered display form (utf-8 quoted as `c"…"` when valid,
    /// hex preview `c"\\x..\\x.."` when not). The trailing NUL is
    /// always stripped from the rendered text.
    CString(StringVariable),
    /// Phase 1 S13/S14: `std::ffi::OsString` and `std::path::PathBuf`.
    /// On unix targets these bottom out in `Vec<u8>` underneath the
    /// `Buf` / `OsString` / `PathBuf` wrappers; we BFS-find the
    /// length + data pointer and render utf-8 (`"hello"`) when the
    /// bytes decode cleanly, hex preview (`b"\xNN…"`) when they
    /// don't. Windows WTF-8 buffers fall through to the hex preview.
    OsString(StringVariable),
    /// Phase 1 S10: `core::mem::MaybeUninit<T>`. The inner Value is
    /// the `value` arm (a `ManuallyDrop<T>` peeled to the `T`); we
    /// always render the value arm because the `uninit: ()` arm
    /// carries no payload, but mark the rendered output `[possibly
    /// uninit]` because the bytes may be garbage.
    MaybeUninit(Box<Value>),
    /// Phase 1 S1: `std::sync::Mutex<T>` / `std::sync::RwLock<T>`.
    /// `inner` is the `data: UnsafeCell<T>` field peeled to `T`.
    /// `poisoned` is read out of the `poison: poison::Flag` field
    /// (an `AtomicBool` peeled by S3). `locked` is read off the
    /// futex backend's `Futex { v: AtomicU32 }` (Linux, FreeBSD,
    /// OpenBSD, DragonFly, Hermit, modern Windows, wasm-atomics).
    /// On macOS / iOS / Win7 the layout is a `OnceBox<pthread_mutex_t>`
    /// or `SRWLOCK` and we conservatively report `locked = false`.
    /// The renderer surfaces `[locked]` and `[poisoned]` trailers
    /// when set. We never acquire the lock; this may show torn
    /// state if another thread is mid-write, expected for a peek.
    Mutex {
        inner: Box<Value>,
        poisoned: bool,
        locked: bool,
    },
    /// Phase 1 S2: `MutexGuard<T>` / `RwLockReadGuard<T>` /
    /// `RwLockWriteGuard<T>`. The guard carries a reference to the
    /// parent lock; we deref through that reference, peel the
    /// parent's `data: UnsafeCell<T>` field, and surface `T`. The
    /// `MutexGuard<T>` etc. wrapper name is preserved on the value's
    /// type identity.
    LockGuard(Box<Value>),
    /// Phase 1 S15: `alloc::rc::Weak<T>` / `alloc::sync::Weak<T>`.
    /// Carries the raw allocation pointer plus the strong / weak
    /// counts read out of the `RcBox` / `ArcInner` it points to.
    /// `strong == 0` means the underlying `T` has been dropped; the
    /// renderer surfaces a `[dropped]` annotation in that case.
    Weak {
        ptr: PointerValue,
        strong: u64,
        weak: u64,
    },
}

/// Phase 1 S6 — the six concrete `core::ops::Range*` shapes.
/// Renders to `start..end`, `start..=end`, `start..`, `..end`,
/// `..=end`, or `..`. `RangeInclusive` carries an `exhausted` bit
/// because once an inclusive iterator drains, libcore flips that
/// boolean to mark the range done; we surface it as a `[exhausted]`
/// trailer.
#[derive(Clone, PartialEq)]
pub enum RangeValue {
    /// `start..end`
    Half { start: Box<Value>, end: Box<Value> },
    /// `start..=end`
    Inclusive {
        start: Box<Value>,
        end: Box<Value>,
        exhausted: bool,
    },
    /// `start..`
    From { start: Box<Value> },
    /// `..end`
    To { end: Box<Value> },
    /// `..=end`
    ToInclusive { end: Box<Value> },
    /// `..` — no fields.
    Full,
}

impl RangeValue {
    /// Render the range to its canonical Rust source form
    /// (`start..end`, `start..=end`, `start..`, `..end`, `..=end`,
    /// `..`). `RangeInclusive` ranges that have already drained get a
    /// `[exhausted]` suffix. Bound values use the underlying scalar's
    /// `Display` impl via `value_layout()` round-tripping.
    pub fn render(&self) -> String {
        // Each bound is the inner Value's prerendered scalar text.
        // Falls back to `?` if the scalar doesn't render.
        let bound = |val: &Value| -> String {
            match val {
                Value::Scalar(s) => s
                    .value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                _ => "?".into(),
            }
        };
        match self {
            RangeValue::Half { start, end } => format!("{}..{}", bound(start), bound(end)),
            RangeValue::Inclusive {
                start,
                end,
                exhausted,
            } => {
                let core = format!("{}..={}", bound(start), bound(end));
                if *exhausted {
                    format!("{core} [exhausted]")
                } else {
                    core
                }
            }
            RangeValue::From { start } => format!("{}..", bound(start)),
            RangeValue::To { end } => format!("..{}", bound(end)),
            RangeValue::ToInclusive { end } => format!("..={}", bound(end)),
            RangeValue::Full => "..".to_string(),
        }
    }
}

pub struct VariableParserExtension<'a> {
    parser: &'a ValueParser,
}

impl<'a> VariableParserExtension<'a> {
    pub fn new(parser: &'a ValueParser) -> Self {
        Self { parser }
    }

    pub fn parse_str(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_str_inner(pcx, Value::Struct(structure.clone()))
                .context("&str interpretation")
        )
        .map(SpecializedValue::Str)
    }

    fn parse_str_inner(&self, pcx: &ParseContext, val: Value) -> Result<StrVariable, ParsingError> {
        let len = val.assume_field_as_scalar_number("length")?;
        let (len, elided) = guard_len_with_truncation(len);

        let data_ptr = val.assume_field_as_pointer("data_ptr")?;

        let data = debugger::read_memory_by_pid(
            pcx.evcx.ecx.pid_on_focus(),
            data_ptr as usize,
            len as usize,
        )
        .map(Bytes::from)?;

        Ok(StrVariable {
            value: String::from_utf8(data.to_vec()).map_err(AssumeError::from)?,
            elided,
        })
    }

    /// Parse a Rust slice `&[T]` / `&mut [T]`. The fat-pointer struct
    /// has fields `data_ptr` and `length`; we read `length` items of
    /// element-type size starting at `data_ptr` and parse each as a
    /// `Value`. Used by the parser dispatch when the underlying
    /// struct's type name is a slice (`&[T]` or `&mut [T]`).
    pub fn parse_slice(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        element_type: crate::debugger::debugee::dwarf::r#type::TypeId,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_slice_inner(pcx, structure.clone(), element_type)
                .context("&[T] slice interpretation")
        )
        .map(SpecializedValue::Slice)
    }

    fn parse_slice_inner(
        &self,
        pcx: &ParseContext,
        structure: StructValue,
        element_type: crate::debugger::debugee::dwarf::r#type::TypeId,
    ) -> Result<SliceVariable, ParsingError> {
        let val = Value::Struct(structure.clone());
        let len = val.assume_field_as_scalar_number("length")?;
        let (len, elided) = guard_len_with_truncation(len);
        let data_ptr = val.assume_field_as_pointer("data_ptr")? as usize;

        let el_type = pcx.type_graph;
        let el_type_size = el_type
            .type_size_in_bytes(pcx.evcx, element_type)
            .ok_or(UnknownSize(el_type.identity(element_type)))?
            as usize;

        let raw_data = debugger::read_memory_by_pid(
            pcx.evcx.ecx.pid_on_focus(),
            data_ptr,
            len as usize * el_type_size,
        )
        .map(Bytes::from)?;

        let (mut bytes_chunks, mut empty_chunks);
        let raw_items_iter: &mut dyn Iterator<Item = (usize, &[u8])> = if el_type_size != 0 {
            bytes_chunks = raw_data.chunks(el_type_size).enumerate();
            &mut bytes_chunks
        } else {
            let v: Vec<&[u8]> = vec![&[]; len as usize];
            empty_chunks = v.into_iter().enumerate();
            &mut empty_chunks
        };

        let items: Vec<ArrayItem> = raw_items_iter
            .filter_map(|(i, chunk)| {
                let data = ObjectBinaryRepr {
                    raw_data: raw_data.slice_ref(chunk),
                    address: Some(data_ptr + (i * el_type_size)),
                    size: el_type_size,
                };
                Some(ArrayItem {
                    index: i as i64,
                    value: self.parser.parse_inner(pcx, Some(data), element_type)?,
                })
            })
            .collect();

        Ok(SliceVariable {
            structure,
            items,
            elided,
        })
    }

    pub fn parse_string(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_string_inner(pcx, Value::Struct(structure.clone()))
                .context("String interpretation")
        )
        .map(SpecializedValue::String)
    }

    fn parse_string_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<StringVariable, ParsingError> {
        let len = val.assume_field_as_scalar_number("len")?;
        let (len, elided) = guard_len_with_truncation(len);

        let data_ptr = val.assume_field_as_pointer("pointer")?;

        let data = debugger::read_memory_by_pid(
            pcx.evcx.ecx.pid_on_focus(),
            data_ptr as usize,
            len as usize,
        )?;

        Ok(StringVariable {
            value: String::from_utf8(data).map_err(AssumeError::from)?,
            elided,
        })
    }

    pub fn parse_vector(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_vector_inner(pcx, Value::Struct(structure.clone()), type_params)
                .context("Vec<T> interpretation")
        )
        .map(SpecializedValue::Vector)
    }

    fn parse_vector_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<VecValue, ParsingError> {
        let inner_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;
        let len = val.assume_field_as_scalar_number("len")?;
        let (len, elided) = guard_len_with_truncation(len);

        let cap = extract_capacity(pcx, &val)? as i64;
        let cap = guard_cap(cap);

        let data_ptr = val.assume_field_as_pointer("pointer")? as usize;

        let el_type = pcx.type_graph;
        let el_type_size = el_type
            .type_size_in_bytes(pcx.evcx, inner_type)
            .ok_or(UnknownSize(el_type.identity(inner_type)))? as usize;

        let raw_data = debugger::read_memory_by_pid(
            pcx.evcx.ecx.pid_on_focus(),
            data_ptr,
            len as usize * el_type_size,
        )
        .map(Bytes::from)?;

        let (mut bytes_chunks, mut empty_chunks);
        let raw_items_iter: &mut dyn Iterator<Item = (usize, &[u8])> = if el_type_size != 0 {
            bytes_chunks = raw_data.chunks(el_type_size).enumerate();
            &mut bytes_chunks
        } else {
            // if an item type is zst
            let v: Vec<&[u8]> = vec![&[]; len as usize];
            empty_chunks = v.into_iter().enumerate();
            &mut empty_chunks
        };

        let items = raw_items_iter
            .filter_map(|(i, chunk)| {
                let data = ObjectBinaryRepr {
                    raw_data: raw_data.slice_ref(chunk),
                    address: Some(data_ptr + (i * el_type_size)),
                    size: el_type_size,
                };
                Some(ArrayItem {
                    index: i as i64,
                    value: self.parser.parse_inner(pcx, Some(data), inner_type)?,
                })
            })
            .collect::<Vec<_>>();

        Ok(VecValue {
            structure: StructValue {
                type_id: None,
                type_ident: val.r#type().clone(),
                members: vec![
                    Member {
                        field_name: Some("buf".to_owned()),
                        value: Value::Array(ArrayValue {
                            type_id: None,
                            type_ident: pcx.type_graph.identity(inner_type).as_array_type(),
                            items: Some(items),
                            // set to `None` because the address operator unavailable for spec vars
                            raw_address: None,
                        }),
                    },
                    Member {
                        field_name: Some("cap".to_owned()),
                        value: Value::Scalar(ScalarValue {
                            type_id: None,
                            type_ident: TypeIdentity::no_namespace("usize"),
                            value: Some(SupportedScalar::Usize(cap as usize)),
                            // set to `None` because the address operator unavailable for spec vars
                            raw_address: None,
                        }),
                    },
                ],
                type_params: type_params.clone(),
                // set to `None` because the address operator unavailable for spec vars
                raw_address: None,
            },
            elided,
        })
    }

    pub fn parse_tls_old(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        type_params: &IndexMap<String, Option<TypeId>>,
        is_const_initialized: bool,
    ) -> Option<SpecializedValue> {
        let tls_var = if is_const_initialized {
            self.parse_const_init_tls_inner(pcx, Value::Struct(structure.clone()), type_params)
        } else {
            self.parse_tls_inner_old(pcx, Value::Struct(structure.clone()), type_params)
        };

        weak_error!(tls_var.context("TLS variable interpretation")).map(SpecializedValue::Tls)
    }

    fn parse_const_init_tls_inner(
        &self,
        pcx: &ParseContext,
        inner: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<TlsVariable, ParsingError> {
        let value_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;
        let value = inner.field("value");
        Ok(TlsVariable {
            inner_value: value.map(Box::new),
            inner_type: pcx.type_graph.identity(value_type),
        })
    }

    fn parse_tls_inner_old(
        &self,
        pcx: &ParseContext,
        inner_val: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<TlsVariable, ParsingError> {
        let inner_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;

        let inner = inner_val
            .bfs_iterator()
            .find_map(|(field, child)| {
                (field == FieldOrIndex::Field(Some("inner"))).then_some(child)
            })
            .ok_or(FieldNotFound("inner"))?;
        let inner_option = inner.assume_field_as_rust_enum("value")?;
        let inner_value = inner_option.value.ok_or(IncompleteInterp("value"))?;

        // we assume that DWARF representation of tls variable contains ::Option
        if let Value::Struct(ref opt_variant) = inner_value.value {
            let tls_value = if opt_variant.type_ident.name() == Some("None") {
                None
            } else {
                Some(Box::new(
                    inner_value
                        .value
                        .bfs_iterator()
                        .find_map(|(field, child)| {
                            (field == FieldOrIndex::Field(Some("__0"))).then_some(child)
                        })
                        .ok_or(FieldNotFound("__0"))?
                        .clone(),
                ))
            };

            return Ok(TlsVariable {
                inner_value: tls_value,
                inner_type: pcx.type_graph.identity(inner_type),
            });
        }

        Err(ParsingError::Assume(IncompleteInterp(
            "expect TLS inner value as option",
        )))
    }

    pub fn parse_tls(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        type_params: &IndexMap<String, Option<TypeId>>,
        rv: RustVersion,
    ) -> Result<Option<TlsVariable>, ParsingError> {
        if structure.type_ident.namespace().contains(&["eager"]) {
            // constant tls
            self.parse_const_tls_inner(pcx, Value::Struct(structure.clone()), type_params)
        } else {
            self.parse_tls_inner(pcx, Value::Struct(structure.clone()), type_params, rv)
        }
    }

    fn parse_tls_inner(
        &self,
        pcx: &ParseContext,
        inner_val: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
        rv: RustVersion,
    ) -> Result<Option<TlsVariable>, ParsingError> {
        if type_params.is_empty() {
            return Ok(None);
        }

        let inner_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;

        let state = inner_val
            .bfs_iterator()
            .find_map(|(field, child)| {
                (field == FieldOrIndex::Field(Some("state"))).then_some(child)
            })
            .ok_or(FieldNotFound("state"))?;

        let state = state.assume_field_as_rust_enum("value")?;
        if let Some(member) = state.value {
            let tls_val = if member.field_name.as_deref() == Some("Alive") {
                version_switch!(
                    rv,
                    .. (1 . 89) => member.value.field("__0").map(Box::new),
                    (1 . 89) .. (1 . 94) => {
                        let get_val_from_storage = || {
                            let Value::Struct(storage) = inner_val else {
                                return None;
                            };

                            storage.field("value")?.field("value")?
                                    .field("value")?
                                    .field("value")
                                    .map(Box::new)
                        };

                        get_val_from_storage()
                    },
                    (1 . 94) .. => {
                        let get_val_from_storage = || {
                            let Value::Struct(storage) = inner_val else {
                                return None;
                            };

                            storage.field("value")?.field("value")?
                                    .field("value")?
                                    .field("value")?
                                    .field("__0")
                                    .map(Box::new)
                        };

                        get_val_from_storage()
                    }
                )
                .expect("all rust versions are covered")
            } else {
                return Ok(None);
            };

            return Ok(Some(TlsVariable {
                inner_value: tls_val,
                inner_type: pcx.type_graph.identity(inner_type),
            }));
        };
        Ok(None)
    }

    fn parse_const_tls_inner(
        &self,
        pcx: &ParseContext,
        inner_val: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<Option<TlsVariable>, ParsingError> {
        let inner_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;

        if let Some(val) = inner_val.field("val") {
            return Ok(Some(TlsVariable {
                inner_value: val.field("value").map(Box::new),
                inner_type: pcx.type_graph.identity(inner_type),
            }));
        }

        Err(ParsingError::Assume(IncompleteInterp(
            "expect TLS inner value as `val` field",
        )))
    }

    pub fn parse_hashmap(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_hashmap_inner(pcx, Value::Struct(structure.clone()))
                .context("HashMap<K, V> interpretation")
        )
        .map(SpecializedValue::HashMap)
    }

    fn parse_hashmap_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<HashMapVariable, ParsingError> {
        let ctrl = val.assume_field_as_pointer("pointer")?;
        let bucket_mask = val.assume_field_as_scalar_number("bucket_mask")?;

        let table = val.assume_field_as_struct("table")?;
        let kv_type = table
            .type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;

        let r#type = pcx.type_graph;
        let kv_size = r#type
            .type_size_in_bytes(pcx.evcx, kv_type)
            .ok_or(UnknownSize(r#type.identity(kv_type)))?;

        let reflection =
            HashmapReflection::new(ctrl as *mut u8, bucket_mask as usize, kv_size as usize);

        let iterator = reflection.iter(pcx.evcx.ecx.pid_on_focus())?;
        let mut kv_items: Vec<(Value, Value)> = iterator
            .map_err(ParsingError::from)
            .filter_map(|bucket| {
                let raw_data = bucket.read(pcx.evcx.ecx.pid_on_focus());
                let data = weak_error!(raw_data).map(|d| ObjectBinaryRepr {
                    raw_data: Bytes::from(d),
                    address: Some(bucket.location()),
                    size: bucket.size(),
                });

                let tuple = self.parser.parse_inner(pcx, data, kv_type);

                if let Some(Value::Struct(mut tuple)) = tuple
                    && tuple.members.len() == 2
                {
                    let v = tuple.members.pop();
                    let k = tuple.members.pop();
                    return Ok(Some((k.unwrap().value, v.unwrap().value)));
                }

                Err(Assume(UnexpectedType("hashmap bucket")))
            })
            .collect()?;

        // Phase 1 F3 — truncate at LEN_GUARD and report the count
        // of (k, v) pairs the renderer will skip.
        let elided = if kv_items.len() as i64 > LEN_GUARD {
            let n = (kv_items.len() as i64 - LEN_GUARD) as u64;
            kv_items.truncate(LEN_GUARD as usize);
            Some(n)
        } else {
            None
        };

        Ok(HashMapVariable {
            type_ident: val.r#type().to_owned(),
            kv_items,
            elided,
        })
    }

    pub fn parse_hashset(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_hashset_inner(pcx, Value::Struct(structure.clone()))
                .context("HashSet<T> interpretation")
        )
        .map(SpecializedValue::HashSet)
    }

    fn parse_hashset_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<HashSetVariable, ParsingError> {
        let ctrl = val.assume_field_as_pointer("pointer")?;
        let bucket_mask = val.assume_field_as_scalar_number("bucket_mask")?;

        let table = val.assume_field_as_struct("table")?;
        let kv_type = table
            .type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;
        let r#type = pcx.type_graph;
        let kv_size = r#type
            .type_size_in_bytes(pcx.evcx, kv_type)
            .ok_or_else(|| UnknownSize(r#type.identity(kv_type)))?;

        let reflection =
            HashmapReflection::new(ctrl as *mut u8, bucket_mask as usize, kv_size as usize);

        let iterator = reflection.iter(pcx.evcx.ecx.pid_on_focus())?;
        let mut items: Vec<Value> = iterator
            .map_err(ParsingError::from)
            .filter_map(|bucket| {
                let raw_data = bucket.read(pcx.evcx.ecx.pid_on_focus());
                let data = weak_error!(raw_data).map(|d| ObjectBinaryRepr {
                    raw_data: Bytes::from(d),
                    address: Some(bucket.location()),
                    size: bucket.size(),
                });

                let tuple = self.parser.parse_inner(pcx, data, kv_type);

                if let Some(Value::Struct(mut tuple)) = tuple
                    && tuple.members.len() == 2
                {
                    let _ = tuple.members.pop();
                    let k = tuple.members.pop().unwrap();
                    return Ok(Some(k.value));
                }

                Err(Assume(UnexpectedType("hashset bucket")))
            })
            .collect()?;

        // Phase 1 F3 — truncate at LEN_GUARD.
        let elided = if items.len() as i64 > LEN_GUARD {
            let n = (items.len() as i64 - LEN_GUARD) as u64;
            items.truncate(LEN_GUARD as usize);
            Some(n)
        } else {
            None
        };

        Ok(HashSetVariable {
            type_ident: val.r#type().to_owned(),
            items,
            elided,
        })
    }

    pub fn parse_btree_map(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        identity: TypeId,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_btree_map_inner(
                pcx,
                Value::Struct(structure.clone()),
                identity,
                type_params
            )
            .context("BTreeMap<K, V> interpretation")
        )
        .map(SpecializedValue::BTreeMap)
    }

    fn parse_btree_map_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
        identity: TypeId,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<HashMapVariable, ParsingError> {
        let height = val.assume_field_as_scalar_number("height")?;
        let ptr = val.assume_field_as_pointer("pointer")?;

        let k_type = type_params
            .get("K")
            .ok_or(TypeParameterNotFound("K"))?
            .ok_or(TypeParameterTypeNotFound("K"))?;
        let v_type = type_params
            .get("V")
            .ok_or(TypeParameterNotFound("V"))?
            .ok_or(TypeParameterTypeNotFound("V"))?;

        let reflection = BTreeReflection::new(
            pcx.type_graph,
            ptr,
            height as usize,
            identity,
            k_type,
            v_type,
        )?;
        let iterator = reflection.iter(pcx.evcx)?;
        let mut kv_items: Vec<(Value, Value)> = iterator
            .map_err(ParsingError::from)
            .filter_map(|(k, v)| {
                let Some(key) = self.parser.parse_inner(pcx, Some(k), k_type) else {
                    return Ok(None);
                };

                let Some(value) = self.parser.parse_inner(pcx, Some(v), v_type) else {
                    return Ok(None);
                };

                Ok(Some((key, value)))
            })
            .collect::<Vec<_>>()?;

        // Phase 1 F3 — apply LEN_GUARD to BTreeMap collection too.
        let elided = if kv_items.len() as i64 > LEN_GUARD {
            let n = (kv_items.len() as i64 - LEN_GUARD) as u64;
            kv_items.truncate(LEN_GUARD as usize);
            Some(n)
        } else {
            None
        };

        Ok(HashMapVariable {
            type_ident: val.r#type().to_owned(),
            kv_items,
            elided,
        })
    }

    pub fn parse_btree_set(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_btree_set_inner(Value::Struct(structure.clone()))
                .context("BTreeSet interpretation")
        )
        .map(SpecializedValue::BTreeSet)
    }

    fn parse_btree_set_inner(&self, val: Value) -> Result<HashSetVariable, ParsingError> {
        let inner_map = val
            .bfs_iterator()
            .find_map(|(field_or_idx, child)| {
                if let Value::Specialized {
                    value: Some(SpecializedValue::BTreeMap(map)),
                    ..
                } = child
                    && field_or_idx == FieldOrIndex::Field(Some("map"))
                {
                    return Some(map.clone());
                }
                None
            })
            .ok_or(IncompleteInterp("BTreeSet"))?;

        // Phase 1 F3 — BTreeSet inherits BTreeMap's truncation: the
        // inner map's `elided` field is already the right count
        // because BTreeSet just discards the values.
        Ok(HashSetVariable {
            type_ident: val.r#type().to_owned(),
            items: inner_map.kv_items.into_iter().map(|(k, _)| k).collect(),
            elided: inner_map.elided,
        })
    }

    pub fn parse_vec_dequeue(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_vec_dequeue_inner(pcx, Value::Struct(structure.clone()), type_params)
                .context("VeqDequeue<T> interpretation")
        )
        .map(SpecializedValue::VecDeque)
    }

    fn parse_vec_dequeue_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
        type_params: &IndexMap<String, Option<TypeId>>,
    ) -> Result<VecValue, ParsingError> {
        let inner_type = type_params
            .get("T")
            .ok_or(TypeParameterNotFound("T"))?
            .ok_or(TypeParameterTypeNotFound("T"))?;
        let len = val.assume_field_as_scalar_number("len")? as usize;
        let len = guard_len(len as i64) as usize;

        let r#type = pcx.type_graph;
        let el_type_size = r#type
            .type_size_in_bytes(pcx.evcx, inner_type)
            .ok_or_else(|| UnknownSize(r#type.identity(inner_type)))?
            as usize;
        let cap = if el_type_size == 0 {
            usize::MAX
        } else {
            guard_cap(extract_capacity(pcx, &val)? as i64) as usize
        };
        let head = val.assume_field_as_scalar_number("head")? as usize;

        let wrapped_start = if cap == 0 { 0 } else { head % cap };
        let head_len = cap - wrapped_start;

        let slice_ranges = if head_len >= len {
            (wrapped_start..wrapped_start + len, 0..0)
        } else {
            let tail_len = len - head_len;
            (wrapped_start..cap, 0..tail_len)
        };

        let data_ptr = val.assume_field_as_pointer("pointer")? as usize;

        let data =
            debugger::read_memory_by_pid(pcx.evcx.ecx.pid_on_focus(), data_ptr, cap * el_type_size)
                .map(Bytes::from)?;

        let items = slice_ranges
            .0
            .chain(slice_ranges.1)
            .enumerate()
            .filter_map(|(i, real_idx)| {
                let offset = real_idx * el_type_size;
                let el_raw_data = &data[offset..(real_idx + 1) * el_type_size];
                let el_data = ObjectBinaryRepr {
                    raw_data: data.slice_ref(el_raw_data),
                    address: Some(data_ptr + offset),
                    size: el_type_size,
                };

                Some(ArrayItem {
                    index: i as i64,
                    value: self.parser.parse_inner(pcx, Some(el_data), inner_type)?,
                })
            })
            .collect::<Vec<_>>();

        Ok(VecValue {
            structure: StructValue {
                type_id: None,
                type_ident: val.r#type().to_owned(),
                members: vec![
                    Member {
                        field_name: Some("buf".to_owned()),
                        value: Value::Array(ArrayValue {
                            type_id: None,
                            type_ident: pcx.type_graph.identity(inner_type).as_array_type(),
                            items: Some(items),
                            // set to `None` because the address operator unavailable for spec vars
                            raw_address: None,
                        }),
                    },
                    Member {
                        field_name: Some("cap".to_owned()),
                        value: Value::Scalar(ScalarValue {
                            type_id: None,
                            type_ident: TypeIdentity::no_namespace("usize"),
                            value: Some(SupportedScalar::Usize(if el_type_size == 0 {
                                0
                            } else {
                                cap
                            })),
                            // set to `None` because the address operator unavailable for spec vars
                            raw_address: None,
                        }),
                    },
                ],
                type_params: type_params.clone(),
                // set to `None` because the address operator unavailable for spec vars
                raw_address: None,
            },
            // VecDeque path: capacity-bound applied via guard_cap; the
            // `len` here is already accurate (no separate truncation
            // signal needed for now — the rendered Vec/VecDeque
            // already shows up to LEN_GUARD items either way).
            elided: None,
        })
    }

    pub fn parse_cell(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_cell_inner(Value::Struct(structure.clone()))
                .context("Cell<T> interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::Cell)
    }

    fn parse_cell_inner(&self, val: Value) -> Result<Value, ParsingError> {
        let unsafe_cell = val.assume_field_as_struct("value")?;
        let member = unsafe_cell
            .members
            .first()
            .ok_or(IncompleteInterp("UnsafeCell"))?;
        Ok(member.value.clone())
    }

    pub fn parse_refcell(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_refcell_inner(Value::Struct(structure.clone()))
                .context("RefCell<T> interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::RefCell)
    }

    fn parse_refcell_inner(&self, val: Value) -> Result<Value, ParsingError> {
        let borrow = val
            .bfs_iterator()
            .find_map(|(_, child)| {
                if let Value::Specialized {
                    value: Some(SpecializedValue::Cell(val)),
                    ..
                } = child
                {
                    return Some(val.clone());
                }
                None
            })
            .ok_or(IncompleteInterp("Cell"))?;
        let Value::Scalar(var) = *borrow else {
            return Err(IncompleteInterp("Cell").into());
        };
        let borrow = Value::Scalar(var);

        let unsafe_cell = val.assume_field_as_struct("value")?;
        let value = unsafe_cell
            .members
            .first()
            .ok_or(IncompleteInterp("UnsafeCell"))?;

        Ok(Value::Struct(StructValue {
            type_id: None,
            type_ident: val.r#type().to_owned(),
            members: vec![
                Member {
                    field_name: Some("borrow".to_string()),
                    value: borrow,
                },
                value.clone(),
            ],
            type_params: Default::default(),
            // set to `None` because the address operator unavailable for spec vars
            raw_address: None,
        }))
    }

    /// Phase 1 S15 — `Weak<T>` (rc / sync). Like `parse_rc` we BFS
    /// for the inner allocation pointer, but additionally deref it to
    /// read the `strong` and `weak` counters out of `RcBox` /
    /// `ArcInner`. Both shapes' counters live under members literally
    /// named `strong` and `weak`; for the `Rc` flavour they're
    /// wrapped in `Cell<usize>`, for the `Arc` flavour in
    /// `AtomicUsize` — by the time we read them through the
    /// dispatcher both have been peeled to a bare `usize`, so a BFS
    /// pluck works for either.
    pub fn parse_weak(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_weak_inner(pcx, Value::Struct(structure.clone()))
                .context("Weak<T> interpretation")
        )
    }

    fn parse_weak_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<SpecializedValue, ParsingError> {
        let ptr = val
            .bfs_iterator()
            .find_map(|(field_or_idx, child)| {
                if let Value::Pointer(p) = child
                    && field_or_idx == FieldOrIndex::Field(Some("pointer"))
                {
                    return Some(p.clone());
                }
                None
            })
            .ok_or(IncompleteInterp("Weak inner pointer"))?;

        // Deref the allocation. A freshly-constructed `Weak::new()`
        // points at libstd's `WEAK_EMPTY` sentinel; the read may
        // succeed and return zero counts, which is the right answer
        // anyway. If deref returns None (read failed) we surface
        // counts of zero so the renderer can still show the address
        // and a `[dropped]` annotation.
        let mut strong = 0u64;
        let mut weak = 0u64;
        if let Some(inner) = ptr.deref(pcx) {
            strong = read_named_usize(&inner, "strong").unwrap_or(0);
            weak = read_named_usize(&inner, "weak").unwrap_or(0);
        }

        Ok(SpecializedValue::Weak { ptr, strong, weak })
    }

    pub fn parse_rc(
        &self,
        pcx: &ParseContext,
        structure: &mut StructValue,
    ) -> Option<SpecializedValue> {
        let mut ptr = weak_error!(
            self.parse_rc_inner(Value::Struct(structure.clone()))
                .context("Rc<T> interpretation")
        )?;
        let bail = eager_deref_with_cycle_check(pcx, &mut ptr);
        if let Some(marker) = bail {
            // Propagate the marker to the outer struct's type_ident
            // — the renderer reads `original.type_ident` for the
            // `Specialized::Rc/Arc` arm.
            let original = structure.type_ident.name().unwrap_or("Rc").to_string();
            structure
                .type_ident
                .set_name(format!("{original} {marker}"));
        }
        // Leave `dereffed` pointing at the full `RcInner<T>` struct
        // — that way `value_children` can iterate `strong`, `weak`,
        // `value` and the user can open the tree. The top-level
        // value rendering peels to `value` at render time (see
        // `data.rs` `Wrapped` branch) so the inline display reads
        // `* "shared"` rather than `* RcInner<String> {...}`.
        Some(SpecializedValue::Rc(ptr))
    }

    fn parse_rc_inner(&self, val: Value) -> Result<PointerValue, ParsingError> {
        Ok(val
            .bfs_iterator()
            .find_map(|(field_or_idx, child)| {
                if let Value::Pointer(pointer) = child
                    && field_or_idx == FieldOrIndex::Field(Some("pointer"))
                {
                    let new_pointer = pointer.clone();
                    return Some(new_pointer);
                }
                None
            })
            .ok_or(IncompleteInterp("rc"))?)
    }

    pub fn parse_arc(
        &self,
        pcx: &ParseContext,
        structure: &mut StructValue,
    ) -> Option<SpecializedValue> {
        let mut ptr = weak_error!(
            self.parse_arc_inner(Value::Struct(structure.clone()))
                .context("Arc<T> interpretation")
        )?;
        let bail = eager_deref_with_cycle_check(pcx, &mut ptr);
        if let Some(marker) = bail {
            let original = structure.type_ident.name().unwrap_or("Arc").to_string();
            structure
                .type_ident
                .set_name(format!("{original} {marker}"));
        }
        // Same shape as Rc — leave dereffed pointing at the full
        // `ArcInner<T>` so child iteration works. Render-time
        // peels the `data` (or `value`) field for the inline
        // display.
        Some(SpecializedValue::Arc(ptr))
    }

    fn parse_arc_inner(&self, val: Value) -> Result<PointerValue, ParsingError> {
        Ok(val
            .bfs_iterator()
            .find_map(|(field_or_idx, child)| {
                if let Value::Pointer(pointer) = child
                    && field_or_idx == FieldOrIndex::Field(Some("pointer"))
                {
                    let new_pointer = pointer.clone();
                    return Some(new_pointer);
                }
                None
            })
            .ok_or(IncompleteInterp("Arc"))?)
    }

    /// Phase 1 S3 — `core::sync::atomic::Atomic*` peeling.
    ///
    /// Layout: every `AtomicX` is a single-field struct whose lone
    /// member is `UnsafeCell<X>`; `UnsafeCell` is itself a single-field
    /// struct. Field names vary across libstd versions and atomic
    /// variants (`v` vs `p`, `value` vs other), so we ignore names and
    /// peel positionally — `members[0].value[0]`. We never acquire any
    /// lock or fence; this may show torn state if another thread is
    /// mid-write, which is the expected behaviour for a debugger peek.
    pub fn parse_atomic(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_atomic_inner(Value::Struct(structure.clone()))
                .context("Atomic<T> interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::Atomic)
    }

    fn parse_atomic_inner(&self, val: Value) -> Result<Value, ParsingError> {
        let outer = match val {
            Value::Struct(s) => s,
            _ => return Err(UnexpectedType("Atomic<T> outer is not a struct").into()),
        };
        let inner_member = outer
            .members
            .into_iter()
            .next()
            .ok_or(IncompleteInterp("Atomic"))?;
        let Value::Struct(unsafe_cell) = inner_member.value else {
            return Err(UnexpectedType("Atomic<T> inner is not UnsafeCell<T>").into());
        };
        let value_member = unsafe_cell
            .members
            .into_iter()
            .next()
            .ok_or(IncompleteInterp("UnsafeCell"))?;
        Ok(value_member.value)
    }

    /// Phase 1 S2 — `MutexGuard` / `RwLockReadGuard` /
    /// `RwLockWriteGuard` peeling. Each guard's first member is a
    /// reference to its parent lock (`lock: &Mutex<T>` or
    /// `&RwLock<T>`). We BFS-find the first non-null pointer in the
    /// guard struct, deref it through `PointerValue::deref`, then
    /// look at the resulting Mutex/RwLock value: if it's already
    /// been spec'd by S1 we extract the inner T; otherwise we walk
    /// `data` → `UnsafeCell` → first-member as a fallback.
    pub fn parse_lock_guard(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_lock_guard_inner(pcx, Value::Struct(structure.clone()))
                .context("Mutex/RwLock guard interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::LockGuard)
    }

    fn parse_lock_guard_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<Value, ParsingError> {
        // Two layouts in libstd:
        //   MutexGuard / RwLockWriteGuard: `lock: &Mutex<T>` /
        //     `&RwLock<T>` — deref to get the parent, extract data.
        //   RwLockReadGuard: `data: NonNull<T>` — deref to T directly
        //     (no parent walk needed; the read guard borrows the
        //     reader-locked memory by raw pointer).
        // Try the read-guard shape first: a field literally named
        // `data` whose value is a NonNull or Pointer.
        let outer = match val {
            Value::Struct(s) => s,
            _ => return Err(UnexpectedType("guard outer is not a struct").into()),
        };
        if let Some(data) = outer
            .members
            .iter()
            .find(|m| m.field_name.as_deref() == Some("data"))
        {
            // RwLockReadGuard's `data` is a NonNull<T>; the BFS
            // recovers the inner *const T pointer.
            if let Some(ptr) = data
                .value
                .bfs_iterator()
                .find_map(|(_, child)| match child {
                    Value::Pointer(p) if p.value.is_some() => Some(p.clone()),
                    _ => None,
                })
                && let Some(inner) = ptr.deref(pcx)
            {
                return Ok(inner);
            }
        }
        // Fall back to the `lock` reference shape: BFS for the first
        // non-null pointer, deref it, and either accept the
        // already-spec'd Mutex inner or walk `data: UnsafeCell<T>`
        // manually.
        let lock_ptr = Value::Struct(outer.clone())
            .bfs_iterator()
            .find_map(|(_, child)| match child {
                Value::Pointer(p) if p.value.is_some() => Some(p.clone()),
                _ => None,
            })
            .ok_or(IncompleteInterp("guard lock pointer"))?;
        let parent = lock_ptr
            .deref(pcx)
            .ok_or(IncompleteInterp("guard parent lock deref"))?;
        match parent {
            Value::Specialized {
                value: Some(SpecializedValue::Mutex { inner, .. }),
                ..
            } => Ok(*inner),
            Value::Struct(parent_struct) => {
                let data_member = parent_struct
                    .members
                    .into_iter()
                    .find(|m| m.field_name.as_deref() == Some("data"))
                    .ok_or(FieldNotFound("data"))?;
                let Value::Struct(unsafe_cell) = data_member.value else {
                    return Err(UnexpectedType("guard parent data is not UnsafeCell").into());
                };
                let inner = unsafe_cell
                    .members
                    .into_iter()
                    .next()
                    .ok_or(IncompleteInterp("UnsafeCell"))?;
                Ok(inner.value)
            }
            _ => Err(UnexpectedType("guard parent unexpected shape").into()),
        }
    }

    /// Phase 1 S1 — `Mutex<T>` / `RwLock<T>` peeling. Both have a
    /// `data: UnsafeCell<T>` field; we extract by name then peel
    /// UnsafeCell's first member (any name — libstd has used `value`
    /// historically). We also read the `poison: poison::Flag` field
    /// (an `AtomicBool` peeled by S3) and surface a `[poisoned]`
    /// trailer when set. The `inner: sys::Mutex` field stays opaque;
    /// the `[locked]` badge is still deferred (lock-state requires
    /// platform-specific layout knowledge).
    pub fn parse_mutex(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_mutex_inner(pcx, Value::Struct(structure.clone()))
                .context("Mutex<T> / RwLock<T> interpretation")
        )
    }

    fn parse_mutex_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<SpecializedValue, ParsingError> {
        let outer = match val {
            Value::Struct(s) => s,
            _ => return Err(UnexpectedType("Mutex outer is not a struct").into()),
        };
        // Phase 1 S1 (poison) — find the `poison` member; BFS for
        // its inner bool. Defaults to false when the field is absent
        // (older libstd revisions had `poison` differently named).
        let poisoned = outer
            .members
            .iter()
            .find(|m| m.field_name.as_deref() == Some("poison"))
            .and_then(|m| {
                m.value.bfs_iterator().find_map(|(_, child)| match child {
                    Value::Scalar(s) => match s.value {
                        Some(SupportedScalar::Bool(b)) => Some(b),
                        _ => None,
                    },
                    _ => None,
                })
            })
            .unwrap_or(false);
        // Phase 1 S1 (locked) — futex backend only. Mutex stores
        // `inner: sys::Mutex { futex: SmallFutex }` and RwLock stores
        // `inner: sys::RwLock { state: Futex, writer_notify: Futex }`.
        // The futex itself is an atomic u32 (peeled by S3). Locked
        // iff the first u32 in the inner member is non-zero. Other
        // backends (pthread on macOS, SRWLOCK on Win7) don't have
        // the `futex`/`state` field so this defaults to false.
        let inner_member = outer
            .members
            .iter()
            .find(|m| m.field_name.as_deref() == Some("inner"));
        let mut locked = inner_member
            .and_then(|m| {
                m.value.bfs_iterator().find_map(|(_, child)| match child {
                    Value::Scalar(s) => match s.value {
                        Some(SupportedScalar::U32(v)) => Some(v != 0),
                        _ => None,
                    },
                    _ => None,
                })
            })
            .unwrap_or(false);

        // macOS Mutex/RwLock raw-byte probe DISABLED for now. The
        // earlier attempt (reading bytes 8..12 of the `inner` field's
        // runtime address) coincided with a debug-session crash in
        // showcase when `let captured_copy = 10;` was uncommented, so
        // the probe is parked while we bisect the actual culprit. Once
        // the kill is rooted out, restore by reading 4 bytes at offset
        // 8 (os_unfair_lock owner word) and setting locked = nonzero.
        // See the git history at this file for the previous shape.
        let _ = (pcx, inner_member); // silence unused-variable warnings
        // `data` is the only field we care about; the `inner` lock
        // primitive and `poison` flag are ignored.
        let data_member = outer
            .members
            .into_iter()
            .find(|m| m.field_name.as_deref() == Some("data"))
            .ok_or(FieldNotFound("data"))?;
        let Value::Struct(unsafe_cell) = data_member.value else {
            return Err(UnexpectedType("Mutex data is not UnsafeCell<T>").into());
        };
        // UnsafeCell<T>'s only member is the T payload; peel
        // positionally so we don't depend on libstd's field name.
        let inner = unsafe_cell
            .members
            .into_iter()
            .next()
            .ok_or(IncompleteInterp("UnsafeCell"))?;
        Ok(SpecializedValue::Mutex {
            inner: Box::new(inner.value),
            poisoned,
            locked,
        })
    }

    /// Phase 1 S10 — `core::mem::MaybeUninit<T>` peeling. Layout is
    /// `union { value: ManuallyDrop<T>, uninit: () }`. The `uninit`
    /// arm has no payload, so we always pick the `value` arm — peel
    /// `ManuallyDrop`'s inner `value` field to get the underlying
    /// `T`. The renderer adds a `[possibly uninit]` trailer.
    pub fn parse_maybe_uninit(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_maybe_uninit_inner(Value::Struct(structure.clone()))
                .context("MaybeUninit<T> interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::MaybeUninit)
    }

    fn parse_maybe_uninit_inner(&self, val: Value) -> Result<Value, ParsingError> {
        // The Union has been parsed as a struct; find the `value`
        // member (the `ManuallyDrop<T>` arm) and peel one more layer
        // to get T. ManuallyDrop is `#[repr(transparent)]` and its
        // single field is also called `value` in stable libcore.
        let outer = match val {
            Value::Struct(s) => s,
            _ => return Err(UnexpectedType("MaybeUninit outer is not a struct").into()),
        };
        let value_member = outer
            .members
            .into_iter()
            .find(|m| m.field_name.as_deref() == Some("value"))
            .ok_or(FieldNotFound("value"))?;
        // ManuallyDrop is transparent — its inner field may or may
        // not appear in DWARF. If we see an inner struct, peel it.
        match value_member.value {
            Value::Struct(inner) => {
                if let Some(first) = inner.members.into_iter().next() {
                    Ok(first.value)
                } else {
                    Err(IncompleteInterp("ManuallyDrop").into())
                }
            }
            other => Ok(other),
        }
    }

    /// Phase 1 S13/S14 — `OsString` / `PathBuf` peeling. Both wrap a
    /// `Vec<u8>` on unix (the `Buf { inner: Vec<u8> }` →
    /// `OsString { inner: Buf }` → `PathBuf { inner: OsString }`
    /// chain on macOS / Linux). We BFS-find the first `usize` length
    /// and the first non-null pointer (the `Vec`'s data pointer at
    /// the bottom of the chain), read up to `min(LEN_GUARD, MAX_READ)`
    /// bytes, then attempt utf-8. On success render as plain
    /// double-quoted string; on failure use a hex preview prefixed
    /// `b"`. There is no trailing-NUL stripping (unlike `CString`).
    pub fn parse_os_string(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_os_string_inner(pcx, Value::Struct(structure.clone()))
                .context("OsString interpretation")
        )
        .map(SpecializedValue::OsString)
    }

    fn parse_os_string_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<StringVariable, ParsingError> {
        let mut found_len: Option<i64> = None;
        let mut found_ptr: Option<*const ()> = None;
        for (_, child) in val.bfs_iterator() {
            if found_len.is_none()
                && let Value::Scalar(s) = child
                && let Some(SupportedScalar::Usize(n)) = s.value
            {
                found_len = Some(n as i64);
                continue;
            }
            if found_ptr.is_none()
                && let Value::Pointer(p) = child
                && let Some(addr) = p.value
            {
                found_ptr = Some(addr);
            }
            if found_len.is_some() && found_ptr.is_some() {
                break;
            }
        }
        let len = found_len.ok_or(IncompleteInterp("OsString length"))?;
        let ptr = found_ptr.ok_or(IncompleteInterp("OsString data pointer"))?;
        const MAX_READ: i64 = 64 * 1024;
        let len = guard_len(len).min(MAX_READ);
        let bytes =
            debugger::read_memory_by_pid(pcx.evcx.ecx.pid_on_focus(), ptr as usize, len as usize)?;
        let display = match std::str::from_utf8(&bytes) {
            Ok(s) => format!("{:?}", s),
            Err(_) => {
                let mut out = String::from("b\"");
                for b in bytes.iter().take(32) {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                if bytes.len() > 32 {
                    out.push_str(" …");
                }
                out.push('"');
                out
            }
        };
        Ok(StringVariable {
            value: display,
            elided: None,
        })
    }

    /// Phase 1 S12 — `alloc::ffi::c_str::CString`. Layout is
    /// `CString { inner: Box<[u8]> }`; the `Box<[u8]>` is a fat
    /// pointer `(data_ptr: *const u8, len: usize)` with the trailing
    /// NUL byte included in `len`. We BFS-find the data pointer and
    /// length, read up to 64 KiB to bound corrupted-pointer reads,
    /// strip the trailing NUL, then try utf-8. On success we render
    /// `c"hello"`; on failure we render a hex preview prefixed `c"\\x..`.
    pub fn parse_cstring(
        &self,
        pcx: &ParseContext,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_cstring_inner(pcx, Value::Struct(structure.clone()))
                .context("CString interpretation")
        )
        .map(SpecializedValue::CString)
    }

    fn parse_cstring_inner(
        &self,
        pcx: &ParseContext,
        val: Value,
    ) -> Result<StringVariable, ParsingError> {
        // The fat pointer's two fields aren't always named at the
        // `CString` level (the wrapper hides them via `Box<[u8]>`),
        // so BFS for the first usize length and the first non-null
        // pointer-shaped child.
        let mut found_len: Option<i64> = None;
        let mut found_ptr: Option<*const ()> = None;
        for (_, child) in val.bfs_iterator() {
            if found_len.is_none()
                && let Value::Scalar(s) = child
                && let Some(SupportedScalar::Usize(n)) = s.value
            {
                found_len = Some(n as i64);
                continue;
            }
            if found_ptr.is_none()
                && let Value::Pointer(p) = child
                && let Some(addr) = p.value
            {
                found_ptr = Some(addr);
            }
            if found_len.is_some() && found_ptr.is_some() {
                break;
            }
        }
        let len = found_len.ok_or(IncompleteInterp("CString length"))?;
        let ptr = found_ptr.ok_or(IncompleteInterp("CString data pointer"))?;
        // Bound the read to 64 KiB to limit damage from a corrupted
        // length field. The plan calls this out explicitly.
        const MAX_READ: i64 = 64 * 1024;
        let len = guard_len(len).min(MAX_READ);
        let mut bytes =
            debugger::read_memory_by_pid(pcx.evcx.ecx.pid_on_focus(), ptr as usize, len as usize)?;
        // CString invariants guarantee a trailing NUL. Strip it before
        // attempting utf-8 decode.
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        let display = match std::str::from_utf8(&bytes) {
            Ok(s) => format!("c{:?}", s),
            Err(_) => {
                // Hex preview, capped at 32 bytes for readability.
                let mut out = String::from("c\"");
                for b in bytes.iter().take(32) {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                if bytes.len() > 32 {
                    out.push_str(" …");
                }
                out.push('"');
                out
            }
        };
        Ok(StringVariable {
            value: display,
            elided: None,
        })
    }

    /// Phase 1 S4 — `core::time::Duration` peeling. Layout is
    /// `Duration { secs: u64, nanos: Nanoseconds }` where
    /// `Nanoseconds` is a single-field tuple struct around `u32`. We
    /// read the outer `secs` field directly and BFS-find the inner
    /// `u32` for nanos so we don't depend on the wrapper's field name
    /// (which has changed across libcore revisions).
    pub fn parse_duration(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_duration_inner(structure)
                .context("Duration interpretation")
        )
        .map(SpecializedValue::Duration)
    }

    fn parse_duration_inner(&self, structure: &StructValue) -> Result<(u64, u32), ParsingError> {
        // `secs` is a top-level u64 field on the Duration struct.
        let secs = structure
            .members
            .iter()
            .find(|m| m.field_name.as_deref() == Some("secs"))
            .and_then(|m| match &m.value {
                Value::Scalar(s) => match s.value {
                    Some(SupportedScalar::U64(v)) => Some(v),
                    _ => None,
                },
                _ => None,
            })
            .ok_or(FieldNotFound("secs"))?;
        // `nanos` is a Nanoseconds(u32) wrapper; descend any depth to
        // find the first u32 scalar under the `nanos` member.
        let nanos_member = structure
            .members
            .iter()
            .find(|m| m.field_name.as_deref() == Some("nanos"))
            .ok_or(FieldNotFound("nanos"))?;
        let nanos = nanos_member
            .value
            .bfs_iterator()
            .find_map(|(_, child)| match child {
                Value::Scalar(s) => match s.value {
                    Some(SupportedScalar::U32(v)) => Some(v),
                    _ => None,
                },
                _ => None,
            })
            .ok_or(IncompleteInterp("Duration nanos"))?;
        Ok((secs, nanos))
    }

    /// Phase 1 S6 — `core::ops::Range*` peeling. Variant is decided
    /// from the struct name (`Range`, `RangeInclusive`, `RangeFrom`,
    /// `RangeTo`, `RangeToInclusive`, `RangeFull`); we read `start` /
    /// `end` fields by name. For `RangeInclusive`, the private
    /// `exhausted: bool` is read when present (libcore has renamed /
    /// gated this field across versions — we tolerate absence and
    /// default to `false`).
    pub fn parse_range(
        &self,
        struct_name: &str,
        structure: &StructValue,
    ) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_range_inner(struct_name, structure)
                .context("Range* interpretation")
        )
        .map(SpecializedValue::Range)
    }

    fn parse_range_inner(
        &self,
        struct_name: &str,
        structure: &StructValue,
    ) -> Result<RangeValue, ParsingError> {
        let find = |field: &'static str| -> Option<Value> {
            structure
                .members
                .iter()
                .find(|m| m.field_name.as_deref() == Some(field))
                .map(|m| m.value.clone())
        };
        let take = |field: &'static str| -> Result<Box<Value>, ParsingError> {
            find(field).map(Box::new).ok_or(FieldNotFound(field).into())
        };
        // Match prefix so we accept e.g. `Range<i32>` and bare `Range`.
        Ok(if struct_name.starts_with("RangeInclusive") {
            let exhausted = match find("exhausted") {
                Some(Value::Scalar(s)) => matches!(s.value, Some(SupportedScalar::Bool(true))),
                _ => false,
            };
            RangeValue::Inclusive {
                start: take("start")?,
                end: take("end")?,
                exhausted,
            }
        } else if struct_name.starts_with("RangeFrom") {
            RangeValue::From {
                start: take("start")?,
            }
        } else if struct_name.starts_with("RangeToInclusive") {
            RangeValue::ToInclusive { end: take("end")? }
        } else if struct_name.starts_with("RangeTo") {
            RangeValue::To { end: take("end")? }
        } else if struct_name.starts_with("RangeFull") {
            RangeValue::Full
        } else {
            RangeValue::Half {
                start: take("start")?,
                end: take("end")?,
            }
        })
    }

    /// Phase 1 S7 — `core::pin::Pin<P>` peeling. Layout is a tuple
    /// struct `Pin { __pointer: P }` (the field name has changed
    /// across libcore revisions; we peel positionally to avoid a
    /// version-switch). The inner `P` (a reference, `Box`, `Rc`, or
    /// any user `Deref` impl) is surfaced directly.
    pub fn parse_pin(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_pin_inner(Value::Struct(structure.clone()))
                .context("Pin<P> interpretation")
        )
        .map(Box::new)
        .map(SpecializedValue::Pin)
    }

    fn parse_pin_inner(&self, val: Value) -> Result<Value, ParsingError> {
        let outer = match val {
            Value::Struct(s) => s,
            _ => return Err(UnexpectedType("Pin<P> outer is not a struct").into()),
        };
        let inner = outer
            .members
            .into_iter()
            .next()
            .ok_or(IncompleteInterp("Pin"))?;
        Ok(inner.value)
    }

    /// Phase 1 S11 — `core::ptr::NonNull<T>` peeling. Layout is
    /// `NonNull { pointer: *const T }`; we extract the inner
    /// `pointer` field and surface it as a plain `*T`.
    pub fn parse_nonnull(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_nonnull_inner(Value::Struct(structure.clone()))
                .context("NonNull<T> interpretation")
        )
        .map(SpecializedValue::NonNull)
    }

    fn parse_nonnull_inner(&self, val: Value) -> Result<PointerValue, ParsingError> {
        Ok(val
            .bfs_iterator()
            .find_map(|(field_or_idx, child)| {
                if let Value::Pointer(pointer) = child
                    && field_or_idx == FieldOrIndex::Field(Some("pointer"))
                {
                    return Some(pointer.clone());
                }
                None
            })
            .ok_or(IncompleteInterp("NonNull"))?)
    }

    pub fn parse_uuid(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_uuid_inner(structure)
                .context("Uuid interpretation")
        )
        .map(SpecializedValue::Uuid)
    }

    fn parse_uuid_inner(&self, structure: &StructValue) -> Result<[u8; 16], ParsingError> {
        let member0 = structure.members.first().ok_or(FieldNotFound("member 0"))?;
        let Value::Array(ref arr) = member0.value else {
            return Err(UnexpectedType("uuid struct member must be an array").into());
        };
        let items = arr
            .items
            .as_ref()
            .ok_or(AssumeError::NoData("uuid items"))?;
        if items.len() != 16 {
            return Err(AssumeError::UnexpectedType("uuid struct member must be [u8; 16]").into());
        }

        let mut bytes_repr = [0; 16];
        for (i, item) in items.iter().enumerate() {
            let Value::Scalar(ScalarValue {
                value: Some(SupportedScalar::U8(byte)),
                ..
            }) = item.value
            else {
                return Err(UnexpectedType("uuid struct member must be [u8; 16]").into());
            };
            bytes_repr[i] = byte;
        }

        Ok(bytes_repr)
    }

    fn parse_timespec(&self, timespec: &StructValue) -> Result<(i64, u32), ParsingError> {
        let &[
            Member {
                value: Value::Scalar(secs),
                ..
            },
            Member {
                value: Value::Struct(n_secs),
                ..
            },
        ] = &timespec.members.as_slice()
        else {
            let err = "`Timespec` should contains secs and n_secs fields";
            return Err(UnexpectedType(err).into());
        };

        let &[
            Member {
                value: Value::Scalar(n_secs),
                ..
            },
        ] = &n_secs.members.as_slice()
        else {
            let err = "`Nanoseconds` should contains u32 field";
            return Err(UnexpectedType(err).into());
        };

        let secs = secs
            .try_as_number()
            .ok_or(UnexpectedType("`Timespec::tv_sec` not an int"))?;
        let n_secs = n_secs
            .try_as_number()
            .ok_or(UnexpectedType("Timespec::tv_nsec` not an int"))? as u32;

        Ok((secs, n_secs))
    }

    pub fn parse_sys_time(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_sys_time_inner(structure)
                .context("SystemTime interpretation")
        )
        .map(SpecializedValue::SystemTime)
    }

    fn parse_sys_time_inner(&self, structure: &StructValue) -> Result<(i64, u32), ParsingError> {
        let &[
            Member {
                value: Value::Struct(time_instant),
                ..
            },
        ] = &structure.members.as_slice()
        else {
            let err = "`std::time::SystemTime` should contains a `time::SystemTime` field";
            return Err(UnexpectedType(err).into());
        };

        let &[
            Member {
                value: Value::Struct(timespec),
                ..
            },
        ] = &time_instant.members.as_slice()
        else {
            let err = "`time::SystemTime` should contains a `Timespec` field";
            return Err(UnexpectedType(err).into());
        };

        self.parse_timespec(timespec)
    }

    pub fn parse_instant(&self, structure: &StructValue) -> Option<SpecializedValue> {
        weak_error!(
            self.parse_instant_inner(structure)
                .context("Instant interpretation")
        )
        .map(SpecializedValue::Instant)
    }

    fn parse_instant_inner(&self, structure: &StructValue) -> Result<(i64, u32), ParsingError> {
        let &[
            Member {
                value: Value::Struct(time_instant),
                ..
            },
        ] = &structure.members.as_slice()
        else {
            let err = "`std::time::Instant` should contains a `time::Instant` field";
            return Err(UnexpectedType(err).into());
        };

        let &[
            Member {
                value: Value::Struct(timespec),
                ..
            },
        ] = &time_instant.members.as_slice()
        else {
            let err = "`time::Instant` should contains a `Timespec` field";
            return Err(UnexpectedType(err).into());
        };

        self.parse_timespec(timespec)
    }
}

fn extract_capacity(pcx: &ParseContext, val: &Value) -> Result<usize, ParsingError> {
    let rust_version = pcx
        .evcx
        .rustc_version()
        .ok_or(ParsingError::UnsupportedVersion)?;

    version_switch!(
    rust_version,
    .. (1 . 76) => val.assume_field_as_scalar_number("cap")? as usize,
    (1 . 76) .. => {
            let cap_s = val.assume_field_as_struct("cap")?;
            let cap = &cap_s.members.first().ok_or(IncompleteInterp("Vec"))?.value;
            if let Value::Scalar(ScalarValue {value: Some(SupportedScalar::Usize(cap)), ..}) = cap {
                Ok(*cap)
            } else {
                Err(AssumeError::FieldNotANumber("cap"))
            }?
        },
    )
    .ok_or(ParsingError::UnsupportedVersion)
}

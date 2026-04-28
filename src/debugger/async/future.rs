// SPDX-License-Identifier: MIT
use crate::debugger::address::RelocatedAddress;
use crate::debugger::r#async::AsyncError;
use crate::debugger::debugee::dwarf::r#type::TypeIdentity;
use crate::debugger::variable::value::{RustEnumValue, SpecializedValue, StructValue, Value};
use std::num::ParseIntError;

#[derive(Debug, thiserror::Error)]
pub enum ParseFutureStateError {
    #[error("unexpected future structure representation")]
    UnexpectedStructureRepr,
    #[error("parse suspend state: {0}")]
    ParseSuspendState(ParseIntError),
    #[error("unexpected future state: {0}")]
    UnexpectedState(String),
}

#[derive(Debug, Clone)]
pub enum AsyncFnFutureState {
    /// A future in this state is suspended at the await point in the code.
    /// The compiler generates a special type to indicate a stop at such await point -
    /// `SuspendX` where X is an integer number of such a point.
    Suspend(u32),
    /// The state of async fn that has been panicked on a previous poll.
    Panicked,
    /// Already resolved async fn. In other words, this future has been
    /// polled and returned Poll::Ready(result) from the poll function.
    Returned,
    /// Already created async fn future but not yet polled (using await or select! or any other
    /// async operation).
    Unresumed,
    /// Future already in a completed state.
    Ok,
}

#[derive(Debug, Clone)]
pub struct AsyncFnFuture {
    /// Future name (from debug info).
    pub name: String,
    /// Async function name.
    pub async_fn: String,
    /// Async function state.
    pub state: AsyncFnFutureState,
    /// Phase 3 Feature D — `(file, line)` of the `.await` the future
    /// is suspended at, recovered from `DW_AT_decl_file`/
    /// `DW_AT_decl_line` on the active variant's captured-locals
    /// fields. `None` when the variant has no decl coords (e.g.
    /// `Unresumed`/`Returned`/`Panicked`/`Ok`, or a stripped binary).
    pub await_location: Option<(std::path::PathBuf, u64)>,
}

impl TryFrom<&RustEnumValue> for AsyncFnFuture {
    type Error = AsyncError;

    fn try_from(repr: &RustEnumValue) -> Result<Self, Self::Error> {
        const UNRESUMED_STATE: &str = "Unresumed";
        const RETURNED_STATE: &str = "Returned";
        const PANICKED_STATE: &str = "Panicked";
        const SUSPEND_STATE: &str = "Suspend";
        const OK_STATE: &str = "Ok";

        let async_fn = repr
            .type_ident
            .namespace()
            .as_parts()
            .join("::")
            .to_string();
        let name = repr.type_ident.name_fmt().to_string();

        let Some(Value::Struct(state)) = repr.value.as_deref().map(|m| &m.value) else {
            return Err(AsyncError::ParseFutureState(
                ParseFutureStateError::UnexpectedStructureRepr,
            ));
        };

        let state = match state.type_ident.name_fmt() {
            UNRESUMED_STATE => Ok(AsyncFnFutureState::Unresumed),
            RETURNED_STATE => Ok(AsyncFnFutureState::Returned),
            PANICKED_STATE => Ok(AsyncFnFutureState::Panicked),
            OK_STATE => Ok(AsyncFnFutureState::Ok),
            str => {
                if str.starts_with(SUSPEND_STATE) {
                    let str = str.trim_start_matches(SUSPEND_STATE);
                    let num: u32 = str.parse().map_err(|e| {
                        AsyncError::ParseFutureState(ParseFutureStateError::ParseSuspendState(e))
                    })?;
                    Ok(AsyncFnFutureState::Suspend(num))
                } else {
                    return Err(AsyncError::ParseFutureState(
                        ParseFutureStateError::UnexpectedState(str.to_string()),
                    ));
                }
            }
        }?;

        Ok(Self {
            async_fn,
            name,
            state,
            await_location: repr.await_location.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct CustomFuture {
    pub name: TypeIdentity,
    /// Phase 3 Feature D batch D2b — when the awaitee is a
    /// `Pin<Box<dyn Future>>` / `Box<dyn Future>` / `&dyn Future`,
    /// Phase 3A's vtable resolver annotates the inner fat-pointer
    /// struct's `type_ident` with the recovered `[→ Concrete]` tag.
    /// We surface that string so the await-trace shows the concrete
    /// future type even when the static type is `dyn`.
    pub concrete: Option<String>,
}

impl From<&StructValue> for CustomFuture {
    fn from(repr: &StructValue) -> Self {
        let name = repr.type_ident.clone();
        let concrete = find_trait_object_concrete(&Value::Struct(repr.clone()), 4);
        Self { name, concrete }
    }
}

/// Walk a value tree looking for the canonical `dyn Trait`
/// fat-pointer struct (a two-member struct with `pointer` and
/// `vtable` fields) and return its (possibly Phase-3A-annotated)
/// display name. Bounded by `depth` to dodge pathological nesting.
///
/// `StructValue::is_trait_object()` also matches by name (anything
/// containing `"dyn "`), which trips on outer wrappers like
/// `Pin<Box<dyn Future>>`. We deliberately use only the structural
/// check here so we always reach the innermost fat pointer — that's
/// where Phase 3A spliced the `[→ Concrete]` annotation.
fn find_trait_object_concrete(val: &Value, depth: u32) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let Value::Struct(s) = val else {
        return None;
    };
    if has_fat_pointer_shape(s) {
        return s.type_ident.name().map(str::to_string);
    }
    for m in &s.members {
        if let Some(r) = find_trait_object_concrete(&m.value, depth - 1) {
            return Some(r);
        }
    }
    None
}

fn has_fat_pointer_shape(s: &StructValue) -> bool {
    if s.members.len() != 2 {
        return false;
    }
    let m0 = s.members[0].field_name.as_deref();
    let m1 = s.members[1].field_name.as_deref();
    matches!(
        (m0, m1),
        (Some("pointer"), Some("vtable"))
            | (Some("data_ptr"), Some("vtable"))
            | (Some("vtable"), Some("pointer"))
            | (Some("vtable"), Some("data_ptr"))
    )
}

#[derive(Debug, Clone)]
pub struct TokioSleepFuture {
    pub name: TypeIdentity,
    pub instant: (i64, u32),
}

impl TryFrom<StructValue> for TokioSleepFuture {
    type Error = AsyncError;

    fn try_from(val: StructValue) -> Result<Self, Self::Error> {
        let name = val.type_ident.clone();

        let Some(Value::Struct(entry)) = val.field("entry") else {
            return Err(AsyncError::IncorrectAssumption(
                "Sleep future should contains `entry` field",
            ));
        };

        let Some(Value::Struct(deadline)) = entry.field("deadline") else {
            return Err(AsyncError::IncorrectAssumption(
                "Sleep future should contains `entry.deadline` field",
            ));
        };

        let Some(Value::Specialized {
            value: Some(SpecializedValue::Instant(instant)),
            ..
        }) = deadline.field("std")
        else {
            return Err(AsyncError::IncorrectAssumption(
                "Sleep future should contains `entry.deadline.std` field",
            ));
        };

        Ok(Self { name, instant })
    }
}

#[derive(Debug, Clone)]
pub struct TokioJoinHandleFuture {
    pub name: TypeIdentity,
    pub wait_for_task: RelocatedAddress,
}

impl TryFrom<StructValue> for TokioJoinHandleFuture {
    type Error = AsyncError;

    fn try_from(val: StructValue) -> Result<Self, Self::Error> {
        let name = val.type_ident.clone();

        let header_field = val
            .field("raw")
            .and_then(|raw| raw.field("ptr")?.field("pointer"));
        let Some(header) = header_field else {
            return Err(AsyncError::IncorrectAssumption(
                "JoinHandle future should contains `raw` field",
            ));
        };

        let Value::Pointer(ref ptr) = header else {
            return Err(AsyncError::IncorrectAssumption(
                "JoinHandle::raw.ptr.pointer not a pointer",
            ));
        };
        let wait_for_task = ptr
            .value
            .map(|p| RelocatedAddress::from(p as usize))
            .ok_or(AsyncError::IncorrectAssumption(
                "JoinHandle::raw.ptr.pointer not a pointer",
            ))?;

        Ok(Self {
            name,
            wait_for_task,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Future {
    AsyncFn(AsyncFnFuture),
    TokioSleep(TokioSleepFuture),
    TokioJoinHandleFuture(TokioJoinHandleFuture),
    Custom(CustomFuture),
    /// Phase 3 Feature D step 5 — parallel awaitee branches.
    ///
    /// Emitted when the active variant's inner struct carries
    /// multiple coroutine-shaped fields rather than a single
    /// `__awaitee`. Each entry in `branches` is the chain rooted at
    /// one of those parallel futures (rendered as a sub-trace). The
    /// canonical caller is `tokio::join!` / `tokio::select!`-style
    /// shapes, but the detector is generic — any struct that
    /// captures 2+ futures triggers it.
    Multi(Vec<Vec<Future>>),
    UnknownFuture,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debugger::variable::value::Member;

    fn dyn_struct(name: &str) -> StructValue {
        // Two-member fat pointer with the canonical pointer/vtable
        // shape — `is_trait_object()` returns true for this.
        StructValue {
            type_ident: TypeIdentity::no_namespace(name),
            type_id: None,
            members: vec![
                Member {
                    field_name: Some("pointer".to_string()),
                    value: Value::CEnum(crate::debugger::variable::value::CEnumValue {
                        type_ident: TypeIdentity::unknown(),
                        type_id: None,
                        value: None,
                        raw_address: None,
                    }),
                },
                Member {
                    field_name: Some("vtable".to_string()),
                    value: Value::CEnum(crate::debugger::variable::value::CEnumValue {
                        type_ident: TypeIdentity::unknown(),
                        type_id: None,
                        value: None,
                        raw_address: None,
                    }),
                },
            ],
            type_params: Default::default(),
            raw_address: None,
        }
    }

    fn wrap(outer: &str, inner: StructValue) -> StructValue {
        StructValue {
            type_ident: TypeIdentity::no_namespace(outer),
            type_id: None,
            members: vec![Member {
                field_name: Some("__0".to_string()),
                value: Value::Struct(inner),
            }],
            type_params: Default::default(),
            raw_address: None,
        }
    }

    #[test]
    fn finds_trait_object_at_top_level() {
        let s = dyn_struct("Box<dyn Future> [→ MyFuture]");
        assert_eq!(
            find_trait_object_concrete(&Value::Struct(s), 4).as_deref(),
            Some("Box<dyn Future> [→ MyFuture]"),
        );
    }

    #[test]
    fn finds_trait_object_nested_in_pin() {
        let inner = dyn_struct("Box<dyn Future> [→ MyConcreteFuture]");
        let pin = wrap("Pin<Box<dyn Future>>", inner);
        assert_eq!(
            find_trait_object_concrete(&Value::Struct(pin), 4).as_deref(),
            Some("Box<dyn Future> [→ MyConcreteFuture]"),
        );
    }

    #[test]
    fn returns_none_for_plain_struct() {
        let s = StructValue {
            type_ident: TypeIdentity::no_namespace("MyStruct"),
            type_id: None,
            members: vec![],
            type_params: Default::default(),
            raw_address: None,
        };
        assert!(find_trait_object_concrete(&Value::Struct(s), 4).is_none());
    }

    #[test]
    fn depth_cap_protects_against_pathological_nesting() {
        // 6 levels deep, cap = 3 → walker bails before reaching the
        // trait object.
        let mut s = dyn_struct("inner [→ X]");
        for _ in 0..5 {
            s = wrap("Wrap", s);
        }
        assert!(find_trait_object_concrete(&Value::Struct(s), 3).is_none());
    }
}

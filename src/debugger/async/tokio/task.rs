// SPDX-License-Identifier: MIT
use super::{AsyncError, Future, TaskBacktrace, types};
use crate::{
    debugger::{
        Debugger, Error,
        address::RelocatedAddress,
        r#async::future::{AsyncFnFuture, CustomFuture, TokioJoinHandleFuture, TokioSleepFuture},
        context::gcx,
        debugee::dwarf::unit::die::Die,
        utils::PopIf,
        variable::{
            dqe::{Dqe, PointerCast},
            execute::QueryResult,
            value::{RustEnumValue, Value},
        },
    },
    resolve_unit_call, weak_error,
};
use core::str;

pub struct Task {
    pub id: u64,
    repr: RustEnumValue,
    raw_ptr: RelocatedAddress,
}

impl Task {
    pub fn from_enum_repr(raw_ptr: RelocatedAddress, id: u64, repr: RustEnumValue) -> Self {
        Self { raw_ptr, id, repr }
    }

    pub fn backtrace(self, debugger: &Debugger) -> Result<TaskBacktrace, AsyncError> {
        Ok(TaskBacktrace {
            task_id: self.id,
            raw_ptr: self.raw_ptr,
            futures: build_chain_from_repr(self.repr, Some(debugger)),
        })
    }
}

const AWAITEE_FIELD: &str = "__awaitee";
/// Phase 3 Feature D step 5 — depth cap on the recursive
/// chain-builder. Each `Multi` branch counts as a recursion step;
/// pathological nesting bails out with a leaf `UnknownFuture`.
const MAX_BRANCH_DEPTH: u32 = 8;

/// Build the linear future chain starting from a coroutine
/// state-machine [`RustEnumValue`]. Walks `__awaitee` for the
/// single-active-future case and emits a [`Future::Multi`] branch
/// when the active variant carries 2+ coroutine-shaped fields
/// (`tokio::join!` / `tokio::select!`-style shapes). When `debugger`
/// is `Some`, also attempts Phase 3 step 6's deep dyn-Future recovery
/// (re-read the awaitee at the recovered concrete TypeId).
fn build_chain_from_repr(start: RustEnumValue, debugger: Option<&Debugger>) -> Vec<Future> {
    build_chain_from_repr_bounded(start, MAX_BRANCH_DEPTH, debugger)
}

fn build_chain_from_repr_bounded(
    start: RustEnumValue,
    depth: u32,
    debugger: Option<&Debugger>,
) -> Vec<Future> {
    let mut result: Vec<Future> = vec![];

    if depth == 0 {
        result.push(Future::UnknownFuture);
        return result;
    }

    let mut next_future_repr = Some(start);
    while let Some(next_future) = next_future_repr.take() {
        let Ok(future) = AsyncFnFuture::try_from(&next_future) else {
            break;
        };
        result.push(Future::AsyncFn(future));

        let Some(member) = next_future.value else {
            break;
        };
        let Value::Struct(val) = member.value else {
            break;
        };

        // Phase 3 Feature D step 5 — collect every coroutine-shaped
        // field of the active variant (excluding the canonical
        // `__awaitee`). When two or more exist, the variant is
        // capturing parallel branches and we emit `Future::Multi`
        // *in addition to* the linear `__awaitee` chain (if any).
        let parallel_branches: Vec<RustEnumValue> = val
            .members
            .iter()
            .filter_map(|m| {
                if m.field_name.as_deref() == Some(AWAITEE_FIELD) {
                    return None;
                }
                if let Value::RustEnum(re) = &m.value {
                    return Some(re.clone());
                }
                None
            })
            .collect();
        if parallel_branches.len() >= 2 {
            let branches = parallel_branches
                .into_iter()
                .map(|seed| build_chain_from_repr_bounded(seed, depth - 1, debugger))
                .collect();
            result.push(Future::Multi(branches));
        }

        let awaitee = val.field(AWAITEE_FIELD);
        match awaitee {
            Some(Value::RustEnum(next_future)) => {
                next_future_repr = Some(next_future);
            }
            Some(Value::Struct(next_future)) => {
                let fmt_name = next_future.type_ident.name_fmt();
                let is_dyn_box = !matches!(fmt_name, "Sleep")
                    && !fmt_name.contains("JoinHandle");
                let leaf = if fmt_name == "Sleep" {
                    weak_error!(TokioSleepFuture::try_from(next_future.clone()))
                        .map(Future::TokioSleep)
                        .unwrap_or(Future::UnknownFuture)
                } else if fmt_name.contains("JoinHandle") {
                    weak_error!(TokioJoinHandleFuture::try_from(next_future.clone()))
                        .map(Future::TokioJoinHandleFuture)
                        .unwrap_or(Future::UnknownFuture)
                } else {
                    Future::Custom(CustomFuture::from(&next_future))
                };
                result.push(leaf);

                // Phase 3 Feature D step 6 (deeper half) — when the
                // awaitee is a `Pin<Box<dyn Future>>`-shaped Custom
                // future and a debugger handle is available, attempt
                // the concrete-type re-read. D2b's annotation in
                // `type_ident` already names the recovered concrete
                // type; this step parses the pointee at that type
                // and recurses into its state machine if it's a
                // coroutine.
                if is_dyn_box && let Some(dbg) = debugger {
                    let probe = Value::Struct(next_future.clone());
                    if let Some(loc) =
                        crate::debugger::r#async::future::locate_dyn_future(&probe, 4)
                        && let Some(re) = recover_concrete_future(dbg, &loc)
                    {
                        let inner =
                            build_chain_from_repr_bounded(re, depth - 1, debugger);
                        result.extend(inner);
                    }
                }
                break;
            }
            _ => {}
        }
    }

    result
}

/// Phase 3 Feature D step 6 (deeper half) — given a recovered
/// concrete type name and the dyn-pointer's data address, look up
/// the type DIE across every loaded `DebugInformation`, issue a
/// `Dqe::DataCast` to read the pointee at that type, and return the
/// resulting `RustEnumValue` if the type is a coroutine state
/// machine. Any failure (type not found, parse error, non-enum
/// result) returns `None` so the caller can degrade gracefully to
/// the bare `Future::Custom` annotation.
fn recover_concrete_future(
    dbg: &Debugger,
    loc: &crate::debugger::r#async::future::DynFutureLocator,
) -> Option<RustEnumValue> {
    use crate::debugger::variable::dqe::DataCast;

    let (debug_info, unit_off, die_off) =
        dbg.debugee.debug_info_all().into_iter().find_map(|di| {
            let (u, d) = di.find_type_die_ref(&loc.concrete_name)?;
            Some((di, u, d))
        })?;

    let dqe = Dqe::DataCast(DataCast::new(
        loc.pointer,
        debug_info.pathname(),
        unit_off,
        die_off,
    ));

    let mut results = weak_error!(dbg.read_variable(dqe))?;
    let qr = results.pop_if_single_el()?;
    if let Value::RustEnum(re) = qr.into_value() {
        Some(re)
    } else {
        None
    }
}

/// Return task header state value and point pair.
pub fn task_header_state_value_and_ptr(
    debugger: &Debugger,
    header_ptr: RelocatedAddress,
) -> Result<(usize, usize), Error> {
    let dqe: Dqe = Dqe::Field(
        Dqe::Deref(
            Dqe::Field(
                Dqe::PtrCast(PointerCast {
                    ptr: header_ptr.as_usize(),
                    ty: types::header_type_name().to_string(),
                })
                .boxed(),
                "pointer".to_string(),
            )
            .boxed(),
        )
        .boxed(),
        "state".to_string(),
    );

    let state = debugger
        .read_variable(dqe)?
        .pop_if_single_el()
        .ok_or(Error::Async(AsyncError::IncorrectAssumption(
            "Header::state field not found in structure",
        )))?;

    let state = state
        .modify_value(|_, state| {
            state
                .field("val")?
                .field("inner")?
                .field("value")?
                .field("v")?
                .field("value")
        })
        .ok_or(Error::Async(AsyncError::IncorrectAssumption(
            "Unexpected Header::state layout",
        )))?;

    let value = state.into_value();
    let addr = value
        .in_memory_location()
        .ok_or(Error::Async(AsyncError::IncorrectAssumption(
            "Header::state without address",
        )))?;
    let value = value
        .into_scalar()
        .and_then(|s| s.try_as_number())
        .ok_or(Error::Async(AsyncError::IncorrectAssumption(
            "Header::state should be usize",
        )))? as usize;

    Ok((value, addr))
}

/// Get task information using `Header` structure.
/// See https://github.com/tokio-rs/tokio/blob/tokio-1.38.0/tokio/src/runtime/task/core.rs#L150
pub fn task_from_header<'a>(
    debugger: &'a Debugger,
    task_header_ptr: QueryResult<'a>,
) -> Result<Task, Error> {
    let Value::Pointer(ptr) = task_header_ptr.value() else {
        return Err(Error::Async(AsyncError::IncorrectAssumption(
            "task.__0.raw.ptr.pointer not a pointer",
        )));
    };

    let vtab_ptr = task_header_ptr
        .clone()
        .modify_value(|pcx, val| val.deref(pcx)?.field("vtable")?.deref(pcx)?.field("poll"))
        .unwrap();
    let Value::Pointer(fn_ptr) = vtab_ptr.value() else {
        return Err(Error::Async(AsyncError::IncorrectAssumption(
            "(*(*task.__0.raw.ptr.pointer).vtable).poll should be a pointer",
        )));
    };
    let poll_fn_addr = fn_ptr
        .value
        .map(|a| RelocatedAddress::from(a as usize))
        .ok_or(AsyncError::IncorrectAssumption(
            "(*(*task.__0.raw.ptr.pointer).vtable).poll fn pointer should contain a value",
        ))?;

    // Now using the value of fn pointer finds poll function of this task
    let poll_fn_addr_global = poll_fn_addr.into_global(&debugger.debugee)?;
    let debug_info = debugger.debugee.debug_info(poll_fn_addr)?;
    let (poll_fn_die, _) = debug_info.find_function_by_pc(poll_fn_addr_global)?.ok_or(
        AsyncError::IncorrectAssumption("poll function for a task not found"),
    )?;

    // poll function should have `T: Future` and `S: Schedule` type parameters
    let t_tpl_die =
        poll_fn_die
            .get_template_parameter("T")
            .ok_or(AsyncError::IncorrectAssumption(
                "poll function should have `T` type argument",
            ))?;
    let t_tpl_die_type_ref = t_tpl_die.type_ref();

    let s_tpl_die =
        poll_fn_die
            .get_template_parameter("S")
            .ok_or(AsyncError::IncorrectAssumption(
                "poll function should have `S` type argument",
            ))?;
    let s_tpl_die_type_ref = s_tpl_die.type_ref();

    // Now we try to find suitable `tokio::runtime::task::core::Cell<T, S>` type
    let unit = poll_fn_die.unit();
    let iter = resolve_unit_call!(debug_info.dwarf(), unit, type_iter);
    let mut cell_type_die_name = None;

    gcx().with_interner(|i| -> Result<(), Error> {
        for (typ_sym, offset) in iter {
            let typ = i.resolve(*typ_sym).expect("string should exist");

            if typ.starts_with("Cell") {
                let typ_die = Die::new(poll_fn_die.dcx(), *offset)?;

                if typ_die.tag() == gimli::DW_TAG_structure_type {
                    let mut s_tpl_found = false;
                    let mut t_tpl_found = false;

                    typ_die.for_each_children(|child| {
                        if gimli::DW_TAG_template_type_parameter == child.tag() {
                            let type_ref = child.type_ref();
                            if type_ref == t_tpl_die_type_ref {
                                t_tpl_found = true;
                            }
                            if type_ref == s_tpl_die_type_ref {
                                s_tpl_found = true;
                            }
                        }
                    });

                    if s_tpl_found & t_tpl_found {
                        cell_type_die_name = typ_die.name();
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    })?;

    let cell_type_die_name = cell_type_die_name.ok_or(AsyncError::IncorrectAssumption(
        "tokio::runtime::task::core::Cell<T, S> type not found",
    ))?;

    // Cell type found, not cast task pointer to this type
    let ptr = RelocatedAddress::from(ptr.value.unwrap() as usize);
    let typ = format!(
        "NonNull<tokio::runtime::task::core::{}>",
        cell_type_die_name
    );

    let dqe = Dqe::Deref(
        Dqe::Field(
            Dqe::PtrCast(PointerCast {
                ptr: ptr.as_usize(),
                ty: typ,
            })
            .boxed(),
            "pointer".to_string(),
        )
        .boxed(),
    );

    // having this type now possible to take underlying future and task_id
    let task_id_dqe = Dqe::Field(
        Dqe::Field(dqe.clone().boxed(), "core".to_string()).boxed(),
        "task_id".to_string(),
    );
    let future_dqe = Dqe::Field(
        Dqe::Field(
            Dqe::Field(
                Dqe::Field(
                    Dqe::Field(
                        Dqe::Field(dqe.clone().boxed(), "core".to_string()).boxed(),
                        "stage".to_string(),
                    )
                    .boxed(),
                    "stage".to_string(),
                )
                .boxed(),
                "__0".to_string(),
            )
            .boxed(),
            "value".to_string(),
        )
        .boxed(),
        "__0".to_string(),
    );

    let task_id = debugger
        .read_variable(task_id_dqe)?
        .pop_if_single_el()
        .ok_or(Error::Async(AsyncError::IncorrectAssumption(
            "task_id field not found in task structure",
        )))?;
    let task_id: u64 = types::TaskIdValue::from_value(unit, task_id.into_value())?.into();

    let mut future = debugger.read_variable(future_dqe)?;
    let Some(QueryResult {
        value: Some(Value::RustEnum(future)),
        ..
    }) = future.pop()
    else {
        return Err(Error::Async(AsyncError::IncorrectAssumption(
            "task root future not found",
        )));
    };
    let task = Task::from_enum_repr(ptr, task_id, future);
    Ok(task)
}

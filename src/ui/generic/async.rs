// SPDX-License-Identifier: MIT
use crate::debugger::r#async::AsyncBacktrace;
use crate::debugger::r#async::AsyncFnFutureState;
use crate::debugger::r#async::Future;
use crate::debugger::r#async::TaskBacktrace;
use crate::ui::generic::print::ExternalPrinter;
use crate::ui::generic::print::style::{
    AsyncTaskView, ErrorView, FutureFunctionView, FutureTypeView,
};
use crossterm::style::Stylize;
use nix::errno::Errno;
use nix::libc;
use nix::sys::time::TimeSpec;
use std::mem::MaybeUninit;
use std::ops::Sub;
use std::time::Duration;

fn print_future(backtrace: &AsyncBacktrace, num: u32, future: &Future, printer: &ExternalPrinter) {
    match future {
        Future::AsyncFn(fn_fut) => {
            printer.println(format!(
                "#{num} async fn {}",
                FutureFunctionView::from(&fn_fut.async_fn)
            ));
            match fn_fut.state {
                AsyncFnFutureState::Suspend(await_num) => {
                    // Phase 3 Feature D — append source coords when the
                    // active variant carried DW_AT_decl_file/decl_line.
                    // Falls back to "await point N" alone for stripped
                    // binaries or pre-await states.
                    let loc = fn_fut
                        .await_location
                        .as_ref()
                        .map(|(file, line)| format!(" at {}:{line}", file.display()))
                        .unwrap_or_default();
                    printer.println(format!("\tsuspended at await point {await_num}{loc}"));
                }
                AsyncFnFutureState::Panicked => {
                    printer.println("\tpanicked!");
                }
                AsyncFnFutureState::Returned => {
                    printer.println("\talready resolved");
                }
                AsyncFnFutureState::Unresumed => {
                    printer.println("\tjust created");
                }
                AsyncFnFutureState::Ok => {
                    printer.println("\tcompleted");
                }
            }
        }
        Future::Custom(custom_fut) => {
            // Phase 3 Feature D batch D2b — when the awaitee is a
            // `dyn Future` fat pointer, append the recovered concrete
            // type. Phase 3A's annotation already lives on the inner
            // trait-object's name, so we don't double-print it; we
            // only show the concrete tag when it differs from the
            // outer name (i.e. there's a layer like `Pin<Box<...>>`
            // between the awaitee and the dyn struct).
            let outer = custom_fut.name.to_string();
            let line = match &custom_fut.concrete {
                Some(concrete) if concrete != &outer => {
                    format!(
                        "#{num} future {} [→ {concrete}]",
                        FutureTypeView::from(outer)
                    )
                }
                _ => format!("#{num} future {}", FutureTypeView::from(outer)),
            };
            printer.println(line);
        }
        Future::TokioJoinHandleFuture(jh_fut) => {
            let wait_for = backtrace
                .tasks
                .iter()
                .find(|t| t.raw_ptr == jh_fut.wait_for_task);
            let wait_for_str = wait_for
                .map(|task| format!(", wait for task id={}", task.task_id))
                .unwrap_or_default();

            printer.println(format!(
                "#{num} Join future {}{}",
                FutureTypeView::from(jh_fut.name.to_string()),
                wait_for_str,
            ));
        }
        Future::TokioSleep(sleep_fut) => {
            fn now_timespec() -> Result<TimeSpec, Errno> {
                let mut t = MaybeUninit::uninit();
                let res = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, t.as_mut_ptr()) };
                if res == -1 {
                    return Err(Errno::last());
                }
                let t = unsafe { t.assume_init() };
                Ok(TimeSpec::new(t.tv_sec, t.tv_nsec))
            }

            pub fn diff_from_now(i: (i64, u32)) -> (std::cmp::Ordering, Duration) {
                let now = now_timespec().expect("broken system clock");
                let this = TimeSpec::new(i.0, i.1 as i64);
                if this < now {
                    (std::cmp::Ordering::Less, Duration::from(now.sub(this)))
                } else {
                    (std::cmp::Ordering::Greater, Duration::from(this.sub(now)))
                }
            }

            let render = match diff_from_now(sleep_fut.instant) {
                (std::cmp::Ordering::Less, d) => {
                    format!("already happened {} seconds ago ", d.as_secs())
                }
                (std::cmp::Ordering::Greater, d) => {
                    format!("{} seconds from now", d.as_secs())
                }
                _ => unreachable!(),
            };

            printer.println(format!("#{num} sleep future, sleeping {render}",));
        }
        Future::UnknownFuture => {
            printer.println(format!("#{num} undefined future",));
        }
        Future::Multi(branches) => {
            // Phase 3 Feature D step 5 — the active variant carries
            // multiple parallel branches (e.g. tokio::join!). Render
            // each branch as a sub-trace, indented one level.
            printer.println(format!(
                "#{num} parallel branches ({} active):",
                branches.len()
            ));
            for (b, branch) in branches.iter().enumerate() {
                printer.println(format!("  branch {b}:"));
                for (i, fut) in branch.iter().enumerate() {
                    print_future(backtrace, i as u32, fut, printer);
                }
            }
        }
    }
}

fn print_task(backtrace: &AsyncBacktrace, task: &TaskBacktrace, printer: &ExternalPrinter) {
    let task_descr = format!("Task id: {}", task.task_id).bold();
    printer.println(AsyncTaskView::from(task_descr));

    for (i, fut) in task.futures.iter().enumerate() {
        print_future(backtrace, i as u32, fut, printer);
    }
}

pub fn print_backtrace(backtrace: &AsyncBacktrace, printer: &ExternalPrinter) {
    let mut workers = backtrace.workers.clone();
    let mut block_threads = backtrace.block_threads.clone();
    workers.sort_by_key(|w| w.thread.number);
    block_threads.sort_by_key(|pt| pt.thread.number);

    for bt in &block_threads {
        let block_thread_header = format!(
            "Thread #{} (pid: {}) block on:",
            bt.thread.number, bt.thread.pid,
        );
        if bt.in_focus {
            printer.println(block_thread_header.bold());
        } else {
            printer.println(block_thread_header);
        }

        for (i, fut) in bt.bt.futures.iter().enumerate() {
            print_future(backtrace, i as u32, fut, printer);
        }
    }

    printer.println("");

    for worker in &workers {
        let worker_header = format!(
            "Async worker #{} (pid: {}, local queue length: {})",
            worker.thread.number,
            worker.thread.pid,
            worker.queue.len(),
        );
        if worker.in_focus {
            printer.println(worker_header.bold());
        } else {
            printer.println(worker_header);
        }

        if let Some(active_task_idx) = worker.active_task {
            let active_task = backtrace
                .tasks
                .get(active_task_idx as usize)
                .or(worker.active_task_standby.as_ref());

            if let Some(active_task) = active_task {
                let task_descr = format!("Active task: {}", active_task.task_id).bold();
                printer.println(AsyncTaskView::from(task_descr));

                for (i, fut) in active_task.futures.iter().enumerate() {
                    print_future(backtrace, i as u32, fut, printer);
                }
            }
        }
    }
}

pub fn print_backtrace_full(backtrace: &AsyncBacktrace, printer: &ExternalPrinter) {
    print_backtrace(backtrace, printer);

    printer.println("");
    printer.println("Known tasks:");

    for task in backtrace.tasks.iter() {
        print_task(backtrace, task, printer);
    }
}

/// Phase 3 Feature D batch D2a — render the current task's awaitee
/// chain as a stack-frame list, source-coords-first. Mirrors how
/// `bt`/`backtrace` reads for synchronous frames so the user can
/// transfer their mental model directly.
///
/// Layout:
///
/// ```text
/// await-trace (task id: 7):
///   #0 my_app::handler at src/handler.rs:42
///   #1 my_app::middleware::auth::check at src/auth.rs:18
///   #2 tokio::time::Sleep (sleeping for 3s)
/// ```
///
/// Each frame is one element of the futures stack. Source coords
/// come from D1's `await_location`. When unavailable (Unresumed /
/// Returned / Panicked / Ok states, or stripped binaries) the frame
/// shows just the function name and the state.
pub fn print_await_trace(backtrace: &AsyncBacktrace, printer: &ExternalPrinter) {
    let Some(task) = backtrace.current_task() else {
        printer.println(ErrorView::from(
            "no active task found for current worker, or no active worker found",
        ));
        return;
    };

    printer.println(format!("await-trace (task id: {}):", task.task_id).bold());

    if task.futures.is_empty() {
        printer.println("\t<empty future stack>");
        return;
    }

    for (i, fut) in task.futures.iter().enumerate() {
        match fut {
            Future::AsyncFn(af) => {
                let fn_view = FutureFunctionView::from(&af.async_fn).to_string();
                let line = match (&af.state, &af.await_location) {
                    (AsyncFnFutureState::Suspend(n), Some((file, line))) => {
                        format!(
                            "  #{i} {fn_view} at {}:{line} (await point {n})",
                            file.display()
                        )
                    }
                    (AsyncFnFutureState::Suspend(n), None) => {
                        format!("  #{i} {fn_view} (await point {n}, no source coords)")
                    }
                    (AsyncFnFutureState::Unresumed, _) => {
                        format!("  #{i} {fn_view} (just created, not yet polled)")
                    }
                    (AsyncFnFutureState::Returned, _) => {
                        format!("  #{i} {fn_view} (already resolved)")
                    }
                    (AsyncFnFutureState::Panicked, _) => format!("  #{i} {fn_view} (panicked)"),
                    (AsyncFnFutureState::Ok, _) => format!("  #{i} {fn_view} (completed)"),
                };
                printer.println(line);
            }
            Future::Custom(custom) => {
                // Phase 3 Feature D batch D2b — append the recovered
                // concrete type when the awaitee is a `dyn Future`
                // fat pointer wrapped inside (e.g.) `Pin<Box<...>>`.
                let outer = custom.name.to_string();
                let line = match &custom.concrete {
                    Some(concrete) if concrete != &outer => format!(
                        "  #{i} {} [→ {concrete}] (custom future)",
                        FutureTypeView::from(outer)
                    ),
                    _ => format!("  #{i} {} (custom future)", FutureTypeView::from(outer)),
                };
                printer.println(line);
            }
            Future::TokioJoinHandleFuture(jh) => {
                let wait_for = backtrace
                    .tasks
                    .iter()
                    .find(|t| t.raw_ptr == jh.wait_for_task)
                    .map(|t| format!(" (waiting for task id={})", t.task_id))
                    .unwrap_or_default();
                printer.println(format!(
                    "  #{i} {}{}",
                    FutureTypeView::from(jh.name.to_string()),
                    wait_for,
                ));
            }
            Future::TokioSleep(sleep) => {
                printer.println(format!(
                    "  #{i} {} (tokio::time::Sleep, deadline {}s.{:09})",
                    FutureTypeView::from(sleep.name.to_string()),
                    sleep.instant.0,
                    sleep.instant.1,
                ));
            }
            Future::UnknownFuture => {
                printer.println(format!("  #{i} <unknown future>"));
            }
            Future::Multi(branches) => {
                // Phase 3 Feature D step 5 — render parallel
                // branches as a numbered sub-trace under the join /
                // select frame's slot.
                printer.println(format!(
                    "  #{i} parallel branches ({} active):",
                    branches.len()
                ));
                for (b, branch) in branches.iter().enumerate() {
                    printer.println(format!("    branch {b}:"));
                    for (j, fut) in branch.iter().enumerate() {
                        // Indent the inner frames a further two
                        // spaces so the visual hierarchy reads.
                        match fut {
                            Future::AsyncFn(af) => {
                                let fn_view = FutureFunctionView::from(&af.async_fn).to_string();
                                let line = match (&af.state, &af.await_location) {
                                    (AsyncFnFutureState::Suspend(n), Some((file, line))) => {
                                        format!(
                                            "      #{j} {fn_view} at {}:{line} (await point {n})",
                                            file.display()
                                        )
                                    }
                                    (AsyncFnFutureState::Suspend(n), None) => {
                                        format!(
                                            "      #{j} {fn_view} (await point {n}, no source coords)"
                                        )
                                    }
                                    (AsyncFnFutureState::Unresumed, _) => {
                                        format!("      #{j} {fn_view} (just created)")
                                    }
                                    (AsyncFnFutureState::Returned, _) => {
                                        format!("      #{j} {fn_view} (returned)")
                                    }
                                    (AsyncFnFutureState::Panicked, _) => {
                                        format!("      #{j} {fn_view} (panicked)")
                                    }
                                    (AsyncFnFutureState::Ok, _) => {
                                        format!("      #{j} {fn_view} (completed)")
                                    }
                                };
                                printer.println(line);
                            }
                            other => {
                                printer.println(format!("      #{j} {other:?}"));
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn print_task_ex(backtrace: &AsyncBacktrace, printer: &ExternalPrinter, regex: Option<&str>) {
    if let Some(regex) = regex {
        let re = regex::Regex::new(regex).unwrap();

        let tasks = &backtrace.tasks;
        for task in tasks.iter() {
            if let Some(Future::AsyncFn(f)) = task.futures.first()
                && re.find(&f.async_fn).is_some()
            {
                print_task(backtrace, task, printer);
            }
        }
    } else {
        // print current task
        let Some(active_task) = backtrace.current_task() else {
            printer.println(ErrorView::from(
                "no active task found for current worker, or no active worker found",
            ));
            return;
        };

        print_task(backtrace, active_task, printer);
    }
}

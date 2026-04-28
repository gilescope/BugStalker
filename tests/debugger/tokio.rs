// SPDX-License-Identifier: MIT
use crate::common::TestHooks;
use crate::{TOKIO_TICKER_APP, prepare_debugee_process};
use bugstalker::debugger::DebuggerBuilder;
use bugstalker::debugger::r#async::{AsyncFnFutureState, Future};
use serial_test::serial;

#[test]
#[serial]
fn test_async0() {
    let process = prepare_debugee_process(TOKIO_TICKER_APP, &[]);
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::default());
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("main.rs", 6).unwrap();
    debugger.start_debugee().unwrap();

    let async_bt = debugger.async_backtrace().unwrap();
    assert!(!async_bt.workers.is_empty());
    assert_eq!(async_bt.block_threads.len(), 0);
    assert!(async_bt.workers.iter().any(|w| w.active_task.is_some()));
    assert!(!async_bt.tasks.is_empty());
}

/// Phase 3 Feature D batch D1 — every suspended `async fn` task in the
/// ticker app should report a recovered `(file, line)` for its current
/// `.await`, derived from the active variant's `DW_AT_decl_file` /
/// `DW_AT_decl_line`. The ticker's only `.await` is at
/// `examples/tokiotiker/src/main.rs:5`.
#[test]
#[serial]
fn test_async_await_location_recovered() {
    let process = prepare_debugee_process(TOKIO_TICKER_APP, &[]);
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::default());
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("main.rs", 6).unwrap();
    debugger.start_debugee().unwrap();

    let async_bt = debugger.async_backtrace().unwrap();

    let mut suspended_with_loc = 0usize;
    let mut suspended_total = 0usize;
    for task in async_bt.tasks.iter() {
        for fut in &task.futures {
            if let Future::AsyncFn(af) = fut
                && matches!(af.state, AsyncFnFutureState::Suspend(_))
            {
                suspended_total += 1;
                if let Some((file, line)) = af.await_location.as_ref() {
                    suspended_with_loc += 1;
                    assert!(
                        file.to_string_lossy().ends_with("main.rs"),
                        "expected main.rs, got {file:?}",
                    );
                    assert_eq!(*line, 5, "ticker `.await` is on main.rs:5");
                }
            }
        }
    }

    assert!(suspended_total > 0, "no suspended async fns found");
    assert_eq!(
        suspended_with_loc, suspended_total,
        "every suspended task should carry its await source location \
         (got {suspended_with_loc}/{suspended_total})",
    );
}

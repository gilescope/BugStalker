// SPDX-License-Identifier: MIT
use crate::CALC_APP;
use crate::common::TestInfo;
use crate::common::{TestHooks, rust_version};
use crate::{HW_APP, RECURSION_APP, VARS_APP, assert_no_proc, prepare_debugee_process};
use bugstalker::debugger::variable::value::{SupportedScalar, Value};
use bugstalker::debugger::{Debugger, DebuggerBuilder};
use bugstalker::ui::command::parser::expression;
use bugstalker::version_switch;
use chumsky::Parser;
use serial_test::serial;
use std::mem;

#[test]
#[serial]
fn test_step_into() {
    let process = prepare_debugee_process(CALC_APP, &["1", "2", "3", "--description", "result"]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("main.rs", 10).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(10));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(25));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(21));
    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(22));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(26));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(21));
    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(22));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(27));

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(15));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_into_skip_libraries() {
    use crate::STEP_INTO_JMC_APP;
    use bugstalker::debugger::StepIntoMode;

    let process = prepare_debugee_process(STEP_INTO_JMC_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // LINE A — the entirely-library call (`"hi".to_uppercase()`).
    debugger.set_breakpoint_at_line("main.rs", 22).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(22));

    // (a) SkipLibraries over an all-library call → Step-Over semantics:
    //     stop on the next user line (LINE B = 26), never inside core.
    debugger
        .step_into_with(StepIntoMode::SkipLibraries)
        .unwrap();
    assert_eq!(info.line.take(), Some(26));

    // (b) MVP: SkipLibraries over a line whose library call invokes a
    //     user closure steps OVER it to the next user line (println = 28),
    //     it does not yet stop in `user_fn`. Phase-4 engine flips this.
    debugger
        .step_into_with(StepIntoMode::SkipLibraries)
        .unwrap();
    assert_eq!(info.line.take(), Some(28));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Step-into-skip-libraries as far as a program will run, from `main`'s
/// entry. Asserts the core invariant: a "skip libraries" stop is **never**
/// inside a library source file — every landing the user sees is their
/// own code. The walk ends naturally at `ProcessExit` (stepping off the
/// end of `main` degrades to run-to-completion).
///
/// This is the regression net for the three bugs the by-hand sweep found:
/// skipping a user call that followed a library call on the same line,
/// erroring off the end of `main`, and stopping on an inlined library
/// line (`boxed.rs`) inside a user frame.
fn assert_skip_libs_no_leak(app: &str, args: &[&'static str], max_steps: usize) {
    use bugstalker::debugger::{FrameKind, StepIntoMode, classify_source_path};
    use std::path::Path;

    let process = prepare_debugee_process(app, args);
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_fn("main").unwrap();
    debugger.start_debugee().unwrap();
    let _ = info.file.take();

    for i in 0..max_steps {
        // A step off the end of user code ends the program; anything else
        // is a genuine stepping failure worth surfacing.
        if debugger
            .step_into_with(StepIntoMode::SkipLibraries)
            .is_err()
        {
            break;
        }
        let file = info.file.take();
        let kind = classify_source_path(file.as_deref().map(Path::new));
        assert_ne!(
            kind,
            FrameKind::Library,
            "{app}: skip-libraries stopped in a library file at step {i}: {file:?}",
        );
    }
}

/// Positive landings: skip-libraries must step *into* user functions a
/// line calls, not over them — even when a library call (a `Vec`→`&[T]`
/// deref) precedes the user call on the same line. Pins the bug where
/// `helper(&v)` was stepped over instead of into. (The negative invariant
/// — never stopping in a library file — is covered by
/// `test_skip_libs_never_stops_in_library`; this is the positive side.)
#[test]
#[serial]
fn test_skip_libs_steps_into_user_calls() {
    use crate::STEP_INTO_PROBE_APP;
    use bugstalker::debugger::StepIntoMode;

    let process = prepare_debugee_process(STEP_INTO_PROBE_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("probe.rs", 16).unwrap(); // let v = vec![..]
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(16));

    // 16 `let v = vec![..]`     → 17 (stepped *over* the all-library vec!)
    // 17 `let r = helper(&v)`   → 8  (stepped *into* helper, past the deref)
    // 8/9/10 inside helper      → ...
    // 10 `total += compute(x)`  → 4  (stepped *into* the nested user call)
    for expected in [17, 8, 9, 10, 4, 5] {
        debugger
            .step_into_with(StepIntoMode::SkipLibraries)
            .unwrap();
        assert_eq!(
            info.line.take(),
            Some(expected),
            "skip-libraries landed on the wrong line",
        );
    }

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_skip_libs_never_stops_in_library() {
    use crate::{CALLS_APP, HW_APP, RECURSION_APP, STEP_INTO_PROBE_APP, VARS_APP};
    // `step_into_probe` exercises the load-bearing shape: `helper(&v)` does
    // a `Vec`→`&[T]` deref (library) *then* calls the user `helper` — the
    // engine must step *into* `helper`, not over the line.
    assert_skip_libs_no_leak(STEP_INTO_PROBE_APP, &[], 30);
    assert_skip_libs_no_leak(HW_APP, &[], 30);
    assert_skip_libs_no_leak(RECURSION_APP, &[], 40);
    assert_skip_libs_no_leak(VARS_APP, &[], 60); // inlined Box/Vec lines
    assert_skip_libs_no_leak(CALLS_APP, &[], 60);
    // calc is intentionally omitted: stepping into `env::args()` from
    // main's entry trips a *pre-existing* unwind limitation
    // (`NoUnwindInfoForAddress`) that affects plain `AnyFrame` step-into
    // identically — not a skip-libraries concern.
}

#[test]
#[serial]
fn test_step_into_recursion() {
    let process = prepare_debugee_process(RECURSION_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_fn("infinite_inc").unwrap();

    fn assert_arg(debugger: &Debugger, expected: u64) {
        let get_i_expr = expression::parser().parse("i").unwrap();
        let i_arg = debugger.read_argument(get_i_expr).unwrap().pop().unwrap();
        let Value::Scalar(scalar) = i_arg.into_value() else {
            panic!("not a scalar");
        };
        assert_eq!(scalar.value, Some(SupportedScalar::U64(expected)));
    }

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(11));
    assert_arg(&debugger, 1);

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(11));
    assert_arg(&debugger, 2);

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(11));
    assert_arg(&debugger, 3);

    debugger.step_into().unwrap();
    assert_eq!(info.line.take(), Some(11));
    assert_arg(&debugger, 4);

    mem::drop(debugger);
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_out() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_fn("main").unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(5));

    let rust_version = rust_version(HW_APP).unwrap();
    let step_count = version_switch!(
            rust_version,
            .. (1 . 81) => 4,
            (1 . 81) .. (1 . 85) => 5,
            (1 . 85) .. => 1,
    )
    .unwrap();
    for _ in 0..step_count {
        debugger.step_into().unwrap();
    }

    assert_eq!(info.line.take(), Some(15));

    debugger.step_out().unwrap();
    assert_eq!(info.line.take(), Some(7));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_over() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_fn("main").unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(5));

    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(7));
    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(9));
    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(10));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_over_inline_code() {
    // TODO this test should be reworked
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 545).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(545));
    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(546));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_over_on_fn_decl() {
    let process = prepare_debugee_process(HW_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger
        .set_breakpoint_at_line("hello_world.rs", 14)
        .unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(14));

    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(15));

    debugger.continue_debugee().unwrap();
    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_step_over_for_loop_issue_156() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 358).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(358));
    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(358));

    for _ in 0..100 {
        debugger.step_over().unwrap();
        assert_eq!(info.line.take(), Some(359));
    }

    debugger.step_over().unwrap();
    assert_eq!(info.line.take(), Some(363));

    debugger.continue_debugee().unwrap();

    assert_no_proc!(debugee_pid);
}

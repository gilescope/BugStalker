// SPDX-License-Identifier: MIT
//! Phase 9 conformance: drive the showcase debuggee end-to-end via the
//! typed `ScriptClient`.
//!
//! The whole point of the typed client is that the test reads like the
//! agent recipe it documents — set a breakpoint, run, inspect a
//! variable. No JSON pointer extraction, no string transcript.

use bs_script_conformance::structured::commands::r#break::{
    BreakInfo, BreakSet, Location,
};
use bs_script_conformance::structured::commands::print_var::Var;
use bs_script_conformance::structured::commands::run::Run;
use bs_script_conformance::structured::commands::{StopKind, StopReason};
use bs_script_conformance::structured::error::ErrorCode;
use bs_script_conformance::structured::event::Event;
use bs_script_conformance::{ClientError, spawn_example};
use serial_test::serial;

#[test]
#[serial]
fn showcase_dyn_ref_typed() {
    let mut bs = spawn_example("showcase");

    // Set a breakpoint at line 122 — yields one entry for `main` and
    // one for the closure copy in showcase.
    let set = bs
        .call(BreakSet {
            at: Location::Shorthand("main.rs:122".into()),
            deferred: false,
        })
        .expect("break.set");
    assert!(!set.deferred, "should resolve eagerly");
    assert!(!set.breakpoints.is_empty(), "no breakpoints created");

    // break.info round-trips the same set.
    let info = bs.call(BreakInfo {}).expect("break.info");
    assert_eq!(info.breakpoints.len(), set.breakpoints.len());

    // Run lands directly on a breakpoint stop.
    let stop: StopReason = bs.call(Run::default()).expect("run");
    assert!(
        matches!(stop.kind, StopKind::Breakpoint),
        "expected breakpoint stop, got {stop:?}"
    );

    // The user's original case: dump dyn_ref + dyn_box.
    let dyn_ref = bs
        .call(Var {
            name: Some("dyn_ref".into()),
            expression: None,
        })
        .expect("var dyn_ref");
    assert_eq!(dyn_ref.items.len(), 1, "dyn_ref must be in scope");
    assert!(
        dyn_ref.items[0].r#type.contains("Greeter"),
        "expected Greeter trait in type, got {}",
        dyn_ref.items[0].r#type
    );
    assert!(
        !dyn_ref.items[0].value_text.is_empty(),
        "value_text should not be empty"
    );

    let dyn_box = bs
        .call(Var {
            name: Some("dyn_box".into()),
            expression: None,
        })
        .expect("var dyn_box");
    assert_eq!(dyn_box.items.len(), 1, "dyn_box must be in scope");
    assert!(
        dyn_box.items[0].r#type.contains("Box"),
        "expected Box in type, got {}",
        dyn_box.items[0].r#type
    );

    // ProcessInstalled was reported as an event before the first call.
    assert!(
        bs.drain_events()
            .iter()
            .any(|e| matches!(e, Event::ProcessInstalled { .. })),
        "missing process_installed event"
    );
}

#[test]
#[serial]
fn unknown_method_is_method_not_found() {
    let mut bs = spawn_example("showcase");
    let err = bs
        .call_raw("no.such.method", serde_json::json!({}))
        .expect_err("unknown method must error");
    match err {
        ClientError::Server(bs_err) => {
            assert_eq!(bs_err.code, ErrorCode::MethodNotFound);
        }
        other => panic!("expected ClientError::Server(MethodNotFound), got {other:?}"),
    }
}

#[test]
#[serial]
fn break_set_surfaces_candidate_disambiguation() {
    use bs_script_conformance::structured::commands::r#break::LineCandidateStatus;
    let mut bs = spawn_example("showcase");
    // Showcase's main.rs:122 is heavy on inlining and yields multiple
    // line-table candidates. We don't assert exact addresses (ASLR
    // varies them) but we do assert that the chooser surfaces what it
    // saw so the agent can disambiguate.
    let resp = bs
        .call(BreakSet {
            at: Location::Shorthand("main.rs:122".into()),
            deferred: false,
        })
        .expect("break.set");
    assert!(
        !resp.candidates.is_empty(),
        "candidate list is empty — did diagnostics fall through? response: {resp:?}"
    );
    assert!(
        resp.candidates
            .iter()
            .any(|c| matches!(c.status, LineCandidateStatus::Selected)),
        "no Selected candidate; chooser must label at least one. response: {resp:?}"
    );
    // Every selected candidate's decl_file should match the request
    // (or be None for orphaned synthetic addresses). Inline copies
    // would surface with status: InlineCopy.
    for c in &resp.candidates {
        if matches!(c.status, LineCandidateStatus::Selected)
            && let Some(decl) = &c.decl_file
        {
            assert!(
                decl.ends_with("main.rs"),
                "selected candidate's decl_file is not main.rs: {decl:?}"
            );
        }
    }
}

#[test]
#[serial]
fn server_error_surfaces_typed() {
    let mut bs = spawn_example("showcase");
    // Asking for a variable before `run` should fail with
    // ProcessNotStarted.
    let err = bs
        .call(Var {
            name: Some("anything".into()),
            expression: None,
        })
        .expect_err("must error before run");
    match err {
        ClientError::Server(bs_err) => {
            assert_eq!(bs_err.code, ErrorCode::ProcessNotStarted, "got {bs_err}");
        }
        other => panic!("expected ClientError::Server, got {other:?}"),
    }
}

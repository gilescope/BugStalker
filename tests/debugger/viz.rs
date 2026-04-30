// SPDX-License-Identifier: MIT
//! Phase 4 Tier-A end-to-end test. Builds `viz_demo` (in
//! `examples/`) which carries two `#[derive(DebugView)]` types,
//! attaches BugStalker, and verifies the spec registry recovered
//! the right number of entries with the right attributes.
//!
//! The test asserts the *contract* of the macro + section reader,
//! not any rendering side-effect — render integration is a
//! separate phase-4 batch. If this test passes we know the spec
//! data flows: macro → linker → section reader → Debugger API.

use crate::common::{TestHooks, TestInfo};
use crate::prepare_debugee_process;
use bugstalker::bs_viz_spec::{self, Format};
use bugstalker::debugger::DebuggerBuilder;
use bugstalker::ui::generic::variable::render_value_with_viz;
use serial_test::serial;

const VIZ_DEMO_APP: &str = "./examples/target/debug/viz_demo";

#[test]
#[serial]
fn debug_view_specs_loaded_from_demo_binary() {
    let process = prepare_debugee_process(VIZ_DEMO_APP, &[]);
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let debugger = builder.build(process).unwrap();

    // Three derives in the demo binary (Person, Counter, Wrap);
    // all three should have been discovered at attach. `Wrap`
    // is generic — we still emit only one spec per type
    // *definition*.
    assert_eq!(
        debugger.view_spec_count(),
        3,
        "expected 3 specs from viz_demo, got {}",
        debugger.view_spec_count(),
    );

    // Person — exact match on the local-name stored by the
    // macro, plus suffix match against a fully-qualified probe.
    let person = debugger
        .view_spec_for("Person")
        .expect("Person spec should be in the registry");
    assert_eq!(person.type_name, "Person");
    assert_eq!(
        person.summary.as_deref(),
        Some("Person({name}, age {age})"),
    );
    assert_eq!(person.fields.len(), 5);

    // Field attributes: skip, rename, format = "hex".
    let by_name: std::collections::HashMap<&str, &bs_viz_spec::FieldSpec> = person
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();

    assert!(by_name["_private_token"].hidden, "_private_token should be hidden");
    assert_eq!(by_name["category"].rename.as_deref(), Some("kind"));
    assert_eq!(by_name["flags"].format, Format::Hex);
    assert!(!by_name["name"].hidden);
    assert!(by_name["name"].rename.is_none());

    // Counter — proves multi-derive in one binary works (one
    // section contains many length-prefixed entries).
    let counter = debugger
        .view_spec_for("Counter")
        .expect("Counter spec should be in the registry");
    assert_eq!(counter.summary.as_deref(), Some("Box<{label}> = {n}"));

    // Suffix match — what BugStalker's renderer will eventually
    // pass: a fully-qualified demangled name.
    assert!(debugger.view_spec_for("viz_demo::Person").is_some());
    assert!(debugger.view_spec_for("viz_demo::Counter").is_some());

    // Generic-arg stripping — `Wrap<i32>` and `Wrap<&str>` both
    // resolve to the single `Wrap` definition.
    assert!(debugger.view_spec_for("Wrap<i32>").is_some());
    assert!(debugger.view_spec_for("Wrap<&str>").is_some());
    assert!(debugger.view_spec_for("viz_demo::Wrap<i32>").is_some());
    // Even nested generics resolve cleanly.
    assert!(debugger.view_spec_for("Wrap<Vec<i32>>").is_some());

    // A type without a derive must miss.
    assert!(debugger.view_spec_for("std::string::String").is_none());

    drop(debugger);
}

#[test]
#[serial]
fn debug_view_summary_applied_at_render_time() {
    let process = prepare_debugee_process(VIZ_DEMO_APP, &[]);
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // BP at the `black_box` line — `p`, `c`, `w_i32`, `w_str`
    // are all alive at that point so we can read them as locals.
    debugger.set_breakpoint_at_line("main.rs", 49).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(49));

    let viz = debugger.view_registry();
    let locals = debugger.read_local_variables().unwrap();

    // Find both registered locals; render each against the viz
    // registry and the bare path. The viz path must produce the
    // summary template; the bare path must not (proves we
    // didn't accidentally make the templating unconditional).
    let p_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("p"))
        .expect("local `p` should be in scope at line 41");
    let bare = render_value_with_viz(p_local.value(), None);
    let with_spec = render_value_with_viz(p_local.value(), Some(viz));

    assert!(
        with_spec.contains("Person(Ada, age 36)"),
        "summary template not applied — render output: {with_spec}",
    );
    assert!(
        !bare.contains("Person(Ada, age 36)"),
        "viz=None path should not apply the template; got: {bare}",
    );

    // `_private_token` is `#[bs_viz(skip)]`; it should *not*
    // appear in the with-spec render.
    assert!(
        !with_spec.contains("_private_token"),
        "skip attribute not honoured — _private_token leaked into render: {with_spec}",
    );
    // It *should* appear in the bare render to confirm we didn't
    // accidentally hide it everywhere.
    assert!(
        bare.contains("_private_token"),
        "bare render unexpectedly omitted _private_token — render is broken: {bare}",
    );

    // `category` is renamed to `kind`. With the spec applied,
    // the original name must not appear, the renamed one must.
    assert!(
        with_spec.contains("kind:"),
        "rename to `kind` not applied: {with_spec}",
    );
    assert!(
        !with_spec.contains("category:"),
        "rename leaked the original `category:` label: {with_spec}",
    );

    // Step 3 — `format = "hex"` on `flags` produces `0xff00ff`
    // (lower-case for `{:#x}`). The bare path keeps the default
    // `u32(16711935)` shape.
    assert!(
        with_spec.contains("flags: 0xff00ff"),
        "format=hex not applied — render output: {with_spec}",
    );
    assert!(
        !bare.contains("0xff00ff"),
        "bare render unexpectedly applied hex format: {bare}",
    );

    // Step 3 — generic `Wrap<i32>` resolves through the
    // `Wrap` spec and renders the summary template
    // `Wrap[{inner}]`. Read both monomorphisations to prove
    // both work off the same single registered entry.
    let w_i32 = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("w_i32"))
        .expect("local `w_i32` should be in scope");
    let w_str = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("w_str"))
        .expect("local `w_str` should be in scope");
    let wi = render_value_with_viz(w_i32.value(), Some(viz));
    let ws = render_value_with_viz(w_str.value(), Some(viz));
    assert!(
        wi.contains("Wrap[17]"),
        "Wrap<i32> summary not applied — got: {wi}",
    );
    assert!(
        ws.contains("Wrap[fish]"),
        "Wrap<&str> summary not applied — got: {ws}",
    );

    debugger.continue_debugee().unwrap();
    drop(debugger);
}

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
use bugstalker::dap::yadap::session::data;
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

    // Ten derives in the demo binary: Person, Counter, Wrap,
    // Event, UserId, Point, Sentinel, Status, Marker, Doc.
    // `Wrap` is generic — one spec per type *definition*;
    // tuple, unit, enum, and byte-array struct each count as
    // one spec.
    assert_eq!(
        debugger.view_spec_count(),
        10,
        "expected 10 specs from viz_demo, got {}",
        debugger.view_spec_count(),
    );

    // Person — step 10 records the fully-qualified
    // `module_path!()`-prefixed name. The registry's reverse-
    // suffix match also resolves the local-only `Person`
    // probe, so both queries hit the same spec.
    let person = debugger
        .view_spec_for("Person")
        .expect("Person spec should be in the registry");
    assert_eq!(person.type_name, "viz_demo::Person");
    assert!(debugger.view_spec_for("viz_demo::Person").is_some());
    assert_eq!(person.summary.as_deref(), Some("Person({name}, age {age})"),);
    assert_eq!(person.fields.len(), 5);

    // Field attributes: skip, rename, format = "hex".
    let by_name: std::collections::HashMap<&str, &bs_viz_spec::FieldSpec> =
        person.fields.iter().map(|f| (f.name.as_str(), f)).collect();

    assert!(
        by_name["_private_token"].hidden,
        "_private_token should be hidden"
    );
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

    // Step 5 — tuple + unit structs are spec'd. UserId has one
    // field with `format = "hex"` named `__0`. Point has two
    // unnamed fields. Sentinel has none.
    let uid_spec = debugger
        .view_spec_for("UserId")
        .expect("UserId spec should be in the registry");
    assert_eq!(uid_spec.summary.as_deref(), Some("UserId#{__0}"));
    assert_eq!(uid_spec.fields.len(), 1);
    assert_eq!(uid_spec.fields[0].name, "__0");
    assert_eq!(uid_spec.fields[0].format, Format::Hex);

    let pt_spec = debugger
        .view_spec_for("Point")
        .expect("Point spec should be in the registry");
    assert_eq!(pt_spec.fields.len(), 2);
    assert_eq!(pt_spec.fields[0].name, "__0");
    assert_eq!(pt_spec.fields[1].name, "__1");

    let sentinel_spec = debugger
        .view_spec_for("Sentinel")
        .expect("Sentinel spec should be in the registry");
    assert!(sentinel_spec.fields.is_empty());
    assert_eq!(sentinel_spec.summary.as_deref(), Some("Sentinel"));

    // Step 6 + 8 — enum spec round-trip. Step 6 emits the
    // type-level summary; step 8 also emits per-variant
    // summaries + tags.
    let status_spec = debugger
        .view_spec_for("Status")
        .expect("Status enum spec should be in the registry");
    assert_eq!(status_spec.summary.as_deref(), Some("Status[{__0}]"));
    assert!(
        status_spec.fields.is_empty(),
        "enum derive emits no top-level field entries; got {:?}",
        status_spec.fields,
    );
    assert_eq!(status_spec.variants.len(), 3);
    let connected = status_spec
        .variants
        .iter()
        .find(|v| v.name == "Connected")
        .expect("Connected variant should be in the spec");
    assert_eq!(
        connected.summary.as_deref(),
        Some("✓ Connected (port {__0})"),
    );
    assert_eq!(connected.tag.as_deref(), Some("ok"));
    assert_eq!(connected.fields.len(), 1);
    assert_eq!(connected.fields[0].name, "__0");

    let err_v = status_spec
        .variants
        .iter()
        .find(|v| v.name == "Error")
        .expect("Error variant should be in the spec");
    assert_eq!(err_v.tag.as_deref(), Some("err"));

    // Step 9 — `name = "qualified::Marker"` overrides the
    // recorded type_name. The local-only "Marker" lookup must
    // miss; the explicit qualified key hits exactly. This is
    // how a crate author disambiguates after a suffix-match
    // ambiguity bail.
    let marker_spec = debugger
        .view_spec_for("qualified::Marker")
        .expect("qualified::Marker should resolve via the explicit name");
    assert_eq!(marker_spec.summary.as_deref(), Some("Marker#{__0}"));
    // The fully-qualified probe also succeeds via suffix-match
    // on its own — `something::qualified::Marker` should still
    // resolve.
    assert!(debugger.view_spec_for("crate::qualified::Marker").is_some());

    drop(debugger);
}

#[test]
#[serial]
fn debug_view_summary_applied_at_render_time() {
    let process = prepare_debugee_process(VIZ_DEMO_APP, &[]);
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // BP at the `black_box` line — every local in `main` is
    // alive at this point.
    debugger.set_breakpoint_at_line("main.rs", 139).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(139));

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

    // Step 4 — `iso8601` and `duration` formats applied.
    // `created_at = 1705322096` is `2024-01-15T12:34:56Z`;
    // `latency_ns = 5_000_000` is `5.000ms`. Both appear inside
    // the summary template *and* in the per-field child render.
    let ev_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("ev"))
        .expect("local `ev` should be in scope");
    let ev_with_spec = render_value_with_viz(ev_local.value(), Some(viz));
    let ev_bare = render_value_with_viz(ev_local.value(), None);

    assert!(
        ev_with_spec.contains("2024-01-15T12:34:56Z"),
        "iso8601 format not applied — got: {ev_with_spec}",
    );
    assert!(
        ev_with_spec.contains("5.000ms"),
        "duration format not applied — got: {ev_with_spec}",
    );
    assert!(
        !ev_bare.contains("2024-01-15"),
        "viz=None path unexpectedly produced ISO-8601: {ev_bare}",
    );
    assert!(
        !ev_bare.contains("5.000ms"),
        "viz=None path unexpectedly produced ms duration: {ev_bare}",
    );
    // The non-formatted `seq` field still renders as the bare
    // type/value form. Sanity that we didn't accidentally route
    // every field through `format_scalar`.
    assert!(
        ev_with_spec.contains("seq:"),
        "seq field missing from spec render: {ev_with_spec}",
    );

    // Step 5 — tuple-struct render. UserId has one field named
    // `__0` with `format = "hex"` applied. The summary template
    // resolves the placeholder, AND the field-level hex
    // formatter is applied via the same path.
    let uid_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("uid"))
        .expect("local `uid` should be in scope");
    let uid_with_spec = render_value_with_viz(uid_local.value(), Some(viz));
    assert!(
        uid_with_spec.contains("UserId#0xcafebabe"),
        "tuple-struct hex format not applied to __0: {uid_with_spec}",
    );

    let pt_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("pt"))
        .expect("local `pt` should be in scope");
    let pt_with_spec = render_value_with_viz(pt_local.value(), Some(viz));
    assert!(
        pt_with_spec.contains("Point(10, 20)"),
        "multi-field tuple-struct summary not applied: {pt_with_spec}",
    );

    let sentinel_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("sentinel"));
    if let Some(sentinel_local) = sentinel_local {
        let sentinel_with_spec = render_value_with_viz(sentinel_local.value(), Some(viz));
        assert!(
            sentinel_with_spec.contains("Sentinel"),
            "unit-struct summary not applied: {sentinel_with_spec}",
        );
    }
    // Note: `sentinel: Sentinel` may be elided by rustc if it
    // contributes no bytes — DWARF on darwin sometimes drops
    // unit-struct locals entirely. Don't fail the test for that;
    // the spec's existence in the registry (asserted above) is
    // already proved.

    // Step 6 — enum summary application. `status_ok =
    // Status::Connected(443)`; the type-level summary
    // `Status[{__0}]` substitutes `__0` from the *active
    // variant*'s struct (`Connected(443)` → `Status[443]`).
    let status_ok_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("status_ok"))
        .expect("local `status_ok` should be in scope");
    let status_err_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("status_err"))
        .expect("local `status_err` should be in scope");
    let ok_with_spec = render_value_with_viz(status_ok_local.value(), Some(viz));
    let err_with_spec = render_value_with_viz(status_err_local.value(), Some(viz));
    let ok_bare = render_value_with_viz(status_ok_local.value(), None);

    // Step 8 — variant-level summary and tag override the
    // type-level template. `Connected(443)` carries
    // `summary = "✓ Connected (port {__0})"` and `tag = "ok"`,
    // so the rendered form prepends `[ok]` and uses the
    // variant template instead of the type-level
    // `Status[{__0}]`.
    assert!(
        ok_with_spec.contains("[ok]") && ok_with_spec.contains("Connected (port 443)"),
        "variant-level summary + tag not applied to Connected: {ok_with_spec}",
    );
    assert!(
        err_with_spec.contains("[err]") && err_with_spec.contains("Error: transport reset"),
        "variant-level summary + tag not applied to Error: {err_with_spec}",
    );
    assert!(
        !ok_bare.contains("[ok]"),
        "viz=None path unexpectedly produced variant tag: {ok_bare}",
    );
    assert!(
        !ok_bare.contains("Connected (port"),
        "viz=None path unexpectedly produced variant summary: {ok_bare}",
    );

    // Step 11 — `format = "utf8"` and `format = "hexdump"`
    // applied to byte-array fields. `body` is `Vec<u8>` carrying
    // ASCII; `raw` carries non-printable bytes. Both go through
    // `format_bytes` rather than `format_scalar`.
    let doc_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("doc"))
        .expect("local `doc` should be in scope");
    let doc_with_spec = render_value_with_viz(doc_local.value(), Some(viz));
    assert!(
        doc_with_spec.contains(r#"b"hello world""#),
        "format=utf8 not applied to body field: {doc_with_spec}",
    );
    assert!(
        doc_with_spec.contains("00 ff 42 53 56 31"),
        "format=hexdump not applied to raw field: {doc_with_spec}",
    );
    // The non-formatted `size` field still renders as the bare
    // form. Sanity check we didn't accidentally route every
    // field through format_bytes.
    assert!(
        doc_with_spec.contains("size:"),
        "size field missing from spec render: {doc_with_spec}",
    );

    // Step 9 — `Marker` registers under `"qualified::Marker"`
    // (an arbitrary prefix the user chose). The renderer
    // queries with the actual demangled path
    // (`viz_demo::Marker`); since that path does not end in
    // `::qualified::Marker`, the suffix-match misses and the
    // bare struct render is produced. This is the *expected*
    // behaviour — the override exists so users can pick the key
    // *they* want for an external lookup, not to magically
    // rewrite what the demangler emits. The contract is
    // asserted at registry level above; here we sanity-check
    // that the renderer doesn't panic when a registered name
    // doesn't match the demangled path.
    let marker_local = locals
        .iter()
        .find(|qr| qr.identity().name.as_deref() == Some("marker"));
    if let Some(marker_local) = marker_local {
        let r = render_value_with_viz(marker_local.value(), Some(viz));
        assert!(!r.is_empty(), "marker render should not be empty");
    }

    // Step 7 — DAP path now threads the registry through. The
    // public `data::read_locals` function is what the IDE
    // ultimately consumes; assert that its `value` strings
    // carry the viz summaries (not the placeholder `{...}`).
    let dap_locals = data::read_locals(&debugger).unwrap();
    let dap_p = dap_locals
        .iter()
        .find(|v| v.name.starts_with("p"))
        .expect("DAP locals should include `p`");
    assert!(
        dap_p.value.contains("Person(Ada, age 36)"),
        "DAP value for `p` missing summary — IDE wiring not flowing: {}",
        dap_p.value,
    );
    let dap_status_ok = dap_locals
        .iter()
        .find(|v| v.name.starts_with("status_ok"))
        .expect("DAP locals should include `status_ok`");
    assert!(
        dap_status_ok.value.contains("[ok]")
            && dap_status_ok.value.contains("Connected (port 443)"),
        "DAP value for `status_ok` missing variant-level enum render: {}",
        dap_status_ok.value,
    );
    // Generics work via DAP too.
    let dap_w_i32 = dap_locals
        .iter()
        .find(|v| v.name.starts_with("w_i32"))
        .expect("DAP locals should include `w_i32`");
    assert!(
        dap_w_i32.value.contains("Wrap[17]"),
        "DAP value for `w_i32` missing generic summary: {}",
        dap_w_i32.value,
    );

    debugger.continue_debugee().unwrap();
    drop(debugger);
}

/// Regression test for the `split-debuginfo = "unpacked"` gap:
/// Rust's default macOS layout leaves DWARF in per-CU `.o` files
/// pointed at by `N_OSO` stabs. Without a `.dSYM` bundle the
/// loader used to fall through to "no debug info", and every
/// breakpoint went UNVERIFIED in DAP. The loader now auto-runs
/// `dsymutil` on first attach to materialise the bundle.
///
/// Test: nuke the bundle, attach, set a breakpoint by line, run.
/// If the loader regenerated the dSYM the BP resolves; otherwise
/// `set_breakpoint_at_line` fails.
#[cfg(target_os = "macos")]
#[test]
#[serial]
fn debug_view_loader_recovers_dsym_for_split_debuginfo() {
    use std::path::PathBuf;
    let bin = PathBuf::from(VIZ_DEMO_APP);
    let mut dsym_dir = bin.clone().into_os_string();
    dsym_dir.push(".dSYM");
    let dsym_dir = PathBuf::from(dsym_dir);
    if dsym_dir.exists() {
        let _ = std::fs::remove_dir_all(&dsym_dir);
    }
    assert!(
        !dsym_dir.exists(),
        "test setup should have nuked the dSYM at {dsym_dir:?}",
    );

    // SAFETY: single-threaded test (`#[serial]`), and we restore
    // the env var at the end so other tests aren't affected.
    // `set_var` / `remove_var` are unsafe in edition 2024.
    unsafe {
        std::env::set_var("BS_TEST_NO_AUTODSYM", "1");
    }
    let process = prepare_debugee_process(VIZ_DEMO_APP, &[]);
    unsafe {
        std::env::remove_var("BS_TEST_NO_AUTODSYM");
    }

    let info = TestInfo::default();
    let builder = DebuggerBuilder::new().with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // The BP at `main.rs:139` only resolves if DWARF was loaded.
    debugger
        .set_breakpoint_at_line("main.rs", 139)
        .expect("loader must auto-run dsymutil and resolve `main.rs:139`");
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(139));

    // Bundle should now exist on disk — the loader's dsymutil
    // call regenerated it.
    assert!(
        dsym_dir.exists(),
        "loader should have re-created {dsym_dir:?}",
    );

    debugger.continue_debugee().unwrap();
    drop(debugger);
}

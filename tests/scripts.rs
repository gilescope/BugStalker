// SPDX-License-Identifier: MIT
//! `cargo test --test scripts` — each `tests/scripts/*.json5` is a
//! self-contained debugging test. Spawns `bs --test <script> <debuggee>`
//! and asserts a clean (exit 0) TAP run.
//!
//! Why a runner instead of an in-process call? Each script gets a fresh
//! debugger + debuggee process, which is what serial integration tests
//! want anyway. The wrapper is ~30 LOC and the alternative — wiring
//! `bugstalker::ui::script::run_test` into the binary's address space —
//! buys us nothing in this slice.

use std::path::PathBuf;
use std::process::Command;

const BS_BINARY: &str = "./target/debug/bs";
const VARS_DEBUGGEE: &str = "./examples/target/debug/vars";
const SHOWCASE_DEBUGGEE: &str = "./examples/target/debug/showcase";

// One `#[test]` per script. When the count outgrows hand-rolling
// (~20+), swap to a build.rs glob. For now the explicit list keeps the
// `cargo test` filter syntax (`-- read_enum`) working without macros.

#[test]
fn read_struct() {
    run("tests/scripts/read_struct.json5", VARS_DEBUGGEE)
}
#[test]
fn read_scalars() {
    run("tests/scripts/read_scalars.json5", VARS_DEBUGGEE)
}
#[test]
fn read_array() {
    run("tests/scripts/read_array.json5", VARS_DEBUGGEE)
}
#[test]
fn read_enum() {
    run("tests/scripts/read_enum.json5", VARS_DEBUGGEE)
}
#[test]
fn read_pointers() {
    run("tests/scripts/read_pointers.json5", VARS_DEBUGGEE)
}
#[test]
fn read_deref_pointers() {
    run("tests/scripts/read_deref_pointers.json5", VARS_DEBUGGEE)
}
#[test]
fn read_type_alias() {
    run("tests/scripts/read_type_alias.json5", VARS_DEBUGGEE)
}
#[test]
fn read_strings() {
    run("tests/scripts/read_strings.json5", VARS_DEBUGGEE)
}
#[test]
fn read_arguments() {
    run("tests/scripts/read_arguments.json5", VARS_DEBUGGEE)
}
#[test]
fn read_vec_and_slice() {
    run("tests/scripts/read_vec_and_slice.json5", VARS_DEBUGGEE)
}
#[test]
fn read_zst() {
    run("tests/scripts/read_zst.json5", VARS_DEBUGGEE)
}
#[test]
fn read_statics() {
    run("tests/scripts/read_statics.json5", VARS_DEBUGGEE)
}
#[test]
fn read_time() {
    run("tests/scripts/read_time.json5", VARS_DEBUGGEE)
}
#[test]
fn read_address_op() {
    run("tests/scripts/read_address_op.json5", VARS_DEBUGGEE)
}

// Phase 3 Feature A batch A4 — locks in the dyn-vtable-as-typed-
// record render against the showcase fixtures. ASLR-dependent
// addresses are masked in the script's `$regex` expects.
#[test]
fn dyn_vtable_render() {
    run("tests/scripts/dyn_vtable_render.json5", SHOWCASE_DEBUGGEE)
}

// Phase 3 Feature A batch A6 — depth-aware collapse on nested dyn.
// `Vec<Box<dyn Greeter>>` at depth 0 puts each entry at depth 1,
// where the multi-line vtable view collapses to a one-liner.
#[test]
fn dyn_vtable_nested() {
    run("tests/scripts/dyn_vtable_nested.json5", SHOWCASE_DEBUGGEE)
}

// Phase 3 Feature A batch A10 — corner cases beyond the basic
// [&dyn / Box<dyn>] pair: Pin<Box<dyn Future>> exercises the
// Pin-peeling path through the specialization layer;
// Box<dyn Iterator<Item=u32>> exercises the `<Concrete as Trait>`
// override + `Trait::default_impl` mix in the trait-grouped
// render.
#[test]
fn dyn_vtable_corners() {
    run("tests/scripts/dyn_vtable_corners.json5", SHOWCASE_DEBUGGEE)
}

fn run(script: &str, debuggee: &str) {
    require_path(BS_BINARY, "build bs first: `cargo build --bin bs`");
    require_path(
        debuggee,
        "build the debuggee first; see tests/scripts/README.md",
    );

    let output = Command::new(BS_BINARY)
        .arg("--test")
        .arg(script)
        .arg(debuggee)
        .output()
        .expect("failed to spawn bs --test");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        panic!(
            "script {} FAILED (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            script,
            output.status.code(),
            stdout,
            stderr
        );
    }
}

fn require_path(p: &str, hint: &str) {
    if !PathBuf::from(p).exists() {
        panic!("missing required path {p:?}: {hint}");
    }
}

// SPDX-License-Identifier: MIT
//! Test-side helpers for driving `bs --script` via the typed
//! [`ScriptClient`].
//!
//! The Rust API in `bugstalker::ui::script::client` is the public
//! way to drive the JSON-RPC front-end from a Rust host. This crate's
//! tests use it to exercise the full transport + dispatch stack
//! against the example debuggees, asserting that response shapes
//! match what an agent would receive.

use std::path::{Path, PathBuf};

pub use bugstalker::ui::script::client::{
    ClientError, ClientResult, ScriptClient,
};
pub use bugstalker::ui::structured;

/// Locate the `bs` binary built by `cargo build`.
pub fn bs_binary() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target = Path::new(manifest_dir)
        .ancestors()
        .nth(2)
        .expect("crate sits two levels under the workspace root")
        .join("target")
        .join("debug")
        .join("bs");
    if !target.exists() {
        panic!(
            "expected bs binary at {target:?} — run `cargo build --bin bs` before \
             the conformance suite"
        );
    }
    target
}

/// Locate one of the example debuggees, trying common cargo target dirs.
pub fn example_debuggee(name: &str) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir)
        .ancestors()
        .nth(2)
        .expect("crate sits two levels under the workspace root");
    let candidates = [
        workspace_root.join("examples/target/debug").join(name),
        workspace_root
            .join(format!(
                "examples/target/{}-apple-darwin/debug",
                std::env::consts::ARCH
            ))
            .join(name),
        workspace_root
            .join(format!(
                "examples/target/{}-unknown-linux-gnu/debug",
                std::env::consts::ARCH
            ))
            .join(name),
    ];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "no built example named `{name}` — run \
                 `cd examples && cargo build` first. Searched: {candidates:?}"
            )
        })
}

/// Spawn a `ScriptClient` against an example debuggee. Convenience
/// wrapper around `ScriptClient::spawn(bs_binary(), example_debuggee(name))`.
pub fn spawn_example(name: &str) -> ScriptClient {
    ScriptClient::spawn(bs_binary(), example_debuggee(name))
        .expect("spawn ScriptClient against example")
}

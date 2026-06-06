// SPDX-License-Identifier: MIT
use crate::dap_client;

use anyhow::Context as _;
use base64::Engine as _;
use bs_replay_driver::engine::format::TraceWriter;
use bs_replay_driver::engine::format::event::Event;
use bs_replay_driver::engine::format::manifest::Manifest;
use bs_replay_driver::engine::format::version::FormatVersion;
use dap_client::{DapSession, example_bin, example_source, spawn_attach_target, wait_for_exit};
use serde_json::{Value, json};
use serial_test::serial;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const HELLO_LINE: i64 = 5;
const SET_VAR_LINE: i64 = 35;
const BS_VIZ_SPEC_REQUESTED_COMMENT_LINE: i64 = 91;
const BS_VIZ_SPEC_BOUND_STATEMENT_LINE: i64 = 96;
/// Last line of `showcase`'s `main`, after every local in sections
/// 1..12 is in scope. Used by the showcase regression test below.
const SHOWCASE_LINE: i64 = 144;
const OPTIONAL_EVENT_TIMEOUT: Duration = Duration::from_secs(10);

fn assert_response(response: &Value, command: &str, request_seq: i64, success: bool) -> bool {
    assert_eq!(
        response.get("type").and_then(Value::as_str),
        Some("response")
    );
    assert_eq!(
        response.get("command").and_then(Value::as_str),
        Some(command)
    );
    assert_eq!(
        response.get("request_seq").and_then(Value::as_i64),
        Some(request_seq)
    );
    let got_success = response.get("success").and_then(Value::as_bool);
    if got_success == Some(success) {
        assert!(response.get("seq").and_then(Value::as_i64).is_some());
        return true;
    }
    if success
        && let Some(message) = response.get("message").and_then(Value::as_str)
        && (message.contains("ENOSYS")
            || message.contains("Function not implemented")
            || message.contains("EPERM")
            || message.contains("Operation not permitted"))
    {
        return false;
    }
    assert_eq!(got_success, Some(success), "response: {response}");
    assert!(response.get("seq").and_then(Value::as_i64).is_some());
    true
}

macro_rules! ensure_response {
    ($session:expr, $response:expr, $command:expr, $seq:expr, $success:expr) => {{
        if !assert_response($response, $command, $seq, $success) {
            $session.shutdown();
            return Ok(());
        }
    }};
}

macro_rules! require_launch {
    ($session:expr, $program:expr, $source:expr, $line:expr) => {{
        match launch_with_breakpoint($session, $program, $source, $line)? {
            Some(thread_id) => thread_id,
            None => {
                $session.shutdown();
                return Ok(());
            }
        }
    }};
}

macro_rules! require_frame {
    ($session:expr, $thread_id:expr) => {{
        match first_frame_id($session, $thread_id)? {
            Some(frame_id) => frame_id,
            None => {
                $session.shutdown();
                return Ok(());
            }
        }
    }};
}

/// `handle_variables` in the DAP layer formats each variable's name as
/// `"<name> : <type>"` so IDEs that don't honour the DAP `type` field
/// inline still render the type next to the value (see
/// `src/dap/yadap/session/data.rs`). Tests that look up variables by
/// name should use this matcher instead of an exact equality check so
/// they stay robust to a type that may or may not be present (e.g. a
/// raw lambda capture has no name, and locals always do).
fn var_name_matches(v: &Value, want: &str) -> bool {
    let Some(name) = v["name"].as_str() else {
        return false;
    };
    name == want || name.starts_with(&format!("{want} : "))
}

fn initialize(session: &mut DapSession) -> anyhow::Result<()> {
    let seq = session
        .client
        .send_request("initialize", json!({ "adapterID": "bugstalker" }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "initialize", seq, true);
    let event = session.client.wait_for_event("initialized")?;
    assert_eq!(event.get("type").and_then(Value::as_str), Some("event"));
    Ok(())
}

fn launch_with_breakpoint(
    session: &mut DapSession,
    program: &Path,
    source: &Path,
    line: i64,
) -> anyhow::Result<Option<i64>> {
    initialize(session)?;
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    if !assert_response(&launch_response, "launch", launch_seq, true) {
        return Ok(None);
    }

    let bp_seq = session.client.send_request(
        "setBreakpoints",
        json!({
            "source": { "path": source },
            "breakpoints": [{ "line": line }],
        }),
    )?;
    let bp_response = session.client.read_response(bp_seq)?;
    if !assert_response(&bp_response, "setBreakpoints", bp_seq, true) {
        return Ok(None);
    }

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    if !assert_response(&config_response, "configurationDone", config_seq, true) {
        return Ok(None);
    }

    let stopped = session.client.wait_for_event("stopped")?;
    let thread_id = stopped
        .get("body")
        .and_then(|body| body.get("threadId"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    Ok(Some(thread_id))
}

fn first_frame_id(session: &mut DapSession, thread_id: i64) -> anyhow::Result<Option<i64>> {
    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack_response = session.client.read_response(stack_seq)?;
    if !assert_response(&stack_response, "stackTrace", stack_seq, true) {
        return Ok(None);
    }
    let frame_id = stack_response["body"]["stackFrames"][0]["id"]
        .as_i64()
        .unwrap_or_default();
    Ok(Some(frame_id))
}

fn top_frame_line(session: &mut DapSession, thread_id: i64) -> anyhow::Result<Option<i64>> {
    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack_response = session.client.read_response(stack_seq)?;
    if !assert_response(&stack_response, "stackTrace", stack_seq, true) {
        return Ok(None);
    }
    Ok(stack_response["body"]["stackFrames"][0]["line"].as_i64())
}

/// Top stack frame's source path (DAP `stackFrames[0].source.path`), or
/// `None` if the frame has no source (e.g. a stripped/library frame).
fn top_frame_source_path(
    session: &mut DapSession,
    thread_id: i64,
) -> anyhow::Result<Option<String>> {
    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack_response = session.client.read_response(stack_seq)?;
    if !assert_response(&stack_response, "stackTrace", stack_seq, true) {
        return Ok(None);
    }
    Ok(stack_response["body"]["stackFrames"][0]["source"]["path"]
        .as_str()
        .map(str::to_owned))
}

fn wait_for_event_or_terminated(
    session: &mut DapSession,
    event_name: &str,
    timeout: Duration,
) -> anyhow::Result<Option<Value>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let event = match session.client.read_event_with_timeout(remaining)? {
            Some(event) => event,
            None => return Ok(None),
        };
        match event.get("event").and_then(Value::as_str) {
            Some(name) if name == event_name => return Ok(Some(event)),
            Some("exited") | Some("terminated") => return Ok(None),
            _ => continue,
        }
    }
}

fn replay_manifest() -> Manifest {
    Manifest {
        format_version: FormatVersion::V1,
        build_id: "deadbeef".repeat(8),
        kernel_release: "test".to_owned(),
        cpu_features: vec![],
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        initial_env: vec![],
        initial_cwd: "/tmp".to_owned(),
        initial_args: vec![],
        recorded_at: None,
        initial_fds: vec![],
    }
}

fn temp_replay_trace(label: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("bs-dap-replay-{label}-{}", std::process::id(),));
    let _ = fs::remove_dir_all(&dir);

    let mut writer = TraceWriter::create(&dir, &replay_manifest())?;
    writer.write_event(Event::Marker { tag: 1, data: 0 })?;
    writer.write_event(Event::Marker { tag: 2, data: 0 })?;
    writer.write_event(Event::Marker { tag: 3, data: 0 })?;
    writer.finish()?;
    Ok(dir)
}

fn bs_viz_spec_test_binary() -> anyhow::Result<PathBuf> {
    let output = Command::new("cargo")
        .args([
            "test",
            "-p",
            "bs-viz-spec",
            "--lib",
            "--no-run",
            "--message-format=json",
        ])
        .current_dir(dap_client::repo_root())
        .output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to build bs-viz-spec test binary: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_artifact = msg.get("reason").and_then(Value::as_str) == Some("compiler-artifact");
        let is_bs_viz_spec = msg
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(Value::as_str)
            == Some("bs_viz_spec");
        let is_test_executable = msg
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            == Some(true);
        if is_artifact
            && is_bs_viz_spec
            && is_test_executable
            && let Some(executable) = msg.get("executable").and_then(Value::as_str)
        {
            return Ok(PathBuf::from(executable));
        }
    }

    Err(anyhow::anyhow!(
        "cargo did not report the bs-viz-spec test executable"
    ))
}

fn bs_viz_spec_target_test_binary() -> anyhow::Result<Option<PathBuf>> {
    let Some(target) = edit_continue_target() else {
        eprintln!("skipping target breakpoint diagnostic: unsupported target platform");
        return Ok(None);
    };

    let target_dir =
        std::env::temp_dir().join(format!("bugstalker-target-dap-test-{}", std::process::id()));
    let output = Command::new("cargo")
        .args([
            "test",
            "-p",
            "bs-viz-spec",
            "--lib",
            "--no-run",
            "--target",
            target,
            "--message-format=json",
        ])
        .current_dir(dap_client::repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to build target bs-viz-spec test binary: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_artifact = msg.get("reason").and_then(Value::as_str) == Some("compiler-artifact");
        let is_bs_viz_spec = msg
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(Value::as_str)
            == Some("bs_viz_spec");
        let is_test_executable = msg
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            == Some(true);
        if is_artifact
            && is_bs_viz_spec
            && is_test_executable
            && let Some(executable) = msg.get("executable").and_then(Value::as_str)
        {
            return Ok(Some(PathBuf::from(executable)));
        }
    }

    Err(anyhow::anyhow!(
        "cargo did not report the target bs-viz-spec test executable"
    ))
}

fn bs_viz_spec_edit_continue_rustflags_test_binary() -> anyhow::Result<Option<PathBuf>> {
    let Some(target) = edit_continue_target() else {
        eprintln!(
            "skipping edit-and-continue rustflags breakpoint diagnostic: unsupported target platform"
        );
        return Ok(None);
    };

    let target_dir = std::env::temp_dir().join(format!(
        "bugstalker-enc-rustflags-dap-test-{}",
        std::process::id()
    ));
    let output = Command::new("cargo")
        .args([
            "test",
            "-p",
            "bs-viz-spec",
            "--lib",
            "--no-run",
            "--target",
            target,
            "--message-format=json",
        ])
        .current_dir(dap_client::repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .env(
            cargo_target_rustflags_env(target),
            "-C symbol-mangling-version=v0 -C linker=clang",
        )
        .output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to build edit-and-continue rustflags bs-viz-spec test binary: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_artifact = msg.get("reason").and_then(Value::as_str) == Some("compiler-artifact");
        let is_bs_viz_spec = msg
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(Value::as_str)
            == Some("bs_viz_spec");
        let is_test_executable = msg
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            == Some(true);
        if is_artifact
            && is_bs_viz_spec
            && is_test_executable
            && let Some(executable) = msg.get("executable").and_then(Value::as_str)
        {
            return Ok(Some(PathBuf::from(executable)));
        }
    }

    Err(anyhow::anyhow!(
        "cargo did not report the edit-and-continue rustflags bs-viz-spec test executable"
    ))
}

fn bs_viz_spec_edit_continue_test_binary() -> anyhow::Result<Option<PathBuf>> {
    let Some(linker) = edit_continue_linker() else {
        eprintln!("skipping edit-and-continue breakpoint diagnostic: wild linker not found");
        return Ok(None);
    };
    let Some(target) = edit_continue_target() else {
        eprintln!("skipping edit-and-continue breakpoint diagnostic: unsupported target platform");
        return Ok(None);
    };

    let target_dir =
        std::env::temp_dir().join(format!("bugstalker-enc-dap-test-{}", std::process::id()));
    let patch_path = target_dir.join("bugstalker.wild-patch");
    let rustflags = format!(
        "-C symbol-mangling-version=v0 \
         -C linker=clang \
         -C link-arg=-fuse-ld={} \
         -C link-arg=-Wl,--incremental-cache=read-write \
         -C link-arg=-Wl,--emit-patch={}",
        linker.display(),
        patch_path.display()
    );
    let output = Command::new("cargo")
        .args([
            "test",
            "-p",
            "bs-viz-spec",
            "--lib",
            "--no-run",
            "--target",
            target,
            "--message-format=json",
        ])
        .current_dir(dap_client::repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .env(cargo_target_rustflags_env(target), rustflags)
        .output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to build edit-and-continue bs-viz-spec test binary: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_artifact = msg.get("reason").and_then(Value::as_str) == Some("compiler-artifact");
        let is_bs_viz_spec = msg
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(Value::as_str)
            == Some("bs_viz_spec");
        let is_test_executable = msg
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            == Some(true);
        if is_artifact
            && is_bs_viz_spec
            && is_test_executable
            && let Some(executable) = msg.get("executable").and_then(Value::as_str)
        {
            return Ok(Some(PathBuf::from(executable)));
        }
    }

    Err(anyhow::anyhow!(
        "cargo did not report the edit-and-continue bs-viz-spec test executable"
    ))
}

fn edit_continue_linker() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BUGSTALKER_WILD_LINKER") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    [
        dap_client::repo_root().join("../linker/target/release/wild"),
        dap_client::repo_root().join("../linker/target/debug/wild"),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn edit_continue_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

fn cargo_target_rustflags_env(target: &str) -> String {
    format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target.to_uppercase().replace('-', "_")
    )
}

fn assert_bs_viz_spec_breakpoint_binds_to_requested_statement(
    program: &Path,
) -> anyhow::Result<()> {
    let source = example_source("crates/bs-viz-spec/src/lib.rs");
    let mut session = DapSession::start()?;
    initialize(&mut session)?;

    let launch_seq = session.client.send_request(
        "launch",
        json!({
            "program": program,
            "args": ["roundtrip_one", "--nocapture"],
        }),
    )?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let bp_seq = session.client.send_request(
        "setBreakpoints",
        json!({
            "source": { "path": source },
            "breakpoints": [{ "line": BS_VIZ_SPEC_REQUESTED_COMMENT_LINE }],
        }),
    )?;
    let bp_response = session.client.read_response(bp_seq)?;
    ensure_response!(session, &bp_response, "setBreakpoints", bp_seq, true);
    let bp = &bp_response["body"]["breakpoints"][0];
    assert_eq!(bp["verified"].as_bool(), Some(true), "{bp_response}");
    assert_eq!(
        bp["line"].as_i64(),
        Some(BS_VIZ_SPEC_BOUND_STATEMENT_LINE),
        "breakpoint must slide from the comment at line {BS_VIZ_SPEC_REQUESTED_COMMENT_LINE} \
         to Format::from_tag, not to the unrelated layout line 25: {bp_response}"
    );

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(
        session,
        &config_response,
        "configurationDone",
        config_seq,
        true
    );

    let stopped = session.client.wait_for_event("stopped")?;
    let thread_id = stopped
        .get("body")
        .and_then(|body| body.get("threadId"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let stopped_line = top_frame_line(&mut session, thread_id)?;
    assert_eq!(
        stopped_line,
        Some(BS_VIZ_SPEC_BOUND_STATEMENT_LINE),
        "debuggee stopped at the wrong source line; stopped event was {stopped}"
    );

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_initialize_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_launch_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "launch", seq, true);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_attach_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let mut target = spawn_attach_target(&example_bin("dap_attach"))?;
    initialize(&mut session)?;
    let seq = session
        .client
        .send_request("attach", json!({ "pid": target.id() }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "attach", seq, true);
    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(
        session,
        &config_response,
        "configurationDone",
        config_seq,
        true
    );
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    let _ = wait_for_exit(&mut target, Duration::from_secs(1))
        .or_else(|_| target.kill().map_err(anyhow::Error::from));
    Ok(())
}

#[test]
#[serial]
fn test_configuration_done_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    assert!(thread_id > 0);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_breakpoints_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let source = example_source("examples/hello_world/src/hello_world.rs");
    let bp_seq = session.client.send_request(
        "setBreakpoints",
        json!({
            "source": { "path": source },
            "breakpoints": [{ "line": HELLO_LINE }],
        }),
    )?;
    let bp_response = session.client.read_response(bp_seq)?;
    ensure_response!(session, &bp_response, "setBreakpoints", bp_seq, true);
    assert!(bp_response["body"]["breakpoints"].is_array());
    let event = session.client.wait_for_event("breakpoint")?;
    assert_eq!(event["event"], "breakpoint");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_breakpoint_slides_from_blank_line() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let bp_seq = session.client.send_request(
        "setBreakpoints",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "breakpoints": [{ "line": 3 }],
        }),
    )?;
    let bp_response = session.client.read_response(bp_seq)?;
    ensure_response!(session, &bp_response, "setBreakpoints", bp_seq, true);
    let bp = &bp_response["body"]["breakpoints"][0];
    assert_eq!(bp["verified"].as_bool(), Some(true));
    assert!(
        bp["line"].as_i64().unwrap_or_default() > 3,
        "breakpoint should bind to the next statement: {bp_response}"
    );

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_bs_viz_spec_breakpoint_binds_to_requested_file_statement() -> anyhow::Result<()> {
    let program = bs_viz_spec_test_binary()?;
    assert_bs_viz_spec_breakpoint_binds_to_requested_statement(&program)
}

#[test]
#[serial]
fn test_bs_viz_spec_breakpoint_with_explicit_target() -> anyhow::Result<()> {
    let Some(program) = bs_viz_spec_target_test_binary()? else {
        return Ok(());
    };
    assert_bs_viz_spec_breakpoint_binds_to_requested_statement(&program)
}

#[test]
#[serial]
fn test_bs_viz_spec_breakpoint_with_edit_continue_rustflags() -> anyhow::Result<()> {
    let Some(program) = bs_viz_spec_edit_continue_rustflags_test_binary()? else {
        return Ok(());
    };
    assert_bs_viz_spec_breakpoint_binds_to_requested_statement(&program)
}

#[test]
#[serial]
fn test_bs_viz_spec_breakpoint_with_edit_continue_linker() -> anyhow::Result<()> {
    let Some(program) = bs_viz_spec_edit_continue_test_binary()? else {
        return Ok(());
    };
    assert_bs_viz_spec_breakpoint_binds_to_requested_statement(&program)
}

#[test]
#[serial]
fn test_set_function_breakpoints_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let seq = session.client.send_request(
        "setFunctionBreakpoints",
        json!({ "breakpoints": [{ "name": "myprint" }] }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setFunctionBreakpoints", seq, true);
    assert!(response["body"]["breakpoints"].is_array());
    let _ = session.client.wait_for_event("breakpoint")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_instruction_breakpoints_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "line": HELLO_LINE,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "gotoTargets", seq, true);
    let target = &response["body"]["targets"][0];
    let instruction = target["instructionPointerReference"]
        .as_str()
        .unwrap_or("0x0");

    let ibp_seq = session.client.send_request(
        "setInstructionBreakpoints",
        json!({
            "breakpoints": [{ "instructionReference": instruction }],
        }),
    )?;
    let ibp_response = session.client.read_response(ibp_seq)?;
    ensure_response!(
        session,
        &ibp_response,
        "setInstructionBreakpoints",
        ibp_seq,
        true
    );
    assert!(ibp_response["body"]["breakpoints"].is_array());
    assert!(thread_id > 0);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_exception_breakpoints_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let seq = session
        .client
        .send_request("setExceptionBreakpoints", json!({ "filters": ["signal"] }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setExceptionBreakpoints", seq, true);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_threads_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("threads", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "threads", seq, true);
    assert!(response["body"]["threads"].is_array());
    assert!(thread_id > 0);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_stack_trace_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stackTrace", seq, true);
    assert!(response["body"]["stackFrames"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_scopes_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "scopes", seq, true);
    assert!(response["body"]["scopes"].is_array());
    session.shutdown();
    Ok(())
}

/// Lazy file-scope scopes (variables-view perf, design-principles.md
/// §2). Statics / Thread-locals must advertise `expensive: true` so the
/// client doesn't auto-expand them — enumerating every static's value
/// on each stop is the per-step cost we moved off the hot path. Locals /
/// Arguments stay cheap (`expensive: false`). Expanding the deferred
/// scope must still return a variables array, proving the lazy
/// population fires on demand.
#[test]
#[serial]
fn test_file_scope_scopes_are_lazy() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "scopes", seq, true);

    let scopes = response["body"]["scopes"].as_array().cloned().unwrap();
    let expensive_of = |name: &str| -> Option<bool> {
        scopes
            .iter()
            .find(|s| s["name"] == name)
            .and_then(|s| s["expensive"].as_bool())
    };
    assert_eq!(expensive_of("Statics"), Some(true), "Statics must be lazy");
    assert_eq!(
        expensive_of("Thread-locals"),
        Some(true),
        "Thread-locals must be lazy"
    );
    assert_eq!(expensive_of("Locals"), Some(false));
    assert_eq!(expensive_of("Arguments"), Some(false));

    // Expanding the deferred Statics scope must still resolve — this is
    // the on-demand enumeration path.
    let statics_ref = scopes
        .iter()
        .find(|s| s["name"] == "Statics")
        .and_then(|s| s["variablesReference"].as_i64())
        .unwrap_or(0);
    assert!(statics_ref != 0, "Statics scope needs a real reference");
    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": statics_ref }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "variables", seq, true);
    // (hello_world links only std, which is precompiled without debug
    // info, so its all-crates set can legitimately be empty — the
    // all-crates *broadening* is guarded by `test_thin_crate_sees_dep_statics`.)
    assert!(response["body"]["variables"].is_array());

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_variables_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let scopes_seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes_response = session.client.read_response(scopes_seq)?;
    ensure_response!(session, &scopes_response, "scopes", scopes_seq, true);
    let locals_ref = scopes_response["body"]["scopes"][0]["variablesReference"]
        .as_i64()
        .unwrap_or(0);

    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": locals_ref }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "variables", seq, true);
    assert!(response["body"]["variables"].is_array());
    session.shutdown();
    Ok(())
}

/// Smoke guard for the variables-view file-scope read path: building
/// the DAP `scopes` response eagerly enumerates every file-scope
/// static + thread-local in the current crate (`handle_scopes` →
/// `file_scope_var_items` → `query_file_scope` → `root_from_die` →
/// `read_value` → `into_raw_bytes`), and the field crash (capacity
/// overflow, seen as `adapter-error: connection closed`) happened
/// right there. This stops inside `dap_cache_vars::main` and requests
/// `scopes` *and* the `variables` of every scope, so a panic anywhere
/// in that path surfaces as a connection-closed error and fails the
/// test.
///
/// NB: this does not by itself reproduce the original capacity
/// overflow — that needs a `DW_AT_upper_bound = -1` array DIE, which
/// rustc doesn't emit (it uses `DW_AT_count`); it comes from C/`-sys`
/// debug info. The overflow arithmetic itself is covered directly by
/// the unit test on `array_byte_size` in `dwarf::r#type`.
#[test]
#[serial]
fn test_file_scope_enumeration_no_crash() -> anyhow::Result<()> {
    // Ensure the debuggee exists (default build).
    let status = Command::new("cargo")
        .args(["build", "-p", "dap_cache_vars"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build dap_cache_vars example");
    }

    let mut session = DapSession::start()?;
    // Line 25 is the `println!` in dap_cache_vars/main.rs — after
    // `outer` is fully built, so the stop is in a user frame and the
    // current-crate file-scope filter resolves.
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_cache_vars"),
        &example_source("examples/dap_cache_vars/src/main.rs"),
        25
    );
    let frame_id = require_frame!(&mut session, thread_id);

    // The crash path: `scopes` builds the file-scope scopes eagerly.
    let scopes_seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes_response = session.client.read_response(scopes_seq)?;
    ensure_response!(session, &scopes_response, "scopes", scopes_seq, true);

    // Drive the `variables` read of every scope (Locals, Statics,
    // Thread-locals, …) so the file-scope value read is exercised
    // even where it's deferred to the `variables` request.
    let scopes = scopes_response["body"]["scopes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for scope in scopes {
        let vref = scope["variablesReference"].as_i64().unwrap_or(0);
        if vref == 0 {
            continue;
        }
        let seq = session
            .client
            .send_request("variables", json!({ "variablesReference": vref }))?;
        let response = session.client.read_response(seq)?;
        ensure_response!(session, &response, "variables", seq, true);
        assert!(
            response["body"]["variables"].is_array(),
            "variables for scope {} not an array: {response}",
            scope["name"]
        );
    }

    session.shutdown();
    Ok(())
}

/// Namespace tree (variables-view, design-principles.md §4). Expanding
/// the Statics scope on a static-heavy binary must yield a *tree* keyed
/// by `::` path — collapsible namespace nodes — not a flat dump. Drives
/// `statics_heavy` (4000 statics across `m0..m39`): expanding Statics
/// surfaces the `statics_heavy` crate node; expanding that surfaces the
/// `m*` module nodes; expanding one of those reaches the `RO_*`/`RW_*`
/// leaves.
#[test]
#[serial]
fn test_statics_rendered_as_namespace_tree() -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "statics_heavy"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build statics_heavy example");
    }

    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("statics_heavy"),
        &example_source("examples/statics_heavy/src/main.rs"),
        13
    );
    let frame_id = require_frame!(&mut session, thread_id);

    // Descend the tree from the `statics_heavy` crate node down through
    // its `m*` module chain to the `RO_*`/`RW_*` leaves — proving the
    // Statics scope is a navigable tree, not a flat dump, and that it
    // expands level-by-level.
    let leaves = statics_first_chain_leaves(&mut session, frame_id)?;
    assert!(
        leaves
            .iter()
            .any(|n| n.starts_with("RO_") || n.starts_with("RW_")),
        "expected RO_*/RW_* leaf statics by descending the tree; got: {leaves:?}"
    );

    session.shutdown();
    Ok(())
}

/// A giant static (800 KB `BIG_TABLE`) must be shown as a *lazy* leaf —
/// a size summary + expand arrow — not materialised (all 100 000 elements
/// read) when the Statics tree opens. Guards the size-gate that keeps the
/// Statics pane responsive on binaries with precomputed crypto tables.
#[test]
#[serial]
fn test_statics_giant_static_is_lazy() -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "big_static"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build big_static example");
    }
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("big_static"),
        &example_source("examples/big_static/src/main.rs"),
        15
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes = session.client.read_response(seq)?;
    ensure_response!(session, &scopes, "scopes", seq, true);
    let statics_ref = scopes["body"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Statics"))
        .and_then(|s| s["variablesReference"].as_i64())
        .unwrap_or(0);

    // Descend the namespace tree (root → `big_static` → …) collecting the
    // `BIG_TABLE` and `SMALL` leaves. Bounded so a malformed tree can't loop.
    let mut frontier = vec![statics_ref];
    let mut seen = std::collections::HashSet::new();
    let (mut big_row, mut small_row): (Option<Value>, Option<Value>) = (None, None);
    for _ in 0..64 {
        let Some(r) = frontier.pop() else { break };
        if r == 0 || !seen.insert(r) {
            continue;
        }
        let seq = session
            .client
            .send_request("variables", json!({ "variablesReference": r }))?;
        let resp = session.client.read_response(seq)?;
        ensure_response!(session, &resp, "variables", seq, true);
        for row in resp["body"]["variables"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            // The DAP layer renders names as `name : type` (var_name_matches).
            let leaf = row["name"].as_str().unwrap_or("");
            let leaf = leaf.split(" : ").next().unwrap_or(leaf);
            match leaf {
                "BIG_TABLE" => big_row = Some(row.clone()),
                "SMALL" => small_row = Some(row.clone()),
                _ => {}
            }
            // Descend namespace nodes only (value is a `(count)`).
            if let Some(child) = row["variablesReference"].as_i64()
                && row["value"].as_str().is_some_and(|v| v.starts_with('('))
            {
                frontier.push(child);
            }
        }
        if big_row.is_some() && small_row.is_some() {
            break;
        }
    }

    // The giant static is a lazy node — size summary + expand arrow, no
    // eager read of its 100 000 elements.
    let big = big_row.expect("BIG_TABLE leaf should appear in the Statics tree");
    assert!(
        big["value"]
            .as_str()
            .unwrap_or("")
            .contains("expand to load"),
        "giant static must be a lazy node, got value {:?}",
        big["value"]
    );
    assert!(
        big["variablesReference"].as_i64().unwrap_or(0) != 0,
        "lazy giant static must be expandable: {big}"
    );

    // The small static is *not* gated — it still renders inline.
    let small = small_row.expect("SMALL leaf should appear in the Statics tree");
    assert_eq!(
        small["value"].as_str(),
        Some("42"),
        "small static must render its value inline, not lazily: {small}"
    );

    session.shutdown();
    Ok(())
}

/// Lazy skeleton (variables-view, design-principles.md §2). The *first*
/// Statics expand must not materialise every static — it returns only
/// the namespace skeleton, built from names. On `statics_heavy` the root
/// is a single `statics_heavy (4000)` namespace node: the `(4000)` count
/// proves all names were enumerated, and the absence of any `RO_*`/`RW_*`
/// row proves no leaf values were read at this level.
#[test]
#[serial]
fn test_statics_first_expand_is_lazy_skeleton() -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "statics_heavy"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build statics_heavy example");
    }
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("statics_heavy"),
        &example_source("examples/statics_heavy/src/main.rs"),
        13
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes = session.client.read_response(seq)?;
    ensure_response!(session, &scopes, "scopes", seq, true);
    let statics_ref = scopes["body"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Statics"))
        .and_then(|s| s["variablesReference"].as_i64())
        .unwrap_or(0);

    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": statics_ref }))?;
    let resp = session.client.read_response(seq)?;
    ensure_response!(session, &resp, "variables", seq, true);
    let rows = resp["body"]["variables"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    assert!(
        rows.iter().all(|r| {
            let n = r["name"].as_str().unwrap_or("");
            !n.starts_with("RO_") && !n.starts_with("RW_")
        }),
        "root expand materialised leaf statics instead of a skeleton: {:?}",
        rows.iter()
            .map(|r| r["name"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );
    assert!(
        rows.iter().any(|r| r["value"].as_str() == Some("(4000)")),
        "expected a namespace node counting all 4000 statics; got: {:?}",
        rows.iter()
            .map(|r| (
                r["name"].as_str().unwrap_or(""),
                r["value"].as_str().unwrap_or("")
            ))
            .collect::<Vec<_>>()
    );

    session.shutdown();
    Ok(())
}

/// Descend the Statics namespace tree, following the first expandable
/// (namespace) row at each level, and return the leaf names at the
/// first level that actually holds statics (`RO_*` / `RW_*`).
fn statics_first_chain_leaves(
    session: &mut DapSession,
    frame_id: i64,
) -> anyhow::Result<Vec<String>> {
    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let resp = session.client.read_response(seq)?;
    let statics_ref = resp["body"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Statics"))
        .and_then(|s| s["variablesReference"].as_i64())
        .unwrap_or(0);
    // The Statics scope now lists every crate (design-principles.md §4);
    // start from the `statics_heavy` crate node, then descend its chain.
    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": statics_ref }))?;
    let top = session.client.read_response(seq)?;
    let mut vref = top["body"]["variables"]
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|r| {
                r["name"]
                    .as_str()
                    .is_some_and(|n| n == "statics_heavy" || n.starts_with("statics_heavy::"))
            })
        })
        .and_then(|r| r["variablesReference"].as_i64())
        .unwrap_or(0);
    let mut rows = Vec::new();
    for _ in 0..16 {
        let seq = session
            .client
            .send_request("variables", json!({ "variablesReference": vref }))?;
        let r = session.client.read_response(seq)?;
        rows = r["body"]["variables"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let has_leaves = rows.iter().any(|row| {
            row["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("RO_") || n.starts_with("RW_"))
        });
        if has_leaves {
            break;
        }
        match rows
            .iter()
            .find(|row| row["variablesReference"].as_i64().unwrap_or(0) != 0)
            .and_then(|row| row["variablesReference"].as_i64())
        {
            Some(next) => vref = next,
            None => break,
        }
    }
    Ok(rows
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect())
}

/// Immutable-static cache (design-principles.md §3). When the Statics
/// pane is kept open across a step, read-only statics are served from
/// cache and only mutable ones are re-read — but the tree must stay
/// *complete*: a buggy merge could drop the cached read-only half. This
/// expands Statics, steps over one line, re-expands, and asserts the
/// same module still surfaces both `RO_*` (cache) and `RW_*` (re-read)
/// leaves with an unchanged count.
#[test]
#[serial]
fn test_statics_cache_survives_step() -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "statics_heavy"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build statics_heavy example");
    }

    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("statics_heavy"),
        &example_source("examples/statics_heavy/src/main.rs"),
        13
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let before = statics_first_chain_leaves(&mut session, frame_id)?;
    assert!(before.iter().any(|n| n.starts_with("RO_")), "no RO_ before");
    assert!(before.iter().any(|n| n.starts_with("RW_")), "no RW_ before");

    // Step over one line, then re-expand at the new stop.
    let seq = session
        .client
        .send_request("next", json!({ "threadId": thread_id }))?;
    let _ = session.client.read_response(seq)?;
    if wait_for_event_or_terminated(&mut session, "stopped", Duration::from_secs(10))?.is_none() {
        // Program ran to completion — nothing left to assert.
        session.shutdown();
        return Ok(());
    }
    let frame_id = require_frame!(&mut session, thread_id);
    let after = statics_first_chain_leaves(&mut session, frame_id)?;

    assert!(
        after.iter().any(|n| n.starts_with("RO_")),
        "read-only statics missing after step — cache dropped them"
    );
    assert!(
        after.iter().any(|n| n.starts_with("RW_")),
        "mutable statics missing after step"
    );
    assert_eq!(
        before.len(),
        after.len(),
        "statics leaf count changed across a step (before {before:?}, after {after:?})"
    );

    session.shutdown();
    Ok(())
}

/// All-crates Statics default (design-principles.md §4) — the
/// regression behind the field report of an empty pane. A binary with
/// no statics of its own (`thin_app`) must still surface a dependency's
/// statics (`dep_with_static::DEP_STATIC`); the old current-crate-only
/// filter hid them. Reproduces the user's case (a test binary whose
/// interesting statics — `hyper_util::…::__CALLSITE` etc. — all live in
/// dependencies).
#[test]
#[serial]
fn test_thin_crate_sees_dep_statics() -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "thin_app"])
        .current_dir(dap_client::repo_root().join("examples"))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to build thin_app example");
    }
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("thin_app"),
        &example_source("examples/thin_app/src/main.rs"),
        5
    );
    let frame_id = require_frame!(&mut session, thread_id);

    let seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes = session.client.read_response(seq)?;
    ensure_response!(session, &scopes, "scopes", seq, true);
    let statics_ref = scopes["body"]["scopes"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Statics"))
        .and_then(|s| s["variablesReference"].as_i64())
        .unwrap_or(0);

    // The dependency crate appears as a top-level namespace node even
    // though it isn't the current crate.
    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": statics_ref }))?;
    let top = session.client.read_response(seq)?;
    ensure_response!(session, &top, "variables", seq, true);
    let dep_ref = top["body"]["variables"]
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|r| {
                r["name"]
                    .as_str()
                    .is_some_and(|n| n == "dep_with_static" || n.starts_with("dep_with_static::"))
            })
        })
        .and_then(|r| r["variablesReference"].as_i64())
        .unwrap_or(0);
    assert!(
        dep_ref != 0,
        "dependency crate `dep_with_static` missing from Statics: {:?}",
        top["body"]["variables"].as_array().map(|rows| rows
            .iter()
            .map(|r| r["name"].as_str().unwrap_or(""))
            .collect::<Vec<_>>())
    );

    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": dep_ref }))?;
    let leaves = session.client.read_response(seq)?;
    ensure_response!(session, &leaves, "variables", seq, true);
    // The DAP row name is the `name : type` display form
    // (`DEP_STATIC : u64`), so match the leading identifier.
    assert!(
        leaves["body"]["variables"].as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("DEP_STATIC"))
            })
        }),
        "DEP_STATIC not found under dep_with_static; got: {:?}",
        leaves["body"]["variables"]
    );

    session.shutdown();
    Ok(())
}

/// Regression test: walking every local in the showcase example
/// (which exercises most variable shapes BugStalker renders) must
/// not crash bs. Currently reproduces a kill-on-debug seen when
/// `let captured_copy = 10;` is uncommented at showcase main.rs:120
/// — that shifts the stack layout and exposes a panic somewhere in
/// the variable-rendering code. The test:
///
///   1. builds `showcase` so the example binary exists,
///   2. launches it under bs DAP and breaks at the last line of
///      `main` (every local in sections 1..12 is in scope),
///   3. requests `variables` for the top-level locals scope,
///   4. recursively expands every child whose `variablesReference`
///      is non-zero (depth-first walk),
///
/// Any DAP error / EOF during the walk is treated as bs crashing
/// or hanging — the test fails with the captured error. When the
/// underlying panic is fixed the walk completes and the test passes.
///
/// macOS-only: the original kill-on-debug was reproduced with the
/// `wild` linker on `aarch64-apple-darwin`, and this test hard-codes
/// that target triple to build showcase the same way the
/// codelldb-fork extension does. Building for `aarch64-apple-darwin`
/// from a Linux host fails with `error[E0463]: can't find crate for
/// std` because that target's libstd isn't installed there.
#[cfg(target_os = "macos")]
#[test]
#[serial]
fn test_showcase_locals_no_crash() -> anyhow::Result<()> {
    // Build showcase the same way the codelldb-fork extension does
    // when EnC is enabled: with the `wild` linker and the
    // symbol-mangling / emit-patch RUSTFLAGS. The original kill-on-
    // debug report came from that exact build path; default ld64
    // builds may not reproduce it (this is the test's main reason
    // to exist).
    let linker_dir = dap_client::repo_root()
        .parent()
        .map(|p| p.join("linker").join("target").join("release").join("wild"));
    let mut env_args: Vec<(String, String)> = Vec::new();
    let target_triple = "aarch64-apple-darwin";
    if let Some(wild) = linker_dir.as_ref().filter(|p| p.exists()) {
        // CARGO_TARGET_<triple>_RUSTFLAGS — same key the extension uses.
        let key = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            target_triple.to_uppercase().replace('-', "_")
        );
        let rustflags = format!(
            "-C symbol-mangling-version=v0 -C linker=clang -C link-arg=-fuse-ld={} \
             -C link-arg=-Wl,--incremental-cache=read-write",
            wild.display()
        );
        env_args.push((key, rustflags));
    }
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "-p", "showcase", "--target", target_triple])
        .current_dir(dap_client::repo_root().join("examples"));
    // Strip the inherited RUSTFLAGS so cargo's precedence doesn't
    // override our CARGO_TARGET_<triple>_RUSTFLAGS.
    cmd.env_remove("RUSTFLAGS");
    cmd.env_remove("CARGO_ENCODED_RUSTFLAGS");
    for (k, v) in &env_args {
        cmd.env(k, v);
    }
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("failed to build showcase example");
    }

    let showcase_bin = dap_client::repo_root()
        .join("examples")
        .join("target")
        .join(target_triple)
        .join("debug")
        .join("showcase");
    if !showcase_bin.exists() {
        anyhow::bail!(
            "showcase binary not at expected path {} after build",
            showcase_bin.display()
        );
    }
    #[cfg(target_os = "macos")]
    {
        // Showcase has to be debuggable by the spawned bs — give it
        // the get-task-allow entitlement the existing
        // `ensure_example_binaries` codesign would normally apply.
        let _ = Command::new("codesign")
            .args(["--entitlements"])
            .arg(
                dap_client::repo_root()
                    .join("tests")
                    .join("darwin.entitlements"),
            )
            .args(["--force", "--sign", "-"])
            .arg(&showcase_bin)
            .status();
    }

    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &showcase_bin,
        &dap_client::example_source("examples/showcase/src/main.rs"),
        SHOWCASE_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);

    let scopes_seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes_response = session
        .client
        .read_response(scopes_seq)
        .context("scopes response — bs likely crashed")?;
    ensure_response!(session, &scopes_response, "scopes", scopes_seq, true);
    let locals_ref = scopes_response["body"]["scopes"][0]["variablesReference"]
        .as_i64()
        .unwrap_or(0);

    let seq = session
        .client
        .send_request("variables", json!({ "variablesReference": locals_ref }))?;
    let response = session
        .client
        .read_response(seq)
        .context("variables (locals) response — bs likely crashed")?;
    ensure_response!(session, &response, "variables", seq, true);

    // DFS-expand every child reference. The walk is bounded so a
    // bogus self-referential `variablesReference` chain can't loop
    // forever.
    let mut queue: Vec<i64> = response["body"]["variables"]
        .as_array()
        .map(|vars| {
            vars.iter()
                .filter_map(|v| v["variablesReference"].as_i64())
                .filter(|r| *r > 0)
                .collect()
        })
        .unwrap_or_default();
    let mut visited: std::collections::HashSet<i64> = Default::default();
    let mut depth = 0usize;
    const MAX_EXPAND_DEPTH: usize = 8;
    while let Some(r) = queue.pop() {
        if !visited.insert(r) {
            continue;
        }
        depth += 1;
        if depth > MAX_EXPAND_DEPTH * 64 {
            break;
        }
        let s = session
            .client
            .send_request("variables", json!({ "variablesReference": r }))?;
        let resp = session
            .client
            .read_response(s)
            .with_context(|| format!("variables ref={r} — bs likely crashed mid-walk"))?;
        ensure_response!(session, &resp, "variables", s, true);
        if let Some(arr) = resp["body"]["variables"].as_array() {
            for v in arr {
                if let Some(child_ref) = v["variablesReference"].as_i64() {
                    if child_ref > 0 {
                        queue.push(child_ref);
                    }
                }
            }
        }
    }

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_variable_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_set_variable"),
        &example_source("examples/dap_set_variable/src/main.rs"),
        SET_VAR_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let scopes_seq = session
        .client
        .send_request("scopes", json!({ "frameId": frame_id }))?;
    let scopes_response = session.client.read_response(scopes_seq)?;
    ensure_response!(session, &scopes_response, "scopes", scopes_seq, true);
    let locals_ref = scopes_response["body"]["scopes"][0]["variablesReference"]
        .as_i64()
        .unwrap_or(0);

    let vars_seq = session
        .client
        .send_request("variables", json!({ "variablesReference": locals_ref }))?;
    let vars_response = session.client.read_response(vars_seq)?;
    ensure_response!(session, &vars_response, "variables", vars_seq, true);
    let container = vars_response["body"]["variables"]
        .as_array()
        .and_then(|vars| vars.iter().find(|v| var_name_matches(v, "container")))
        .cloned()
        .unwrap();
    let container_ref = container["variablesReference"].as_i64().unwrap_or(0);

    let point_seq = session
        .client
        .send_request("variables", json!({ "variablesReference": container_ref }))?;
    let point_response = session.client.read_response(point_seq)?;
    ensure_response!(session, &point_response, "variables", point_seq, true);
    let point = point_response["body"]["variables"]
        .as_array()
        .and_then(|vars| vars.iter().find(|v| var_name_matches(v, "point")))
        .cloned()
        .unwrap();
    let point_ref = point["variablesReference"].as_i64().unwrap_or(0);

    let seq = session.client.send_request(
        "setVariable",
        json!({
            "variablesReference": point_ref,
            "name": "x",
            "value": "42",
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setVariable", seq, true);
    assert!(response["body"]["value"].as_str().is_some());
    let _ = session.client.wait_for_event("invalidated")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_evaluate_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_set_variable"),
        &example_source("examples/dap_set_variable/src/main.rs"),
        SET_VAR_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session.client.send_request(
        "evaluate",
        json!({ "expression": "container.point.x", "frameId": frame_id }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "evaluate", seq, true);
    assert!(response["body"]["result"].as_str().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_expression_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_set_variable"),
        &example_source("examples/dap_set_variable/src/main.rs"),
        SET_VAR_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session.client.send_request(
        "setExpression",
        json!({
            "expression": "container.point.x",
            "value": "41",
            "frameId": frame_id,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setExpression", seq, true);
    assert!(response["body"]["value"].as_str().is_some());
    let _ = session.client.wait_for_event("invalidated")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_continue_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("continue", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "continue", seq, true);
    let _ = wait_for_event_or_terminated(&mut session, "continued", OPTIONAL_EVENT_TIMEOUT)?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_next_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("next", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "next", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

/// `next` with `granularity: "instruction"` (Disassembly View) advances a single
/// machine instruction, not a whole source line — the PC moves by ~one
/// instruction's worth, not a line's.
#[test]
#[serial]
fn test_next_instruction_granularity() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let pc_of = |session: &mut DapSession| -> anyhow::Result<Option<u64>> {
        let seq = session
            .client
            .send_request("stackTrace", json!({ "threadId": thread_id }))?;
        let resp = session.client.read_response(seq)?;
        Ok(resp["body"]["stackFrames"][0]["instructionPointerReference"]
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()))
    };
    let before = pc_of(&mut session)?;

    let seq = session.client.send_request(
        "next",
        json!({ "threadId": thread_id, "granularity": "instruction" }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "next", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    let after = pc_of(&mut session)?;
    if let (Some(a), Some(b)) = (before, after) {
        let delta = b.abs_diff(a);
        assert!(
            delta != 0 && delta <= 64,
            "instruction-granularity step moved the PC {delta} bytes; expected \
             ~one instruction (≤64), not a whole source line",
        );
    }
    session.shutdown();
    Ok(())
}

/// `bs/setAsmFocus {focused:true}` makes a plain `next` (no granularity field)
/// step one machine instruction, matching the behaviour VS Code's Disassembly
/// View gets from `granularity:"instruction"`.
/// Also checks that the stopped event carries `preserveFocusHint:true` so VS
/// Code does not steal focus from the asm panel between steps.
#[test]
#[serial]
fn test_asm_focus_next_steps_instruction() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );

    let seq = session
        .client
        .send_request("bs/setAsmFocus", json!({ "focused": true }))?;
    let resp = session.client.read_response(seq)?;
    assert!(resp["success"].as_bool().unwrap_or(false), "bs/setAsmFocus failed");

    let pc_of = |s: &mut DapSession| -> anyhow::Result<Option<u64>> {
        let seq = s.client.send_request("stackTrace", json!({ "threadId": thread_id }))?;
        let resp = s.client.read_response(seq)?;
        Ok(resp["body"]["stackFrames"][0]["instructionPointerReference"]
            .as_str()
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()))
    };
    let before = pc_of(&mut session)?;

    let seq = session.client.send_request("next", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "next", seq, true);

    let stopped = session.client.wait_for_event("stopped")?;
    assert_eq!(
        stopped["body"]["preserveFocusHint"].as_bool(),
        Some(true),
        "instruction step must set preserveFocusHint:true to keep asm panel focused",
    );

    let after = pc_of(&mut session)?;
    if let (Some(a), Some(b)) = (before, after) {
        let delta = b.abs_diff(a);
        assert!(
            delta != 0 && delta <= 64,
            "bs/setAsmFocus next moved {delta} bytes — expected ≤64 (one instruction), not a source line",
        );
    }
    session.shutdown();
    Ok(())
}

/// Same as above but for `stepIn` — asm focus must redirect it to a single
/// instruction step, not a source-level descent.
#[test]
#[serial]
fn test_asm_focus_step_in_steps_instruction() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );

    let seq = session
        .client
        .send_request("bs/setAsmFocus", json!({ "focused": true }))?;
    let resp = session.client.read_response(seq)?;
    assert!(resp["success"].as_bool().unwrap_or(false), "bs/setAsmFocus failed");

    let pc_of = |s: &mut DapSession| -> anyhow::Result<Option<u64>> {
        let seq = s.client.send_request("stackTrace", json!({ "threadId": thread_id }))?;
        let resp = s.client.read_response(seq)?;
        Ok(resp["body"]["stackFrames"][0]["instructionPointerReference"]
            .as_str()
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()))
    };
    let before = pc_of(&mut session)?;

    let seq = session.client.send_request("stepIn", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    let after = pc_of(&mut session)?;
    if let (Some(a), Some(b)) = (before, after) {
        let delta = b.abs_diff(a);
        assert!(
            delta != 0 && delta <= 64,
            "bs/setAsmFocus stepIn moved {delta} bytes — expected ≤64 (one instruction), not a source descent",
        );
    }
    session.shutdown();
    Ok(())
}

/// The `bs/stepIn` custom request (alt+right / shift+alt+right keybindings)
/// must also honour asm focus: with the panel focused it steps one instruction
/// rather than running its skip-libraries source descent — otherwise per-step
/// instruction counts are inflated by a whole line's worth of work.
#[test]
#[serial]
fn test_asm_focus_bs_step_in_steps_instruction() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );

    let seq = session
        .client
        .send_request("bs/setAsmFocus", json!({ "focused": true }))?;
    let resp = session.client.read_response(seq)?;
    assert!(resp["success"].as_bool().unwrap_or(false), "bs/setAsmFocus failed");

    let pc_of = |s: &mut DapSession| -> anyhow::Result<Option<u64>> {
        let seq = s.client.send_request("stackTrace", json!({ "threadId": thread_id }))?;
        let resp = s.client.read_response(seq)?;
        Ok(resp["body"]["stackFrames"][0]["instructionPointerReference"]
            .as_str()
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()))
    };
    let before = pc_of(&mut session)?;

    // shift+alt+right maps to bs/stepIn with skipLibraries:false.
    let seq = session.client.send_request(
        "bs/stepIn",
        json!({ "threadId": thread_id, "skipLibraries": false }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "bs/stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    let after = pc_of(&mut session)?;
    if let (Some(a), Some(b)) = (before, after) {
        let delta = b.abs_diff(a);
        assert!(
            delta != 0 && delta <= 64,
            "asm-focus bs/stepIn moved {delta} bytes — expected ≤64 (one instruction), not a skip-libs source descent",
        );
    }
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_step_in_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("stepIn", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

// `step_into_jmc` fixture line numbers (1-indexed). Keep in sync with
// `examples/step_into_jmc/src/main.rs`.
const JMC_SOURCE: &str = "examples/step_into_jmc/src/main.rs";
const JMC_LINE_A: i64 = 22; // entirely-library call (`to_uppercase`)
const JMC_LINE_B: i64 = 26; // library call invoking a user closure
const JMC_LINE_PRINTLN: i64 = 28; // the user line after LINE B

/// `bs/stepIn { skipLibraries: true }` on an entirely-library call must
/// behave as Step-Over: stop on the next *user* line, never inside
/// alloc/core.
#[test]
#[serial]
fn test_bs_step_in_skip_libraries_steps_over_library_call() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("step_into_jmc"),
        &example_source(JMC_SOURCE),
        JMC_LINE_A
    );

    let seq = session.client.send_request(
        "bs/stepIn",
        json!({ "threadId": thread_id, "skipLibraries": true }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "bs/stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    // Landed back in the user frame on the next user line, not in core.
    let path = top_frame_source_path(&mut session, thread_id)?;
    assert_eq!(
        path.as_deref(),
        Some(example_source(JMC_SOURCE).to_string_lossy().as_ref()),
        "skip-libraries step-in should stop in the user source file"
    );
    assert_eq!(top_frame_line(&mut session, thread_id)?, Some(JMC_LINE_B));

    session.shutdown();
    Ok(())
}

/// `bs/stepIn { skipLibraries: false }` preserves classic Step-In:
/// descend into the library frame the line calls. The contrast with the
/// skip-libraries test above is the whole point of the feature.
#[test]
#[serial]
fn test_bs_step_in_any_frame_descends_into_library() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("step_into_jmc"),
        &example_source(JMC_SOURCE),
        JMC_LINE_A
    );

    let seq = session.client.send_request(
        "bs/stepIn",
        json!({ "threadId": thread_id, "skipLibraries": false }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "bs/stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    // We descended somewhere out of `main` — definitely not stopped on
    // the next user line. (A library frame may have no source path at
    // all, so assert the negative: not the user line in the fixture.)
    let path = top_frame_source_path(&mut session, thread_id)?;
    let user_src = example_source(JMC_SOURCE).to_string_lossy().into_owned();
    let line = top_frame_line(&mut session, thread_id)?;
    let stopped_on_user_next_line =
        path.as_deref() == Some(user_src.as_str()) && line == Some(JMC_LINE_B);
    assert!(
        !stopped_on_user_next_line,
        "any-frame step-in should descend into the library, not step over it \
         (path={path:?}, line={line:?})"
    );

    session.shutdown();
    Ok(())
}

/// MVP engine: `bs/stepIn { skipLibraries: true }` on a line whose
/// library call invokes a *user closure* currently steps OVER the call
/// (it does not yet stop in the callback). Pin that behaviour so the
/// phase-4 engine upgrade — which flips this to stop in `user_fn` — is a
/// deliberate, visible change. See `doc/plans/phase-12-step-into-just-my-code.md`.
#[test]
#[serial]
fn test_bs_step_in_skip_libraries_callback_is_stepped_over_mvp() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("step_into_jmc"),
        &example_source(JMC_SOURCE),
        JMC_LINE_B
    );

    let seq = session.client.send_request(
        "bs/stepIn",
        json!({ "threadId": thread_id, "skipLibraries": true }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "bs/stepIn", seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    // MVP: stepped over the whole iterator expression, landing on the
    // next user line in `main` — never inside `user_fn`.
    let path = top_frame_source_path(&mut session, thread_id)?;
    assert_eq!(
        path.as_deref(),
        Some(example_source(JMC_SOURCE).to_string_lossy().as_ref()),
        "MVP skip-libraries should stay in the user source file"
    );
    assert_eq!(
        top_frame_line(&mut session, thread_id)?,
        Some(JMC_LINE_PRINTLN),
        "MVP skip-libraries should step over the callback to the next user line"
    );

    session.shutdown();
    Ok(())
}

/// Break-on-panic: the auto-trap fires in the panic runtime, so frame 0 is
/// `core::panicking::…`. The stack-trace must deemphasize the panic
/// machinery (so VS Code skips it) while leaving the user frame that
/// panicked focusable — that's what puts the editor on the `.unwrap()`
/// instead of in toolchain `panicking.rs`.
#[test]
#[serial]
fn test_break_on_panic_deemphasizes_runtime_keeps_culprit() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;

    let launch_seq = session.client.send_request(
        "launch",
        json!({ "program": example_bin("panic"), "args": ["user"] }),
    )?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(
        session,
        &config_response,
        "configurationDone",
        config_seq,
        true
    );

    // The default auto-traps stop the program in the panic runtime.
    let Some(stopped) =
        wait_for_event_or_terminated(&mut session, "stopped", OPTIONAL_EVENT_TIMEOUT)?
    else {
        // No auto-trap stop on this platform/toolchain — nothing to assert.
        session.shutdown();
        return Ok(());
    };
    let thread_id = stopped["body"]["threadId"].as_i64().unwrap_or_default();

    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack = session.client.read_response(stack_seq)?;
    ensure_response!(session, &stack, "stackTrace", stack_seq, true);
    let frames = stack["body"]["stackFrames"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // First user frame = first whose source resolves to the fixture.
    let Some(user_idx) = frames.iter().position(|f| {
        f["source"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("panic.rs"))
    }) else {
        // User frame's source didn't resolve (stripped) — can't assert focus.
        session.shutdown();
        return Ok(());
    };

    // The culprit user frame must stay focusable: it keeps a real,
    // navigable source and is not rendered subtle.
    assert!(
        frames[user_idx]["source"]["path"].is_string(),
        "the panicking user frame must keep a navigable source: {}",
        frames[user_idx]
    );
    assert_ne!(
        frames[user_idx]["presentationHint"].as_str(),
        Some("subtle"),
        "the user frame that panicked must not be deemphasized: {}",
        frames[user_idx]
    );
    // … and the panic machinery above it must be non-navigable (no `source`)
    // and greyed (`subtle`), so VS Code's on-stop reveal skips it and walks
    // down to the culprit instead of popping a toolchain tab.
    assert!(
        user_idx > 0,
        "expected panic-runtime frames above the user frame, got {frames:?}"
    );
    let suppressed_above = frames[..user_idx]
        .iter()
        .all(|f| f["source"].is_null() && f["presentationHint"].as_str() == Some("subtle"));
    assert!(
        suppressed_above,
        "panic-runtime frames above the culprit must be non-navigable + subtle: {frames:?}"
    );

    session.shutdown();
    Ok(())
}

/// Break-on-panic: the culprit frame's reported line must be the *exact*
/// panic site from the `#[track_caller]` `&Location`, not the imprecise
/// DWARF statement line. `panic_kinds str` panics at `main.rs:24:5`; the
/// fixture pins that line, so the stackTrace must report it for the user
/// frame even though the enclosing statement's DWARF line can differ.
#[test]
#[serial]
fn test_break_on_panic_reports_exact_location() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;

    let launch_seq = session.client.send_request(
        "launch",
        json!({ "program": example_bin("panic_kinds"), "args": ["str"] }),
    )?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(
        session,
        &config_response,
        "configurationDone",
        config_seq,
        true
    );

    let Some(stopped) =
        wait_for_event_or_terminated(&mut session, "stopped", OPTIONAL_EVENT_TIMEOUT)?
    else {
        // No auto-trap stop on this platform/toolchain — nothing to assert.
        session.shutdown();
        return Ok(());
    };
    let thread_id = stopped["body"]["threadId"].as_i64().unwrap_or_default();

    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack = session.client.read_response(stack_seq)?;
    ensure_response!(session, &stack, "stackTrace", stack_seq, true);
    let frames = stack["body"]["stackFrames"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let Some(culprit_idx) = frames.iter().position(|f| {
        f["source"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("panic_kinds/src/main.rs"))
    }) else {
        // Fixture source didn't resolve (stripped) — can't assert the line.
        session.shutdown();
        return Ok(());
    };
    let culprit = &frames[culprit_idx];

    // The exact panic site: `panic!("boom")` on line 24, col 5.
    assert_eq!(
        culprit["line"].as_i64(),
        Some(24),
        "culprit frame must report the exact panic line from &Location: {culprit}"
    );
    assert_eq!(
        culprit["column"].as_i64(),
        Some(5),
        "culprit frame must report the exact panic column from &Location: {culprit}"
    );

    // Every panic-runtime frame *above* the culprit must be non-navigable
    // (no `source.path`, no `source.sourceReference`) so VS Code's on-stop
    // reveal can't pop a toolchain tab (`panic_info.rs`/`panicking.rs`); it
    // falls through to the culprit instead. They stay visible as greyed,
    // name-only labels.
    for (i, f) in frames[..culprit_idx].iter().enumerate() {
        assert!(
            f["source"]["path"].is_null() && f["source"]["sourceReference"].is_null(),
            "frame #{i} above the culprit must be non-navigable, got {f}"
        );
    }

    session.shutdown();
    Ok(())
}

/// `focusPanicCulprit: false` opts out: a break-on-panic stop must yield a
/// vanilla stack — every frame keeps a navigable `source`, nothing is greyed
/// (`subtle`), and the top frame is the raw panic-runtime frame (so the
/// editor lands wherever DWARF points, like a plain debugger).
#[test]
#[serial]
fn test_break_on_panic_focus_opt_out() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;

    let launch_seq = session.client.send_request(
        "launch",
        json!({
            "program": example_bin("panic_kinds"),
            "args": ["str"],
            "focusPanicCulprit": false,
        }),
    )?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(session, &config_response, "configurationDone", config_seq, true);

    let Some(stopped) =
        wait_for_event_or_terminated(&mut session, "stopped", OPTIONAL_EVENT_TIMEOUT)?
    else {
        session.shutdown();
        return Ok(());
    };
    let thread_id = stopped["body"]["threadId"].as_i64().unwrap_or_default();

    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let stack = session.client.read_response(stack_seq)?;
    ensure_response!(session, &stack, "stackTrace", stack_seq, true);
    let frames = stack["body"]["stackFrames"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // No frame may be deemphasized when the feature is off.
    assert!(
        frames
            .iter()
            .all(|f| f["presentationHint"].as_str() != Some("subtle")),
        "with focusPanicCulprit off no frame should be subtle: {frames:?}"
    );
    // The top frame keeps its (navigable) source rather than being suppressed.
    if let Some(top) = frames.first() {
        assert!(
            top["source"].is_null() || top["source"]["path"].is_string(),
            "top frame source must be raw (navigable or genuinely absent), got {top}"
        );
    }

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_step_out_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("stepOut", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepOut", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_step_back_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("stepBack", json!({ "threadId": thread_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepBack", seq, false);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn test_live_step_back_restores_previous_stop() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );

    assert_eq!(top_frame_line(&mut session, thread_id)?, Some(HELLO_LINE));

    let next_seq = session
        .client
        .send_request("next", json!({ "threadId": thread_id }))?;
    let next_response = session.client.read_response(next_seq)?;
    ensure_response!(session, &next_response, "next", next_seq, true);
    let _ = session.client.wait_for_event("stopped")?;

    let stepped_line = top_frame_line(&mut session, thread_id)?;
    assert_ne!(stepped_line, Some(HELLO_LINE));

    let step_back_seq = session
        .client
        .send_request("stepBack", json!({ "threadId": thread_id }))?;
    let step_back_response = session.client.read_response(step_back_seq)?;
    ensure_response!(
        session,
        &step_back_response,
        "stepBack",
        step_back_seq,
        true
    );
    let stopped = session.client.wait_for_event("stopped")?;
    assert_eq!(
        stopped
            .get("body")
            .and_then(|b| b.get("reason"))
            .and_then(Value::as_str),
        Some("step")
    );

    assert_eq!(top_frame_line(&mut session, thread_id)?, Some(HELLO_LINE));

    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_step_back_request_with_loaded_replay_trace() -> anyhow::Result<()> {
    let trace_dir = temp_replay_trace("step-back")?;
    let mut session = DapSession::start()?;
    initialize(&mut session)?;

    let launch_seq = session.client.send_request(
        "launch",
        json!({ "tracePath": trace_dir.to_string_lossy() }),
    )?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    assert_eq!(launch_response["body"]["totalEvents"], 3);
    assert_eq!(launch_response["body"]["eventIndex"], 3);

    let config_seq = session
        .client
        .send_request("configurationDone", json!({}))?;
    let config_response = session.client.read_response(config_seq)?;
    ensure_response!(
        session,
        &config_response,
        "configurationDone",
        config_seq,
        true
    );
    let _ = session.client.wait_for_event("stopped")?;

    let seq = session
        .client
        .send_request("stepBack", json!({ "threadId": 1 }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepBack", seq, true);
    assert_eq!(response["body"]["eventIndex"], 2);

    let stopped = session.client.wait_for_event("stopped")?;
    assert_eq!(
        stopped
            .get("body")
            .and_then(|b| b.get("reason"))
            .and_then(Value::as_str),
        Some("step")
    );
    assert_eq!(
        stopped
            .get("body")
            .and_then(|b| b.get("threadId"))
            .and_then(Value::as_i64),
        Some(1)
    );

    let threads_seq = session.client.send_request("threads", json!({}))?;
    let threads_response = session.client.read_response(threads_seq)?;
    ensure_response!(session, &threads_response, "threads", threads_seq, true);
    assert_eq!(threads_response["body"]["threads"][0]["id"], 1);

    let stack_seq = session
        .client
        .send_request("stackTrace", json!({ "threadId": 1 }))?;
    let stack_response = session.client.read_response(stack_seq)?;
    ensure_response!(session, &stack_response, "stackTrace", stack_seq, true);
    assert_eq!(stack_response["body"]["totalFrames"], 1);
    assert!(
        stack_response["body"]["stackFrames"][0]["name"]
            .as_str()
            .unwrap_or_default()
            .contains("replay event 2")
    );

    session.shutdown();
    fs::remove_dir_all(&trace_dir).ok();
    Ok(())
}

#[test]
#[serial]
fn test_reverse_continue_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("reverseContinue", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "reverseContinue", seq, false);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_pause_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let pause_seq = session.client.send_request("pause", json!({}))?;
    let pause_response = session.client.read_response(pause_seq)?;
    ensure_response!(session, &pause_response, "pause", pause_seq, true);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_goto_targets_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "line": HELLO_LINE,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "gotoTargets", seq, true);
    assert!(response["body"]["targets"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_goto_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let targets_seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "line": HELLO_LINE,
        }),
    )?;
    let targets_response = session.client.read_response(targets_seq)?;
    ensure_response!(session, &targets_response, "gotoTargets", targets_seq, true);
    let target_id = targets_response["body"]["targets"][0]["id"]
        .as_i64()
        .unwrap_or(0);

    let seq = session
        .client
        .send_request("goto", json!({ "targetId": target_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "goto", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_restart_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("restart", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "restart", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_restart_frame_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("restartFrame", json!({ "frameId": frame_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "restartFrame", seq, true);
    let _ = session.client.wait_for_event("stopped")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_read_memory_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        22
    );
    let targets_seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/dap_disassemble/src/main.rs") },
            "line": 22,
        }),
    )?;
    let targets_response = session.client.read_response(targets_seq)?;
    ensure_response!(session, &targets_response, "gotoTargets", targets_seq, true);
    let instruction = targets_response["body"]["targets"][0]["instructionPointerReference"]
        .as_str()
        .unwrap_or("0x0");

    let seq = session.client.send_request(
        "readMemory",
        json!({ "memoryReference": instruction, "count": 8 }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "readMemory", seq, true);
    assert!(response["body"]["data"].as_str().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_write_memory_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        22
    );
    let targets_seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/dap_disassemble/src/main.rs") },
            "line": 22,
        }),
    )?;
    let targets_response = session.client.read_response(targets_seq)?;
    ensure_response!(session, &targets_response, "gotoTargets", targets_seq, true);
    let instruction = targets_response["body"]["targets"][0]["instructionPointerReference"]
        .as_str()
        .unwrap_or("0x0");

    let read_seq = session.client.send_request(
        "readMemory",
        json!({ "memoryReference": instruction, "count": 4 }),
    )?;
    let read_response = session.client.read_response(read_seq)?;
    ensure_response!(session, &read_response, "readMemory", read_seq, true);
    let data = read_response["body"]["data"].as_str().unwrap_or_default();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap_or_default();
    assert!(!bytes.is_empty());

    let seq = session.client.send_request(
        "writeMemory",
        json!({ "memoryReference": instruction, "data": data }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "writeMemory", seq, true);
    assert!(response["body"]["bytesWritten"].as_u64().is_some());
    let _ = session.client.wait_for_event("invalidated")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_disassemble_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        22
    );
    let targets_seq = session.client.send_request(
        "gotoTargets",
        json!({
            "source": { "path": example_source("examples/dap_disassemble/src/main.rs") },
            "line": 22,
        }),
    )?;
    let targets_response = session.client.read_response(targets_seq)?;
    ensure_response!(session, &targets_response, "gotoTargets", targets_seq, true);
    let instruction = targets_response["body"]["targets"][0]["instructionPointerReference"]
        .as_str()
        .unwrap_or("0x0");

    let seq = session.client.send_request(
        "disassemble",
        json!({ "memoryReference": instruction, "instructionCount": 16 }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "disassemble", seq, true);
    let instructions = response["body"]["instructions"]
        .as_array()
        .expect("disassemble: instructions must be an array");
    assert!(!instructions.is_empty(), "disassemble returned no instructions");
    // Source interleaving: at least one instruction carries a `location`+`line`
    // so VS Code can show Rust on the left, asm on the right.
    let interleaved = instructions.iter().any(|ins| {
        ins.get("location")
            .and_then(|loc| loc.get("path"))
            .and_then(serde_json::Value::as_str)
            .is_some()
            && ins.get("line").and_then(serde_json::Value::as_u64).is_some()
    });
    assert!(
        interleaved,
        "disassemble response should interleave source (location+line) on at least one instruction",
    );
    session.shutdown();
    Ok(())
}

/// VS Code's Disassembly View drives `disassemble` with a **negative**
/// `instructionOffset` (to show context *before* the current PC) and a frame's
/// `instructionPointerReference` as the `memoryReference` — the path the simpler
/// test above skips. Mirror that exact request and assert it succeeds and the
/// anchor instruction is in range.
#[test]
#[serial]
fn test_disassemble_view_context_before() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        22
    );
    // The Disassembly View anchors on the focused frame's IP, like VS Code.
    let st = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let st = session.client.read_response(st)?;
    let ip = st["body"]["stackFrames"][0]["instructionPointerReference"]
        .as_str()
        .expect("stackTrace frame must carry instructionPointerReference (the disasm anchor)")
        .to_string();

    // VS Code's real request shape: anchor + context before (negative offset).
    let seq = session.client.send_request(
        "disassemble",
        json!({
            "memoryReference": ip,
            "instructionOffset": -8,
            "instructionCount": 24,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "disassemble", seq, true);
    let instructions = response["body"]["instructions"]
        .as_array()
        .expect("disassemble: instructions must be an array");
    assert_eq!(
        instructions.len(),
        24,
        "disassemble must return exactly instructionCount entries (padded), even with context-before",
    );
    let anchor = u64::from_str_radix(ip.trim_start_matches("0x"), 16).unwrap();
    let has_anchor = instructions.iter().any(|ins| {
        ins["address"]
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            == Some(anchor)
    });
    assert!(has_anchor, "the anchor (current PC) must appear in the disassembled window");
    session.shutdown();
    Ok(())
}

/// Software breakpoints replace the original instruction with a trap opcode
/// (`brk #0` on aarch64, `int3` on x86_64). `disassemble` must restore the
/// original bytes so the caller never sees the trap.
#[test]
#[serial]
fn test_disassemble_no_brk_at_breakpoint() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        22
    );
    // Get the breakpoint PC from the stack trace.
    let st = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let st = session.client.read_response(st)?;
    let ip = st["body"]["stackFrames"][0]["instructionPointerReference"]
        .as_str()
        .expect("frame must carry instructionPointerReference")
        .to_string();

    // Disassemble the breakpoint address itself.
    let seq = session.client.send_request(
        "disassemble",
        json!({ "memoryReference": ip, "instructionOffset": 0, "instructionCount": 4 }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "disassemble", seq, true);

    let instructions = response["body"]["instructions"]
        .as_array()
        .expect("instructions array");

    // The instruction at the breakpoint address must not be the trap opcode.
    let trap_mnemonics: &[&str] = &["brk", "int3", "int"];
    let bp_ins = instructions
        .iter()
        .find(|ins| ins["address"].as_str() == Some(ip.as_str()));
    let ins_text = bp_ins
        .and_then(|i| i["instruction"].as_str())
        .unwrap_or("");
    let first_word = ins_text.split_whitespace().next().unwrap_or("");
    assert!(
        !trap_mnemonics.contains(&first_word),
        "disassemble at breakpoint address returned trap opcode `{ins_text}`, \
         expected original instruction (breakpoint byte-patching failed)",
    );
    session.shutdown();
    Ok(())
}

/// Disassembly source-line annotations must use `is_stmt=true` DWARF rows only.
/// Non-`is_stmt` rows are boundary markers that often carry the NEXT line's number
/// as a transition hint; if we return one, annotations shift ±1 for instructions
/// that straddle a statement boundary.
///
/// Invariant: every `line` annotation in the full-function disassembly of
/// `busy_work` (lines 5–11 in dap_disassemble's main.rs) must fall in [5, 11].
#[test]
#[serial]
fn test_disassemble_line_annotations_are_stmt_only() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    // Break inside busy_work's loop body (line 8).
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_disassemble"),
        &example_source("examples/dap_disassemble/src/main.rs"),
        8
    );
    let st = session
        .client
        .send_request("stackTrace", json!({ "threadId": thread_id }))?;
    let st = session.client.read_response(st)?;
    let ip = st["body"]["stackFrames"][0]["instructionPointerReference"]
        .as_str()
        .expect("frame must carry instructionPointerReference")
        .to_string();

    // Use bs/functionBounds to get the exact address range of busy_work.
    let bounds = session
        .client
        .send_request("bs/functionBounds", json!({}))?;
    let bounds = session.client.read_response(bounds)?;
    let (mem_ref, instr_count) = if bounds["body"]["unavailable"].as_bool() == Some(true) {
        // Fallback: modest window (no +8 overshoot into adjacent functions).
        (ip.clone(), 64i64)
    } else {
        let start = bounds["body"]["startAddress"]
            .as_str()
            .unwrap_or(ip.as_str())
            .to_string();
        let end_str = bounds["body"]["endAddress"].as_str().unwrap_or("0");
        let start_int = u64::from_str_radix(start.trim_start_matches("0x"), 16).unwrap_or(0);
        let end_int = u64::from_str_radix(end_str.trim_start_matches("0x"), 16).unwrap_or(0);
        // Exact instruction count — no +8 overshoot into the next function.
        let count = ((end_int.saturating_sub(start_int)) / 4) as i64;
        (start, count.max(1))
    };

    let disasm_seq = session.client.send_request(
        "disassemble",
        json!({ "memoryReference": mem_ref, "instructionOffset": 0, "instructionCount": instr_count }),
    )?;
    let disasm_resp = session.client.read_response(disasm_seq)?;
    ensure_response!(session, &disasm_resp, "disassemble", disasm_seq, true);
    let instructions = disasm_resp["body"]["instructions"]
        .as_array()
        .expect("instructions array");

    // Collect every `line` annotation emitted for the dap_disassemble source.
    let src_path = example_source("examples/dap_disassemble/src/main.rs");
    let src_str = src_path.to_string_lossy();
    let annotated_lines: Vec<u64> = instructions
        .iter()
        .filter(|ins| {
            ins["location"]["path"]
                .as_str()
                .map_or(false, |p| p == src_str.as_ref())
        })
        .filter_map(|ins| ins["line"].as_u64())
        .collect();

    assert!(
        !annotated_lines.is_empty(),
        "disassemble must emit at least one source-line annotation"
    );
    // busy_work body spans lines 5-11 (fn signature through closing brace).
    // Non-is_stmt boundary rows could bleed in lines 4 (the #[inline(never)]
    // attribute) or 13+ (next function).
    for &ln in &annotated_lines {
        assert!(
            (5..=11).contains(&ln),
            "disassemble emitted line {ln} outside busy_work's span [5,11]; \
             likely a non-is_stmt boundary row leaked into annotation"
        );
    }
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_data_breakpoint_info_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let seq = session
        .client
        .send_request("dataBreakpointInfo", json!({ "name": "stats.count" }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "dataBreakpointInfo", seq, true);
    assert!(response["body"]["description"].as_str().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_set_data_breakpoints_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("dap_data_breakpoints");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let info_seq = session
        .client
        .send_request("dataBreakpointInfo", json!({ "name": "stats.count" }))?;
    let info_response = session.client.read_response(info_seq)?;
    ensure_response!(
        session,
        &info_response,
        "dataBreakpointInfo",
        info_seq,
        true
    );
    let data_id = info_response["body"]["dataId"].clone();

    let seq = session.client.send_request(
        "setDataBreakpoints",
        json!({ "breakpoints": [{ "dataId": data_id, "accessType": "write" }] }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setDataBreakpoints", seq, true);
    assert!(response["body"]["breakpoints"].is_array());
    let _ = session.client.wait_for_event("breakpoint")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_modules_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let seq = session.client.send_request("modules", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "modules", seq, true);
    assert!(response["body"]["modules"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_loaded_sources_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let seq = session.client.send_request("loadedSources", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "loadedSources", seq, true);
    assert!(response["body"]["sources"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_source_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);

    let seq = session.client.send_request(
        "source",
        json!({ "source": { "path": example_source("examples/hello_world/src/hello_world.rs") } }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "source", seq, true);
    assert!(response["body"]["content"].as_str().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_completions_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_set_variable"),
        &example_source("examples/dap_set_variable/src/main.rs"),
        SET_VAR_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session.client.send_request(
        "completions",
        json!({
            "text": "con",
            "column": 3,
            "frameId": frame_id,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "completions", seq, true);
    assert!(response["body"]["targets"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_step_in_targets_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session
        .client
        .send_request("stepInTargets", json!({ "frameId": frame_id }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "stepInTargets", seq, true);
    assert!(response["body"]["targets"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_breakpoint_locations_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let seq = session.client.send_request(
        "breakpointLocations",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "line": HELLO_LINE,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "breakpointLocations", seq, true);
    assert!(response["body"]["breakpoints"].is_array());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_terminate_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("terminate", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "terminate", seq, true);
    let _ = session.client.wait_for_event("terminated")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_terminate_threads_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session
        .client
        .send_request("terminateThreads", json!({ "threadIds": [] }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "terminateThreads", seq, true);
    let _ = session.client.wait_for_event("terminated")?;
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_disconnect_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let response = session.disconnect(true)?;
    assert!(response["success"].as_bool().unwrap_or(false));
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_cancel_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let seq = session.client.send_request("cancel", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "cancel", seq, true);
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_run_in_terminal_request() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let seq = session
        .client
        .send_request("runInTerminal", json!({ "args": ["/bin/echo", "dap"] }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "runInTerminal", seq, true);
    assert!(response["body"]["processId"].as_u64().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_initialized() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let seq = session
        .client
        .send_request("initialize", json!({ "adapterID": "bugstalker" }))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "initialize", seq, true);
    let event = session.client.wait_for_event("initialized")?;
    assert_eq!(event["event"], "initialized");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_stopped() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_continued() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("continue", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "continue", seq, true);
    let Some(event) =
        wait_for_event_or_terminated(&mut session, "continued", OPTIONAL_EVENT_TIMEOUT)?
    else {
        session.shutdown();
        return Ok(());
    };
    assert_eq!(event["event"], "continued");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_thread() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let Some(event) = wait_for_event_or_terminated(&mut session, "thread", OPTIONAL_EVENT_TIMEOUT)?
    else {
        session.shutdown();
        return Ok(());
    };
    assert!(event["body"]["threadId"].as_i64().is_some());
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_breakpoint() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let bp_seq = session.client.send_request(
        "setBreakpoints",
        json!({
            "source": { "path": example_source("examples/hello_world/src/hello_world.rs") },
            "breakpoints": [{ "line": HELLO_LINE }],
        }),
    )?;
    let bp_response = session.client.read_response(bp_seq)?;
    ensure_response!(session, &bp_response, "setBreakpoints", bp_seq, true);
    let event = session.client.wait_for_event("breakpoint")?;
    assert_eq!(event["event"], "breakpoint");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_module() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let event = session.client.wait_for_event("module")?;
    assert_eq!(event["event"], "module");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_loaded_source() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let event = session.client.wait_for_event("loadedSource")?;
    assert_eq!(event["event"], "loadedSource");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_process() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let event = session.client.wait_for_event("process")?;
    assert_eq!(event["event"], "process");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_output() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("continue", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "continue", seq, true);
    let Some(event) = wait_for_event_or_terminated(&mut session, "output", OPTIONAL_EVENT_TIMEOUT)?
    else {
        session.shutdown();
        return Ok(());
    };
    assert_eq!(event["event"], "output");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_exited() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("continue", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "continue", seq, true);
    let event = session.client.wait_for_event("exited")?;
    assert_eq!(event["event"], "exited");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_terminated() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let _thread_id = require_launch!(
        &mut session,
        &example_bin("hello_world"),
        &example_source("examples/hello_world/src/hello_world.rs"),
        HELLO_LINE
    );
    let seq = session.client.send_request("terminate", json!({}))?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "terminate", seq, true);
    let event = session.client.wait_for_event("terminated")?;
    assert_eq!(event["event"], "terminated");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_progress() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let start = session.client.wait_for_event("progressStart")?;
    assert_eq!(start["event"], "progressStart");
    let update = session.client.wait_for_event("progressUpdate")?;
    assert_eq!(update["event"], "progressUpdate");
    let end = session.client.wait_for_event("progressEnd")?;
    assert_eq!(end["event"], "progressEnd");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_invalidated() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    let thread_id = require_launch!(
        &mut session,
        &example_bin("dap_set_variable"),
        &example_source("examples/dap_set_variable/src/main.rs"),
        SET_VAR_LINE
    );
    let frame_id = require_frame!(&mut session, thread_id);
    let seq = session.client.send_request(
        "setExpression",
        json!({
            "expression": "container.point.x",
            "value": "40",
            "frameId": frame_id,
        }),
    )?;
    let response = session.client.read_response(seq)?;
    ensure_response!(session, &response, "setExpression", seq, true);
    let event = session.client.wait_for_event("invalidated")?;
    assert_eq!(event["event"], "invalidated");
    session.shutdown();
    Ok(())
}

#[test]
#[serial]
fn test_event_capabilities() -> anyhow::Result<()> {
    let mut session = DapSession::start()?;
    initialize(&mut session)?;
    let program = example_bin("hello_world");
    let launch_seq = session
        .client
        .send_request("launch", json!({ "program": program }))?;
    let launch_response = session.client.read_response(launch_seq)?;
    ensure_response!(session, &launch_response, "launch", launch_seq, true);
    let event = session.client.wait_for_event("capabilities")?;
    assert_eq!(event["event"], "capabilities");
    assert!(event["body"]["capabilities"].is_object());
    session.shutdown();
    Ok(())
}

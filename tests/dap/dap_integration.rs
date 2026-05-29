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
    assert!(response["body"]["instructions"].is_array());
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

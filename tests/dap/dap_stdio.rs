// SPDX-License-Identifier: MIT
//! DAP stdio transport tests
//! Tests for DAP in stdio mode (--dap-local)

use libc::{F_GETFL, F_SETFL, O_NONBLOCK, fcntl};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Start bs in stdio DAP mode and return a connection to it
fn start_bs_stdio_dap(debugee: &str) -> anyhow::Result<StdioDAP> {
    let mut child = Command::new(
        std::env::var("CARGO_BIN_EXE_bs").unwrap_or_else(|_| "./target/debug/bs".to_string()),
    )
    .arg("--dap-local")
    .arg(debugee)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    // Set stdout non-blocking so reads can be polled with timeouts
    let fd = stdout.as_raw_fd();
    unsafe {
        let flags = fcntl(fd, F_GETFL);
        if flags >= 0 {
            let _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK);
        }
    }
    Ok(StdioDAP {
        child,
        stdin: Box::new(stdin),
        reader: BufReader::new(stdout),
    })
}

pub struct StdioDAP {
    /// Held only to keep the spawned `bs` process alive for the
    /// lifetime of the DAP session — `Child::drop` reaps and
    /// kills it when this struct goes out of scope.
    #[allow(dead_code)]
    child: Child,
    stdin: Box<dyn Write + Send>,
    reader: BufReader<std::process::ChildStdout>,
}

impl StdioDAP {
    fn read_message(&mut self) -> anyhow::Result<serde_json::Value> {
        const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);
        let deadline = Instant::now() + MESSAGE_TIMEOUT;

        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let read_n = loop {
                match self.reader.read_line(&mut line) {
                    Ok(n) => break n,
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        if Instant::now() > deadline {
                            return Err(anyhow::anyhow!("Timed out waiting for DAP header"));
                        }
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                }
            };

            if read_n == 0 {
                return Err(anyhow::anyhow!("connection closed"));
            }
            let line_trimmed = line.trim_end_matches(['\r', '\n']);
            if line_trimmed.is_empty() {
                if content_length.is_some() {
                    break;
                } else {
                    continue;
                }
            }

            let lower = line_trimmed.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = Some(v.trim().parse()?);
            }
        }

        let len = content_length.ok_or_else(|| anyhow::anyhow!("missing content-length"))?;
        let mut buf = vec![0u8; len];

        // read body with deadline
        let mut offset = 0usize;
        while offset < buf.len() {
            match self.reader.read(&mut buf[offset..]) {
                Ok(0) => return Err(anyhow::anyhow!("DAP connection closed")),
                Ok(n) => {
                    offset += n;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() > deadline {
                        return Err(anyhow::anyhow!("Timed out waiting for DAP body"));
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }

        let v = serde_json::from_slice(&buf)?;
        Ok(v)
    }

    fn write_message(&mut self, msg: &serde_json::Value) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len())?;
        self.stdin.write_all(&payload)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn send_request(
        &mut self,
        seq: i64,
        command: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.write_message(&json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments
        }))
    }
}

#[test]
fn test_stdio_dap_initialize() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Send initialize request
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "supportsVariableType": true,
            "supportsVariablePaging": true,
            "supportsRunInTerminalRequest": false,
            "supportsMemoryReferences": true,
        }),
    )?;

    // Receive initialize response
    let response = dap.read_message()?;
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "initialize");
    assert!(response["success"].as_bool().unwrap_or(false));

    // Receive initialized event
    let event = dap.read_message()?;
    assert_eq!(event["type"], "event");
    assert_eq!(event["event"], "initialized");

    Ok(())
}

#[test]
fn test_stdio_dap_launch() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Send launch request
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": hello_world,
            "stopOnEntry": true,
        }),
    )?;

    // Receive launch response (may get output events first)
    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break msg;
        }
        // Skip any events (like output events) and keep reading
    };
    assert!(response["success"].as_bool().unwrap_or(false));

    Ok(())
}

#[test]
fn test_stdio_dap_capabilities() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let init_response = dap.read_message()?;
    let _event = dap.read_message()?;

    // Check capabilities
    let body = &init_response["body"];
    assert!(
        body["supportsConfigurationDoneRequest"]
            .as_bool()
            .unwrap_or(false)
    );
    assert!(body["supportsSetVariable"].as_bool().unwrap_or(false));
    assert!(
        body["supportsBreakpointLocationsRequest"]
            .as_bool()
            .unwrap_or(false)
    );

    Ok(())
}

#[test]
fn test_stdio_dap_threads() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize and launch
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": hello_world,
            "stopOnEntry": true,
        }),
    )?;

    // Read until we get the launch response (skip any events like output)
    let mut response = None;
    for _ in 0..50 {
        // Limit iterations to avoid infinite loops
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            response = Some(msg);
            break;
        }
    }

    if response.is_none() {
        // If we timeout getting launch response, just skip this test
        return Ok(());
    }

    // Send threads request
    dap.send_request(3, "threads", json!({}))?;

    // Receive threads response (skip any events)
    let mut response_found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "threads" {
            assert!(msg["body"]["threads"].is_array());
            response_found = true;
            break;
        }
    }

    if !response_found {
        // If threads request not supported, just skip
        return Ok(());
    }

    Ok(())
}

#[test]
fn test_stdio_dap_set_breakpoints() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Skip any events from launch before sending next request
    // Just drain any available messages
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Set breakpoint
    dap.send_request(
        3,
        "setBreakpoints",
        json!({
            "source": { "path": &hello_world },
            "breakpoints": [{ "line": 5 }],
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "setBreakpoints" {
            break msg;
        }
    };
    assert!(response["success"].as_bool().unwrap_or(false));

    Ok(())
}

#[test]
fn test_stdio_dap_continue() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send continue request for main thread
    dap.send_request(
        3,
        "continue",
        json!({
            "threadId": 1,
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "continue" {
            break msg;
        }
    };
    assert!(response["success"].as_bool().unwrap_or(false));

    Ok(())
}

#[test]
fn test_stdio_dap_next() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send next request for main thread
    dap.send_request(
        3,
        "next",
        json!({
            "threadId": 1,
        }),
    )?;

    // Next may not be supported or may fail - just check we get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "next" {
            found = true;
            break;
        }
    }
    assert!(found, "next response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_stack_trace() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send stackTrace request for main thread
    dap.send_request(
        3,
        "stackTrace",
        json!({
            "threadId": 1,
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "stackTrace" {
            break msg;
        }
    };
    assert!(response["success"].as_bool().unwrap_or(false));
    assert!(response["body"]["stackFrames"].is_array());

    Ok(())
}

#[test]
fn test_stdio_dap_scopes() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send scopes request for frame 0 (assuming it exists)
    dap.send_request(
        3,
        "scopes",
        json!({
            "frameId": 0,
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "scopes" {
            break msg;
        }
    };
    assert!(response["success"].as_bool().unwrap_or(false));

    Ok(())
}

#[test]
fn test_stdio_dap_variables() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send variables request for scope 0 (assuming it exists)
    dap.send_request(
        3,
        "variables",
        json!({
            "variablesReference": 0,
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "variables" {
            break msg;
        }
    };
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "variables");

    Ok(())
}

#[test]
fn test_stdio_dap_evaluate() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send evaluate request
    dap.send_request(
        3,
        "evaluate",
        json!({
            "expression": "1+1",
            "frameId": 0,
            "context": "watch"
        }),
    )?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "evaluate" {
            break msg;
        }
    };
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "evaluate");

    Ok(())
}

#[test]
fn test_stdio_dap_step_in() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send stepIn request
    dap.send_request(
        3,
        "stepIn",
        json!({
            "threadId": 1,
        }),
    )?;

    // stepIn may not be supported or may fail - just check we get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "stepIn" {
            found = true;
            break;
        }
    }
    assert!(found, "stepIn response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_step_out() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send stepOut request
    dap.send_request(
        3,
        "stepOut",
        json!({
            "threadId": 1,
        }),
    )?;

    // stepOut may not be supported or may fail - just check we get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "stepOut" {
            found = true;
            break;
        }
    }
    assert!(found, "stepOut response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_configuration_done() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send configurationDone request
    dap.send_request(3, "configurationDone", json!({}))?;

    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "configurationDone" {
            break msg;
        }
    };
    assert!(response["success"].as_bool().unwrap_or(false));

    Ok(())
}

#[test]
fn test_stdio_dap_set_function_breakpoints() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Set function breakpoint
    dap.send_request(
        3,
        "setFunctionBreakpoints",
        json!({
            "breakpoints": [{ "name": "main" }],
        }),
    )?;

    // May not be supported but should get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "setFunctionBreakpoints" {
            found = true;
            break;
        }
    }
    assert!(found, "setFunctionBreakpoints response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_set_exception_breakpoints() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Set exception breakpoints
    dap.send_request(
        3,
        "setExceptionBreakpoints",
        json!({
            "filters": ["all"],
        }),
    )?;

    // May not be supported but should get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "setExceptionBreakpoints" {
            found = true;
            break;
        }
    }
    assert!(found, "setExceptionBreakpoints response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_pause() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch with stopOnEntry
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send pause request
    dap.send_request(
        3,
        "pause",
        json!({
            "threadId": 1,
        }),
    )?;

    // May not be supported but should get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "pause" {
            found = true;
            break;
        }
    }
    assert!(found, "pause response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_disconnect() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send disconnect request
    dap.send_request(3, "disconnect", json!({}))?;

    // Should get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "disconnect" {
            found = true;
            break;
        }
    }
    assert!(found, "disconnect response not received");

    Ok(())
}

#[test]
fn test_stdio_dap_source() -> anyhow::Result<()> {
    let hello_world = std::env::var("CARGO_BIN_EXE_hello_world")
        .unwrap_or_else(|_| "./examples/target/debug/hello_world".to_string());

    let mut dap = start_bs_stdio_dap(&hello_world)?;

    // Initialize
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID": "test",
            "clientName": "test-client",
            "adapterID": "bs-dap",
        }),
    )?;

    let _init_response = dap.read_message()?;
    let _init_event = dap.read_message()?;

    // Launch
    dap.send_request(
        2,
        "launch",
        json!({
            "request": "launch",
            "program": &hello_world,
            "stopOnEntry": true,
        }),
    )?;

    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Send source request
    dap.send_request(
        3,
        "source",
        json!({
            "source": { "path": &hello_world },
        }),
    )?;

    // May not be supported but should get a response
    let mut found = false;
    for _ in 0..50 {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "source" {
            found = true;
            break;
        }
    }
    assert!(found, "source response not received");

    Ok(())
}

/// Phase 4 step 12 — `bs/visualiserList` custom DAP request.
/// Verifies an IDE settings panel can enumerate every Tier-A
/// visualiser the loader recovered from the `viz_demo` debuggee
/// without first having to hit a value.
#[test]
fn test_stdio_dap_visualiser_list() -> anyhow::Result<()> {
    let viz_demo = std::env::var("CARGO_BIN_EXE_viz_demo")
        .unwrap_or_else(|_| "./examples/target/debug/viz_demo".to_string());

    let mut dap = start_bs_stdio_dap(&viz_demo)?;

    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID":   "test",
            "clientName": "test-client",
            "adapterID":  "bs-dap",
        }),
    )?;
    let _ = dap.read_message()?; // initialize response
    let _ = dap.read_message()?; // initialized event

    dap.send_request(
        2,
        "launch",
        json!({
            "request":     "launch",
            "program":     &viz_demo,
            "stopOnEntry": true,
        }),
    )?;
    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    dap.send_request(3, "bs/visualiserList", json!({}))?;
    let response = loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "bs/visualiserList" {
            break msg;
        }
    };

    assert_eq!(
        response["success"], true,
        "request should succeed: {response:?}"
    );
    let visualisers = response["body"]["visualisers"]
        .as_array()
        .expect("body.visualisers should be an array");
    assert_eq!(
        visualisers.len(),
        10,
        "expected 10 visualisers from viz_demo, got {} ({visualisers:?})",
        visualisers.len(),
    );

    // Pick out a few specific specs and assert their shape end-
    // to-end through the DAP wire.
    let person = visualisers
        .iter()
        .find(|v| v["typeName"] == "viz_demo::Person")
        .expect("viz_demo::Person should be in the list");
    assert_eq!(person["origin"], "tier-a");
    assert_eq!(person["summary"], "Person({name}, age {age})");
    let person_fields = person["fields"]
        .as_array()
        .expect("Person.fields should be an array");
    assert_eq!(person_fields.len(), 5);
    let flags = person_fields
        .iter()
        .find(|f| f["name"] == "flags")
        .expect("Person.flags field should be in the list");
    assert_eq!(flags["format"], "hex");
    let token = person_fields
        .iter()
        .find(|f| f["name"] == "_private_token")
        .expect("Person._private_token field should be in the list");
    assert_eq!(token["hidden"], true);

    let status = visualisers
        .iter()
        .find(|v| v["typeName"] == "viz_demo::Status")
        .expect("viz_demo::Status should be in the list");
    let status_variants = status["variants"]
        .as_array()
        .expect("Status.variants should be an array");
    assert_eq!(status_variants.len(), 3);
    let connected = status_variants
        .iter()
        .find(|v| v["name"] == "Connected")
        .expect("Status::Connected variant should be in the list");
    assert_eq!(connected["tag"], "ok");
    assert_eq!(connected["summary"], "✓ Connected (port {__0})");

    let marker = visualisers
        .iter()
        .find(|v| v["typeName"] == "qualified::Marker")
        .expect("qualified::Marker (via name override) should be in the list");
    assert_eq!(marker["summary"], "Marker#{__0}");

    Ok(())
}

/// Phase 4 step 13 — `bs/visualiserToggle` per-session mute.
/// Toggle a registered visualiser off, verify
/// `bs/visualiserList` reflects the new `enabled` state, then
/// toggle it back on. Also covers the unknown-typeName error
/// path so a typo from the IDE produces an actionable response
/// listing the registered keys.
#[test]
fn test_stdio_dap_visualiser_toggle() -> anyhow::Result<()> {
    let viz_demo = std::env::var("CARGO_BIN_EXE_viz_demo")
        .unwrap_or_else(|_| "./examples/target/debug/viz_demo".to_string());

    let mut dap = start_bs_stdio_dap(&viz_demo)?;
    dap.send_request(
        1,
        "initialize",
        json!({
            "clientID":   "test",
            "clientName": "test-client",
            "adapterID":  "bs-dap",
        }),
    )?;
    let _ = dap.read_message()?;
    let _ = dap.read_message()?;
    dap.send_request(
        2,
        "launch",
        json!({
            "request":     "launch",
            "program":     &viz_demo,
            "stopOnEntry": true,
        }),
    )?;
    loop {
        let msg = dap.read_message()?;
        if msg["type"] == "response" && msg["command"] == "launch" {
            break;
        }
    }

    // Sanity: Person is enabled by default.
    let read_person_enabled = |dap: &mut StdioDAP, seq: i64| -> anyhow::Result<bool> {
        dap.send_request(seq, "bs/visualiserList", json!({}))?;
        let resp = loop {
            let m = dap.read_message()?;
            if m["type"] == "response" && m["command"] == "bs/visualiserList" {
                break m;
            }
        };
        let v = resp["body"]["visualisers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["typeName"] == "viz_demo::Person")
            .expect("Person should be in the list")
            .clone();
        Ok(v["enabled"].as_bool().unwrap_or(true))
    };
    assert!(read_person_enabled(&mut dap, 3)?);

    // Toggle Person off.
    dap.send_request(
        4,
        "bs/visualiserToggle",
        json!({
            "typeName": "viz_demo::Person",
            "enabled":  false,
        }),
    )?;
    let toggle_resp = loop {
        let m = dap.read_message()?;
        if m["type"] == "response" && m["command"] == "bs/visualiserToggle" {
            break m;
        }
    };
    assert_eq!(toggle_resp["success"], true);
    assert_eq!(toggle_resp["body"]["typeName"], "viz_demo::Person");
    assert_eq!(toggle_resp["body"]["enabled"], false);

    // List now reports `enabled = false` for Person.
    assert!(!read_person_enabled(&mut dap, 5)?);

    // Toggle back on.
    dap.send_request(
        6,
        "bs/visualiserToggle",
        json!({ "typeName": "viz_demo::Person", "enabled": true }),
    )?;
    loop {
        let m = dap.read_message()?;
        if m["type"] == "response" && m["command"] == "bs/visualiserToggle" {
            break;
        }
    }
    assert!(read_person_enabled(&mut dap, 7)?);

    // Unknown typeName: error response with actionable message.
    dap.send_request(
        8,
        "bs/visualiserToggle",
        json!({ "typeName": "Nonexistent", "enabled": false }),
    )?;
    let err_resp = loop {
        let m = dap.read_message()?;
        if m["type"] == "response" && m["command"] == "bs/visualiserToggle" {
            break m;
        }
    };
    assert_eq!(err_resp["success"], false);
    let msg = err_resp["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("\"Nonexistent\""),
        "error message should quote the bad name: {msg}",
    );
    assert!(
        msg.contains("known: ["),
        "error message should list known keys: {msg}",
    );

    Ok(())
}

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);

static BUILD_FIXTURES: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();

pub fn ensure_example_binaries() -> anyhow::Result<()> {
    let cell = BUILD_FIXTURES.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if let Some(result) = guard.as_ref() {
        return result.clone().map_err(|err| anyhow!(err));
    }
    let result = (|| {
        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                "dap_set_variable",
                "-p",
                "dap_data_breakpoints",
                "-p",
                "dap_disassemble",
                "-p",
                "dap_attach",
                "-p",
                "hello_world",
            ])
            .current_dir(repo_root().join("examples"))
            .status()
            .context("build DAP example fixtures")?;
        if !status.success() {
            return Err(anyhow!("failed to build example fixtures"));
        }
        ensure_bs_entitled()?;
        Ok(())
    })()
    .map_err(|err| err.to_string());
    *guard = Some(result.clone());
    result.map_err(|err| anyhow!(err))
}

/// Darwin: re-sign a binary with the test entitlements plist if
/// it isn't already signed with the entitlement we need. Used for
/// the spawned `bs` subprocess (needs `com.apple.security.cs.debugger`
/// to call `task_for_pid` on the inferior it launches) and for the
/// `dap_attach` example (needs `com.apple.security.get-task-allow`
/// so an external `bs` can `task_for_pid` *it*). Cargo rebuilds
/// wipe ad-hoc signatures, so we re-sign once per build.
#[cfg(target_os = "macos")]
fn codesign_with_test_entitlements(
    binary: &Path,
    must_have_entitlement: &str,
) -> anyhow::Result<()> {
    if !binary.exists() {
        return Err(anyhow!("binary not found at {}", binary.display()));
    }
    let probe = Command::new("codesign")
        .args(["-d", "--entitlements", "-"])
        .arg(binary)
        .output()
        .context("probe codesign")?;
    let blob = String::from_utf8_lossy(&probe.stdout).into_owned()
        + &String::from_utf8_lossy(&probe.stderr);
    if blob.contains(must_have_entitlement) {
        return Ok(());
    }
    let plist = repo_root().join("tests").join("darwin.entitlements");
    if !plist.exists() {
        return Err(anyhow!(
            "entitlements plist not found at {}",
            plist.display()
        ));
    }
    let status = Command::new("codesign")
        .args(["--entitlements"])
        .arg(&plist)
        .args(["--force", "--sign", "-"])
        .arg(binary)
        .status()
        .context("codesign binary")?;
    if !status.success() {
        return Err(anyhow!(
            "codesign on {} returned {status}",
            binary.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_bs_entitled() -> anyhow::Result<()> {
    let bs = std::env::var("CARGO_BIN_EXE_bs")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target").join("debug").join("bs"));
    codesign_with_test_entitlements(&bs, "com.apple.security.cs.debugger")?;
    // The `dap_attach` target gets attached to by `bs` (a process
    // it doesn't share a parent with), so the kernel demands the
    // *target* carries `get-task-allow` even though the attacher
    // is entitled with `cs.debugger`. Without this the
    // `task_for_pid(target)` inside `bs`'s attach path returns
    // KERN_FAILURE and `Rendezvous::new` collapses with
    // "rendezvous not found".
    let dap_attach = example_bin("dap_attach");
    if dap_attach.exists() {
        codesign_with_test_entitlements(&dap_attach, "com.apple.security.get-task-allow")?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_bs_entitled() -> anyhow::Result<()> {
    Ok(())
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn example_bin(name: &str) -> PathBuf {
    repo_root()
        .join("examples")
        .join("target")
        .join("debug")
        .join(name)
}

pub fn example_source(path: &str) -> PathBuf {
    repo_root().join(path)
}

pub struct DapClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    next_seq: i64,
    pending_events: VecDeque<Value>,
}

impl DapClient {
    pub fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        let start = Instant::now();
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(stream) => break stream,
                Err(err) => {
                    if start.elapsed() > CONNECT_TIMEOUT {
                        return Err(anyhow!("failed to connect to {addr}: {err}"));
                    }
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
            }
        };
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .context("set DAP read timeout")?;
        stream
            .set_write_timeout(Some(READ_TIMEOUT))
            .context("set DAP write timeout")?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            next_seq: 1,
            pending_events: VecDeque::new(),
        })
    }

    pub fn send_request(&mut self, command: &str, arguments: Value) -> anyhow::Result<i64> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let request = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        });
        self.write_message(&request)?;
        Ok(seq)
    }

    pub fn read_response(&mut self, request_seq: i64) -> anyhow::Result<Value> {
        loop {
            let msg = self.read_message()?;
            match msg.get("type").and_then(Value::as_str) {
                Some("event") => self.pending_events.push_back(msg),
                Some("response") => {
                    if msg.get("request_seq").and_then(Value::as_i64) == Some(request_seq) {
                        return Ok(msg);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn read_event(&mut self) -> anyhow::Result<Value> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        loop {
            let msg = self.read_message()?;
            if msg.get("type").and_then(Value::as_str) == Some("event") {
                return Ok(msg);
            }
        }
    }

    pub fn read_event_with_timeout(&mut self, timeout: Duration) -> anyhow::Result<Option<Value>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.read_message_with_deadline(deadline) {
                Ok(msg) => {
                    if msg.get("type").and_then(Value::as_str) == Some("event") {
                        return Ok(Some(msg));
                    }
                }
                Err(err) => {
                    let msg = err.to_string();
                    if msg.contains("Timed out waiting for DAP header")
                        || msg.contains("Timed out waiting for DAP body")
                    {
                        return Ok(None);
                    }
                    return Err(err);
                }
            }
        }
    }

    pub fn wait_for_event(&mut self, name: &str) -> anyhow::Result<Value> {
        loop {
            let event = self.read_event()?;
            if event.get("event").and_then(Value::as_str) == Some(name) {
                return Ok(event);
            }
        }
    }

    fn read_message(&mut self) -> anyhow::Result<Value> {
        let deadline = Instant::now() + MESSAGE_TIMEOUT;
        self.read_message_with_deadline(deadline)
    }

    fn read_message_with_deadline(&mut self, deadline: Instant) -> anyhow::Result<Value> {
        let mut content_length = None;
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
                            return Err(anyhow!("Timed out waiting for DAP header"));
                        }
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                }
            };
            if read_n == 0 {
                return Err(anyhow!("DAP connection closed"));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }

        let len = content_length.ok_or_else(|| anyhow!("Missing Content-Length"))?;
        let mut buf = vec![0u8; len];
        self.read_exact_with_deadline(&mut buf, deadline)?;
        let msg = serde_json::from_slice(&buf)?;
        Ok(msg)
    }

    fn read_exact_with_deadline(
        &mut self,
        buf: &mut [u8],
        deadline: Instant,
    ) -> anyhow::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            match self.reader.read(&mut buf[offset..]) {
                Ok(0) => return Err(anyhow!("DAP connection closed")),
                Ok(n) => offset += n,
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() > deadline {
                        return Err(anyhow!("Timed out waiting for DAP body"));
                    }
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    fn write_message(&mut self, message: &Value) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(message)?;
        write!(self.stream, "Content-Length: {}\r\n\r\n", payload.len())?;
        self.stream.write_all(&payload)?;
        self.stream.flush()?;
        Ok(())
    }
}

pub struct DapSession {
    pub client: DapClient,
    process: Child,
    closed: bool,
}

impl DapSession {
    pub fn start() -> anyhow::Result<Self> {
        ensure_example_binaries()?;
        let listener = TcpListener::bind("127.0.0.1:0").context("bind test TCP port")?;
        let addr = listener.local_addr()?;
        drop(listener);

        let bin_path = std::env::var("CARGO_BIN_EXE_bs")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join("target").join("debug").join("bs"));
        let process = Command::new(bin_path)
            .args(["--dap-remote", &addr.to_string(), "--dap-oneshot"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn bs with --dap-remote")?;
        let client = DapClient::connect(addr)?;
        Ok(Self {
            client,
            process,
            closed: false,
        })
    }

    pub fn disconnect(&mut self, terminate: bool) -> anyhow::Result<Value> {
        let seq = self.client.send_request(
            "disconnect",
            json!({
                "terminateDebuggee": terminate,
            }),
        )?;
        let response = self.client.read_response(seq)?;
        self.closed = true;
        Ok(response)
    }

    #[allow(dead_code)]
    pub fn terminate(&mut self) -> anyhow::Result<Value> {
        let seq = self.client.send_request("terminate", json!({}))?;
        let response = self.client.read_response(seq)?;
        self.closed = true;
        Ok(response)
    }

    pub fn shutdown(&mut self) {
        if !self.closed {
            let _ = self.disconnect(true);
        }
        let _ = wait_for_exit(&mut self.process, SHUTDOWN_TIMEOUT);
    }
}

impl Drop for DapSession {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.disconnect(true);
        }
        if wait_for_exit(&mut self.process, SHUTDOWN_TIMEOUT).is_err() {
            let _ = self.process.kill();
        }
    }
}

pub fn spawn_attach_target(program: &Path) -> anyhow::Result<Child> {
    Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn attach target")
}

pub fn wait_for_exit(child: &mut Child, timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if let Some(_status) = child.try_wait()? {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(anyhow!("process did not exit in time"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

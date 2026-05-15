// SPDX-License-Identifier: MIT
//! JSON-RPC 2.0 framing over a line-oriented byte stream, with JSON5
//! input.
//!
//! The reader supports two framings transparently:
//!   1. One JSON5 value per line — the common case for scripted input.
//!   2. A single JSON5 value spanning multiple lines (e.g. an
//!      indentation-friendly request from a human).
//!
//! Detection rule: try to parse the buffered input as a single value
//! after each newline; if it parses, dispatch and clear the buffer.
//! If it fails with EOF-style brace mismatch, keep reading. If it fails
//! with anything else, return ParseError immediately.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::event::Event;

/// Owns a writer (typically `Stdout`) and serialises every line under a
/// mutex so request responses and event notifications interleave
/// safely.
pub struct OutputSink {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Clone for OutputSink {
    fn clone(&self) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
        }
    }
}

impl OutputSink {
    pub fn new<W: Write + Send + 'static>(writer: W) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
        }
    }

    /// Serialise a single value to one line of output. Errors are
    /// swallowed because there is no upstream to report them to — if
    /// stdout is broken, the agent has gone away.
    pub fn emit<T: Serialize>(&self, value: &T) {
        if let Ok(serialised) = serde_json::to_string(value) {
            let mut w = self.writer.lock().unwrap();
            let _ = writeln!(*w, "{serialised}");
            let _ = w.flush();
        }
    }

    pub fn emit_event(&self, event: Event) {
        let notif = Notification {
            jsonrpc: "2.0",
            method: "event",
            params: event,
        };
        self.emit(&notif);
    }

    pub fn emit_response_ok(&self, id: serde_json::Value, result: serde_json::Value) {
        self.emit(&Response {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        });
    }

    pub fn emit_response_err(&self, id: serde_json::Value, err: BsError) {
        let wire = WireError {
            code: err.code.wire_code(),
            message: err.message,
            data: err.data,
        };
        self.emit(&Response {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(wire),
        });
    }
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Debug, Serialize)]
struct WireError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct Notification<T: Serialize> {
    jsonrpc: &'static str,
    method: &'static str,
    params: T,
}

/// One parsed request from the input stream.
#[derive(Debug)]
pub struct Request {
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: serde_json::Value,
    pub max_response_bytes: Option<usize>,
}

/// Reads JSON5 requests from a BufRead, one at a time. Returns `Ok(None)`
/// at EOF.
pub struct RequestReader<R: Read> {
    reader: BufReader<R>,
    buffer: String,
}

impl<R: Read> RequestReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: String::new(),
        }
    }

    /// Pull the next request. Returns:
    ///   - `Ok(Some(req))` on a parsed request (well-formed JSON-RPC).
    ///   - `Ok(None)` at EOF with nothing pending.
    ///   - `Err((id, BsError))` on any parse / shape error. `id` is
    ///     `Null` if the input was so malformed we couldn't extract one.
    pub fn read(&mut self) -> Result<Option<Request>, (serde_json::Value, BsError)> {
        loop {
            let mut line = String::new();
            let n = match self.reader.read_line(&mut line) {
                Ok(n) => n,
                Err(e) => {
                    return Err((
                        serde_json::Value::Null,
                        BsError::new(ErrorCode::ParseError, format!("stdin read failed: {e}")),
                    ));
                }
            };

            if n == 0 {
                // EOF — anything left in buffer is partial.
                if self.buffer.trim().is_empty() {
                    return Ok(None);
                }
                let leftover = std::mem::take(&mut self.buffer);
                return Err((
                    serde_json::Value::Null,
                    BsError::new(
                        ErrorCode::ParseError,
                        format!("EOF mid-request, buffered: {}", leftover.trim()),
                    ),
                ));
            }

            self.buffer.push_str(&line);

            // Empty / whitespace-only buffer: keep reading.
            if self.buffer.trim().is_empty() {
                self.buffer.clear();
                continue;
            }

            // Try to parse what we have. If it's incomplete JSON, the
            // error is recoverable and we keep buffering.
            match try_parse(&self.buffer) {
                ParseAttempt::Done(value) => {
                    self.buffer.clear();
                    return parse_request(value).map(Some);
                }
                ParseAttempt::Incomplete => {
                    // Wait for more input.
                    continue;
                }
                ParseAttempt::Failed(err) => {
                    let leftover = std::mem::take(&mut self.buffer);
                    return Err((
                        serde_json::Value::Null,
                        BsError::new(
                            ErrorCode::ParseError,
                            format!("malformed JSON5: {err}\ninput: {}", leftover.trim()),
                        ),
                    ));
                }
            }
        }
    }
}

enum ParseAttempt {
    Done(serde_json::Value),
    Incomplete,
    Failed(String),
}

/// Try parsing the buffer as a complete JSON5 value. Distinguishes
/// "incomplete" (could succeed with more input) from "broken" (will never
/// succeed) by checking bracket / brace / quote balance.
fn try_parse(buf: &str) -> ParseAttempt {
    // A buffer that is only whitespace + comments is "not yet anything":
    // wait for more input rather than failing.
    if strip_to_payload(buf).is_empty() {
        return ParseAttempt::Incomplete;
    }
    match json5::from_str::<serde_json::Value>(buf) {
        Ok(v) => ParseAttempt::Done(v),
        Err(e) => {
            // json5's error doesn't tell us "EOF expected here", so fall
            // back to a balance check: if braces / brackets / quotes
            // aren't balanced, more input could help.
            if !is_balanced(buf) {
                ParseAttempt::Incomplete
            } else {
                ParseAttempt::Failed(e.to_string())
            }
        }
    }
}

/// Return the buffer with whitespace and JSON5 comments removed.
/// Used to distinguish "buffer holds only comments / blank lines" from
/// "buffer holds something the parser should attempt".
fn strip_to_payload(buf: &str) -> String {
    let bytes = buf.as_bytes();
    let mut i = 0;
    let mut out = String::with_capacity(buf.len());
    let mut in_str: Option<u8> = None;
    let mut esc = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let c = bytes[i];

        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(q) = in_str {
            out.push(c as char);
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }

        match c {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                i += 2;
            }
            b'"' | b'\'' => {
                in_str = Some(c);
                out.push(c as char);
                i += 1;
            }
            c if (c as char).is_whitespace() => {
                i += 1;
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Lightweight balance check on `{}`, `[]`, and `""` / `''` strings.
/// Comments are skipped (`//` line, `/* */` block).
fn is_balanced(buf: &str) -> bool {
    let bytes = buf.as_bytes();
    let mut i = 0;
    let mut curly = 0i32;
    let mut square = 0i32;
    let mut in_str: Option<u8> = None;
    let mut esc = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let c = bytes[i];

        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(q) = in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }

        match c {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                i += 2;
            }
            b'{' => {
                curly += 1;
                i += 1;
            }
            b'}' => {
                curly -= 1;
                i += 1;
            }
            b'[' => {
                square += 1;
                i += 1;
            }
            b']' => {
                square -= 1;
                i += 1;
            }
            b'"' | b'\'' => {
                in_str = Some(c);
                i += 1;
            }
            _ => i += 1,
        }
    }

    !in_block_comment && in_str.is_none() && curly == 0 && square == 0
}

fn parse_request(value: serde_json::Value) -> Result<Request, (serde_json::Value, BsError)> {
    use serde_json::Value::*;

    let obj = match value {
        Object(o) => o,
        other => {
            return Err((
                Null,
                BsError::new(
                    ErrorCode::InvalidParams,
                    format!(
                        "expected JSON object at top level, got {}",
                        short_kind(&other)
                    ),
                ),
            ));
        }
    };

    let id = obj.get("id").cloned();

    // Validate jsonrpc field if present (warn-don't-fail: agents can
    // omit it and we treat as 2.0).
    if let Some(v) = obj.get("jsonrpc")
        && v != &serde_json::json!("2.0")
    {
        return Err((
            id.unwrap_or(Null),
            BsError::new(
                ErrorCode::InvalidParams,
                format!("only jsonrpc \"2.0\" is supported, got {v}"),
            ),
        ));
    }

    let method = match obj.get("method") {
        Some(String(s)) => s.clone(),
        Some(_) | None => {
            return Err((
                id.unwrap_or(Null),
                BsError::new(ErrorCode::InvalidParams, "missing or non-string `method`"),
            ));
        }
    };

    let params = obj.get("params").cloned().unwrap_or(serde_json::json!({}));

    let max_response_bytes = obj
        .get("max_response_bytes")
        .and_then(|v| v.as_u64().map(|n| n as usize));

    Ok(Request {
        id,
        method,
        params,
        max_response_bytes,
    })
}

fn short_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_simple() {
        assert!(is_balanced("{}"));
        assert!(is_balanced("[1,2,3]"));
        assert!(is_balanced("{\"a\":[1,2]}"));
    }

    #[test]
    fn unbalanced_open() {
        assert!(!is_balanced("{\"a\":1"));
        assert!(!is_balanced("[1, 2"));
    }

    #[test]
    fn comments_stripped() {
        assert!(is_balanced("{ // comment with } in it\n}"));
        assert!(is_balanced("/* { */ {}"));
    }

    #[test]
    fn strings_with_braces() {
        assert!(is_balanced("{\"k\":\"}\"}"));
        assert!(is_balanced("{\"k\":'}'}"));
    }

    #[test]
    fn reads_one_line_request() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"thread.info"}"#;
        let mut r = RequestReader::new(input.as_bytes());
        let req = r.read().unwrap().unwrap();
        assert_eq!(req.method, "thread.info");
        assert_eq!(req.id, Some(serde_json::json!(1)));
    }

    #[test]
    fn reads_request_with_comment() {
        let input = r#"// run this first
{ "jsonrpc": "2.0", id: 1, method: "thread.info" /* trailing */ }"#;
        let mut r = RequestReader::new(input.as_bytes());
        let req = r.read().unwrap().unwrap();
        assert_eq!(req.method, "thread.info");
    }

    #[test]
    fn reads_multiline_request() {
        let input = r#"{
  jsonrpc: "2.0",
  id: 7,
  method: "var",
  params: {
    name: "x",
  },
}"#;
        let mut r = RequestReader::new(input.as_bytes());
        let req = r.read().unwrap().unwrap();
        assert_eq!(req.method, "var");
        assert_eq!(req.id, Some(serde_json::json!(7)));
    }

    #[test]
    fn reads_two_requests_back_to_back() {
        let input = "{id:1,method:\"a\"}\n{id:2,method:\"b\"}\n";
        let mut r = RequestReader::new(input.as_bytes());
        assert_eq!(r.read().unwrap().unwrap().method, "a");
        assert_eq!(r.read().unwrap().unwrap().method, "b");
        assert!(r.read().unwrap().is_none());
    }

    #[test]
    fn rejects_non_object_top_level() {
        let input = "[1,2,3]\n";
        let mut r = RequestReader::new(input.as_bytes());
        let err = r.read().unwrap_err();
        assert_eq!(err.1.code, ErrorCode::InvalidParams);
    }
}

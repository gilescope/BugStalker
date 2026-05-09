// SPDX-License-Identifier: MIT
//! Edit-and-continue DAP requests.

use crate::dap::yadap::protocol::DapRequest;
use crate::ui::command::{self, apply_patch};
use anyhow::{anyhow, bail};
use nix::libc::uintptr_t;
use serde_json::json;
use std::path::PathBuf;

use super::DebugSession;

impl DebugSession {
    /// `bs/applyPatch` — apply a wild-emitted patch file to the live
    /// debuggee. This is the DAP equivalent of the TUI
    /// `apply-patch <path> [<hex-base>]` command.
    ///
    /// Request:
    ///
    /// ```json
    /// { "path": "/tmp/app.wild-patch", "base": "0x100000000" }
    /// ```
    ///
    /// `base` is optional. When omitted, BugStalker maps file offsets
    /// through the loaded executable mapping itself.
    pub(super) fn handle_apply_patch(&mut self, req: &DapRequest) -> anyhow::Result<()> {
        let dbg = self
            .debugger
            .as_ref()
            .ok_or_else(|| anyhow!("bs/applyPatch: debugger not initialized"))?;
        let path = req
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("bs/applyPatch: missing arguments.path"))?;
        let base = parse_optional_base(req)?;
        let verify_executable_hash = parse_verify_executable_hash(req)?;

        let report = apply_patch::Handler::new(dbg)
            .handle(apply_patch::Command::ApplyPatch {
                path: PathBuf::from(path),
                base,
                verify_executable_hash,
            })
            .map_err(apply_patch_error)?;

        self.send_success_body(
            req,
            json!({
                "entriesApplied": report.entries_applied,
                "bytesWritten": report.bytes_written,
                "entriesSkippedDrift": report.entries_skipped_drift,
            }),
        )?;
        self.send_event_body(
            "invalidated",
            json!({ "areas": ["memory", "stack", "variables"] }),
        )
    }
}

fn apply_patch_error(err: command::CommandError) -> anyhow::Error {
    match err {
        command::CommandError::Parsing(msg) => anyhow!("bs/applyPatch: {msg}"),
        other => anyhow!("bs/applyPatch: {other}"),
    }
}

fn parse_optional_base(req: &DapRequest) -> anyhow::Result<Option<uintptr_t>> {
    let Some(base) = req.arguments.get("base") else {
        return Ok(None);
    };
    if base.is_null() {
        return Ok(None);
    }
    if let Some(n) = base.as_u64() {
        return Ok(Some(n as uintptr_t));
    }
    let Some(s) = base.as_str() else {
        bail!("bs/applyPatch: arguments.base must be a number or hex string");
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse::<u64>()
    }
    .map_err(|e| anyhow!("bs/applyPatch: invalid arguments.base {s:?}: {e}"))?;
    Ok(Some(parsed as uintptr_t))
}

fn parse_verify_executable_hash(req: &DapRequest) -> anyhow::Result<bool> {
    if let Some(value) = req.arguments.get("verifyExecutableHash") {
        return value.as_bool().ok_or_else(|| {
            anyhow!("bs/applyPatch: arguments.verifyExecutableHash must be a bool")
        });
    }
    if let Some(value) = req.arguments.get("allowHashMismatch") {
        return value
            .as_bool()
            .map(|allow| !allow)
            .ok_or_else(|| anyhow!("bs/applyPatch: arguments.allowHashMismatch must be a bool"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(arguments: serde_json::Value) -> DapRequest {
        DapRequest {
            seq: 1,
            r#type: "request".to_owned(),
            command: "bs/applyPatch".to_owned(),
            arguments,
        }
    }

    #[test]
    fn parses_missing_base_as_none() {
        assert_eq!(parse_optional_base(&request(json!({}))).unwrap(), None);
    }

    #[test]
    fn parses_hex_string_base() {
        assert_eq!(
            parse_optional_base(&request(json!({ "base": "0x1000" }))).unwrap(),
            Some(0x1000 as uintptr_t)
        );
    }

    #[test]
    fn rejects_bad_base() {
        assert!(parse_optional_base(&request(json!({ "base": [] }))).is_err());
    }

    #[test]
    fn parses_verify_executable_hash_default_true() {
        assert!(parse_verify_executable_hash(&request(json!({}))).unwrap());
    }

    #[test]
    fn parses_verify_executable_hash_false() {
        assert!(
            !parse_verify_executable_hash(&request(json!({
                "verifyExecutableHash": false
            })))
            .unwrap()
        );
    }

    #[test]
    fn parses_allow_hash_mismatch_alias() {
        assert!(
            !parse_verify_executable_hash(&request(json!({
                "allowHashMismatch": true
            })))
            .unwrap()
        );
    }
}

// SPDX-License-Identifier: MIT
pub mod command;
pub mod config;
pub mod console;
pub mod generic;
pub mod proto;
/// Phase 9 AI-bot scripting front-end. JSON-RPC 2.0 over stdin/stdout
/// with JSON5-with-comments input. See `doc/scripting/`.
pub mod script;
pub mod short;
/// Phase 9 structured-command core. The transport-agnostic dispatch
/// layer; `script` is one transport, DAP shims could call into the
/// same dispatch table.
pub mod structured;
pub mod supervisor;
mod syntax;
pub mod tui;

use os_pipe::PipeReader;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

#[derive(Clone)]
pub struct DebugeeOutReader(pub Arc<PipeReader>);

impl From<PipeReader> for DebugeeOutReader {
    fn from(pipe: PipeReader) -> Self {
        Self(Arc::new(pipe))
    }
}

impl AsRawFd for DebugeeOutReader {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Read for DebugeeOutReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.as_ref().read(buf)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum AppState {
    Initial,
    DebugeeRun,
    DebugeeBreak,
    UserInput,
    Finish,
}

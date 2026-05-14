// SPDX-License-Identifier: MIT
//! Method-name → handler dispatch.
//!
//! Adding a new command is two lines: add a `mod` in
//! `crate::ui::structured::commands`, then a row in `dispatch_table!`.

use crate::debugger::Debugger;
use crate::ui::structured::commands;
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::schema::CommandDescriptor;
use crate::ui::structured::{ResponseBudget, StructuredCommand, dispatch_one};

/// Macro: per-command row referenced by both runtime dispatch and
/// `--describe-commands`.
macro_rules! dispatch_table {
    ( $( $cmd:ty ),+ $(,)? ) => {
        /// Run one request against the debugger.
        pub fn run(
            method: &str,
            params: &serde_json::Value,
            dbg: &mut Debugger,
            budget: &ResponseBudget,
        ) -> Result<serde_json::Value, BsError> {
            $(
                if method == <$cmd>::METHOD {
                    return dispatch_one::<$cmd>(dbg, budget, params);
                }
            )+
            Err(BsError::new(
                ErrorCode::MethodNotFound,
                format!("no such method: {method}. \
                         Run --describe-commands for the catalogue."),
            ))
        }

        /// Catalogue for `--describe-commands`.
        pub fn catalogue() -> Vec<CommandDescriptor> {
            vec![
                $( CommandDescriptor::for_command::<$cmd>(), )+
            ]
        }
    };
}

dispatch_table![
    // Read-only.
    commands::backtrace::Backtrace,
    commands::frame::FrameInfo,
    commands::thread::ThreadInfo,
    commands::sharedlib::SharedlibInfo,
    commands::r#break::BreakInfo,
    commands::watch::WatchInfo,
    commands::print_var::Var,
    commands::print_var::Arg,
    // Stateful.
    commands::run::Run,
    commands::r#continue::Continue,
    commands::step::StepInto,
    commands::step::StepOver,
    commands::step::StepOut,
    commands::step::StepInstruction,
    commands::r#break::BreakSet,
    commands::r#break::BreakRemove,
    commands::watch::WatchSet,
    commands::watch::WatchRemove,
];

// SPDX-License-Identifier: MIT
//! `var` and `arg` — read variable / argument values in the focused
//! frame.
//!
//! v1 returns:
//!   - `name`: identity name as known to DWARF.
//!   - `type`: type name as a string.
//!   - `value_text`: the same rendered text the console shows, with no
//!     ANSI colour codes.
//!   - `address`: VAS address of the value if known.
//!
//! Future v2 will add a recursive `tree: ValueNode` so agents can
//! introspect struct fields without re-querying. For v1, fields are
//! reachable by passing a DQE expression in `expression`:
//!
//! ```jsonc
//! { method: "var", params: { expression: "dyn_box.value" } }
//! { method: "var", params: { expression: "vec[0]" } }
//! ```

use chumsky::Parser;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::debugger::Debugger;
use crate::debugger::variable::dqe::{Dqe, Selector};
use crate::debugger::variable::execute::QueryResult;
use crate::debugger::variable::render::RenderValue;
use crate::ui::command::parser::expression;
use crate::ui::generic::variable::render_value;
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::envelope::ListResponse;
use crate::ui::structured::{ResponseBudget, StructuredCommand};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Var {
    /// Variable to read. Either a bare name (`"foo"`) or a DQE
    /// expression (`"foo.bar[0]"`). When omitted, all locals are returned.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expression: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Arg {
    /// Argument to read. Either a bare name or a DQE expression. When
    /// omitted, all arguments are returned.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expression: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VarResult {
    pub name: Option<String>,
    pub r#type: String,
    pub value_text: String,
}

impl<'a> From<&QueryResult<'a>> for VarResult {
    fn from(qr: &QueryResult<'a>) -> Self {
        let value = qr.value();
        Self {
            name: qr.identity().name.clone(),
            r#type: value.r#type().name_fmt().to_string(),
            value_text: render_value(value),
        }
    }
}

fn build_dqe(name: Option<&str>, expression: Option<&str>) -> Result<Dqe, BsError> {
    match (name, expression) {
        (Some(_), Some(_)) => Err(BsError::new(
            ErrorCode::InvalidParams,
            "pass exactly one of `name` or `expression`",
        )),
        (None, None) => Ok(Dqe::Variable(Selector::Any)),
        (Some(n), None) => Ok(Dqe::Variable(Selector::by_name(n, false))),
        (None, Some(expr)) => expression::parser().parse(expr).into_result().map_err(|errs| {
            BsError::new(
                ErrorCode::BadExpression,
                format!(
                    "DQE parse failed: {}",
                    errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ")
                ),
            )
        }),
    }
}

impl StructuredCommand for Var {
    const METHOD: &'static str = "var";
    const SUMMARY: &'static str =
        "Read variable(s) in the focused frame. Returns all locals when no name/expression given.";
    type Response = ListResponse<VarResult>;

    fn execute(
        self,
        dbg: &mut Debugger,
        budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let dqe = build_dqe(self.name.as_deref(), self.expression.as_deref())?;
        let qrs = dbg.read_variable(dqe)?;
        let items: Vec<VarResult> = qrs.iter().map(VarResult::from).collect();
        Ok(ListResponse::from_iter(items, budget.item_cap()))
    }
}

impl StructuredCommand for Arg {
    const METHOD: &'static str = "arg";
    const SUMMARY: &'static str =
        "Read argument(s) in the focused frame. Returns all arguments when no name/expression given.";
    type Response = ListResponse<VarResult>;

    fn execute(
        self,
        dbg: &mut Debugger,
        budget: &ResponseBudget,
    ) -> Result<Self::Response, BsError> {
        let dqe = build_dqe(self.name.as_deref(), self.expression.as_deref())?;
        let qrs = dbg.read_argument(dqe)?;
        let items: Vec<VarResult> = qrs.iter().map(VarResult::from).collect();
        Ok(ListResponse::from_iter(items, budget.item_cap()))
    }
}

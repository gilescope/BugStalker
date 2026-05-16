// SPDX-License-Identifier: MIT
//! `assert.*` — in-script assertions for the test-runner front-end.
//!
//! Each method runs an inner command (or peeks at the event buffer)
//! and deep-partial-matches the JSON response against an `expect`
//! pattern. The reply carries `{ passed, hint?, mismatch?, got }`;
//! the `bs --test` runner accumulates these into TAP output.
//!
//! Operators inside `expect` use Mongo-style `$`-prefixed keys:
//!
//! | op             | meaning                                    |
//! | -------------- | ------------------------------------------ |
//! | `$eq`          | strict equality (escape for literal `$…`)  |
//! | `$ne`          | not-equal                                  |
//! | `$contains`    | substring (string) / element (array)       |
//! | `$regex`       | full regex match against a string          |
//! | `$starts_with` | string prefix                              |
//! | `$ends_with`   | string suffix                              |
//! | `$exists`      | field present (bool arg)                   |
//! | `$any_of`      | matches any of an array of sub-patterns    |
//!
//! Plain values match by equality. Objects are deep partial matches —
//! every key in `expected` must exist in `actual`, extras allowed.
//! Arrays must match in length, element by element.

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::debugger::Debugger;
use crate::ui::structured::commands::print_var::{Arg, Var};
use crate::ui::structured::error::{BsError, ErrorCode};
use crate::ui::structured::{ResponseBudget, StructuredCommand};

// -- shared envelope ---------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Mismatch {
    /// jq-style path within the actual response. Empty for the root.
    pub path: String,
    /// The pattern the matcher was applying.
    pub expected: Value,
    /// What was actually there.
    pub got: Value,
    /// Why the matcher rejected the pair (one short clause).
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssertResult {
    /// `true` iff the response matched `expect`.
    pub passed: bool,
    /// Caller-supplied label for runner output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Populated on `passed == false`. The first mismatch encountered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<Mismatch>,
    /// The full response from the inner command. `--bless` reads this
    /// to rewrite the `expect:` block on the disk script.
    pub got: Value,
    /// `true` when the request omitted `expect:` (or supplied an empty
    /// `{}`). Pass + unset is the "initial fill" case the bless runner
    /// uses to populate fresh templates.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expect_was_unset: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// -- assert.var / assert.arg ------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertVar {
    /// Variable name (bare). Mutually exclusive with `expression`.
    #[serde(default)]
    pub name: Option<String>,
    /// DQE expression (`foo.bar[0]`). Mutually exclusive with `name`.
    #[serde(default)]
    pub expression: Option<String>,
    /// Pattern the underlying `var` response must match. When omitted,
    /// the matcher succeeds against anything (useful as a placeholder
    /// for `--record`/`--bless` to fill in).
    #[serde(default)]
    pub expect: Value,
    /// Optional label used by the TAP runner.
    #[serde(default)]
    pub hint: Option<String>,
}

impl StructuredCommand for AssertVar {
    const METHOD: &'static str = "assert.var";
    const SUMMARY: &'static str = "Read a variable and assert its response shape matches `expect`. \
         Used by `bs --test` and `bs --record`.";
    type Response = AssertResult;

    fn execute(self, dbg: &mut Debugger, budget: &ResponseBudget) -> Result<AssertResult, BsError> {
        let inner = Var {
            name: self.name,
            expression: self.expression,
        };
        let resp = inner.execute(dbg, budget)?;
        let got = serde_json::to_value(&resp).map_err(serialise_err)?;
        Ok(run_match(self.expect, got, self.hint))
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertArg {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expression: Option<String>,
    #[serde(default)]
    pub expect: Value,
    #[serde(default)]
    pub hint: Option<String>,
}

impl StructuredCommand for AssertArg {
    const METHOD: &'static str = "assert.arg";
    const SUMMARY: &'static str =
        "Read an argument and assert its response shape matches `expect`.";
    type Response = AssertResult;

    fn execute(self, dbg: &mut Debugger, budget: &ResponseBudget) -> Result<AssertResult, BsError> {
        let inner = Arg {
            name: self.name,
            expression: self.expression,
        };
        let resp = inner.execute(dbg, budget)?;
        let got = serde_json::to_value(&resp).map_err(serialise_err)?;
        Ok(run_match(self.expect, got, self.hint))
    }
}

// -- assert.frame ------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertFrame {
    /// Pattern that `frame.info`'s response must match. See
    /// module-level docs for the operator vocabulary.
    #[serde(default)]
    pub expect: Value,
    #[serde(default)]
    pub hint: Option<String>,
}

impl StructuredCommand for AssertFrame {
    const METHOD: &'static str = "assert.frame";
    const SUMMARY: &'static str =
        "Read the focused frame's metadata and assert it matches `expect`.";
    type Response = AssertResult;

    fn execute(self, dbg: &mut Debugger, budget: &ResponseBudget) -> Result<AssertResult, BsError> {
        let inner = crate::ui::structured::commands::frame::FrameInfo {};
        let resp = inner.execute(dbg, budget)?;
        let got = serde_json::to_value(&resp).map_err(serialise_err)?;
        Ok(run_match(self.expect, got, self.hint))
    }
}

// -- explicit pass / fail ---------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertOk {
    pub hint: String,
}

impl StructuredCommand for AssertOk {
    const METHOD: &'static str = "assert.ok";
    const SUMMARY: &'static str =
        "Always-passing assertion. Use to mark a reached-this-point checkpoint.";
    type Response = AssertResult;

    fn execute(
        self,
        _dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<AssertResult, BsError> {
        Ok(AssertResult {
            passed: true,
            hint: Some(self.hint),
            mismatch: None,
            got: Value::Null,
            expect_was_unset: false,
        })
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssertFail {
    pub reason: String,
    #[serde(default)]
    pub hint: Option<String>,
}

impl StructuredCommand for AssertFail {
    const METHOD: &'static str = "assert.fail";
    const SUMMARY: &'static str = "Always-failing assertion. Use to mark an unreachable arm.";
    type Response = AssertResult;

    fn execute(
        self,
        _dbg: &mut Debugger,
        _budget: &ResponseBudget,
    ) -> Result<AssertResult, BsError> {
        Ok(AssertResult {
            passed: false,
            hint: self.hint,
            mismatch: Some(Mismatch {
                path: String::new(),
                expected: Value::Null,
                got: Value::Null,
                reason: self.reason,
            }),
            got: Value::Null,
            expect_was_unset: false,
        })
    }
}

// -- glue --------------------------------------------------------------

fn serialise_err(e: serde_json::Error) -> BsError {
    BsError::new(
        ErrorCode::Internal,
        format!("failed to serialise inner response: {e}"),
    )
}

fn run_match(expect: Value, got: Value, hint: Option<String>) -> AssertResult {
    // An omitted (Null) expect block is a "no-op" — passes against any
    // response. This is what `--record` / `--bless` synthesise when
    // priming a script: the first run will surface `got` so the user
    // can promote it to an expectation. `expect_was_unset` lets the
    // bless runner distinguish "intentional pass" from "first run".
    let unset = is_unset(&expect);
    let passed = if unset {
        true
    } else {
        match_value(&expect, &got, "").is_none()
    };
    let mismatch = if passed {
        None
    } else {
        match_value(&expect, &got, "")
    };
    AssertResult {
        passed,
        hint,
        mismatch,
        got,
        expect_was_unset: unset,
    }
}

fn is_unset(v: &Value) -> bool {
    matches!(v, Value::Null) || matches!(v, Value::Object(o) if o.is_empty())
}

// -- matcher -----------------------------------------------------------

/// Deep partial match. Returns `Some(Mismatch)` for the first failure
/// found (in pre-order traversal), `None` on success.
pub fn match_value(expected: &Value, actual: &Value, path: &str) -> Option<Mismatch> {
    // Operator object? Check for `$`-prefixed keys.
    if let Value::Object(obj) = expected
        && let Some((op_key, op_arg)) = obj.iter().find(|(k, _)| k.starts_with('$'))
    {
        // Only one operator per object; mixing operators with regular
        // keys is rejected as a script error.
        if obj.len() != 1 {
            return Some(Mismatch {
                path: path.to_string(),
                expected: expected.clone(),
                got: actual.clone(),
                reason: format!(
                    "operator object must contain exactly one `$…` key; got {} keys",
                    obj.len()
                ),
            });
        }
        return match_operator(op_key, op_arg, actual, path);
    }

    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (k, ev) in e {
                let child_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match a.get(k) {
                    Some(av) => {
                        if let Some(m) = match_value(ev, av, &child_path) {
                            return Some(m);
                        }
                    }
                    None => {
                        return Some(Mismatch {
                            path: child_path,
                            expected: ev.clone(),
                            got: Value::Null,
                            reason: "key missing in response".to_string(),
                        });
                    }
                }
            }
            None
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                return Some(Mismatch {
                    path: path.to_string(),
                    expected: Value::from(e.len()),
                    got: Value::from(a.len()),
                    reason: "array length mismatch".to_string(),
                });
            }
            for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                let child_path = format!("{path}[{i}]");
                if let Some(m) = match_value(ev, av, &child_path) {
                    return Some(m);
                }
            }
            None
        }
        // Cross-type comparisons are mismatches except for the explicit
        // operator forms handled above.
        (e, a) if e == a => None,
        (e, a) => Some(Mismatch {
            path: path.to_string(),
            expected: e.clone(),
            got: a.clone(),
            reason: "value mismatch".to_string(),
        }),
    }
}

fn match_operator(op: &str, arg: &Value, actual: &Value, path: &str) -> Option<Mismatch> {
    let fail = |reason: &str| -> Option<Mismatch> {
        Some(Mismatch {
            path: path.to_string(),
            expected: serde_json::json!({ op: arg.clone() }),
            got: actual.clone(),
            reason: reason.to_string(),
        })
    };
    match op {
        "$eq" => {
            if arg == actual {
                None
            } else {
                fail("$eq: values differ")
            }
        }
        "$ne" => {
            if arg != actual {
                None
            } else {
                fail("$ne: values are equal")
            }
        }
        "$contains" => match (arg, actual) {
            (Value::String(needle), Value::String(haystack)) => {
                if haystack.contains(needle.as_str()) {
                    None
                } else {
                    fail("$contains: substring not found")
                }
            }
            (_, Value::Array(items)) => {
                if items.iter().any(|i| match_value(arg, i, "").is_none()) {
                    None
                } else {
                    fail("$contains: no array element matches")
                }
            }
            _ => fail("$contains: expects string-in-string or pattern-in-array"),
        },
        "$regex" => {
            let pat = arg
                .as_str()
                .ok_or(())
                .map_err(|_| ())
                .and_then(|s| Regex::new(s).map_err(|_| ()));
            let haystack = match actual {
                Value::String(s) => s.as_str(),
                _ => return fail("$regex: actual value is not a string"),
            };
            match pat {
                Ok(re) => {
                    if re.is_match(haystack) {
                        None
                    } else {
                        fail("$regex: pattern did not match")
                    }
                }
                Err(_) => fail("$regex: invalid pattern or non-string argument"),
            }
        }
        "$starts_with" => match (arg, actual) {
            (Value::String(p), Value::String(s)) => {
                if s.starts_with(p.as_str()) {
                    None
                } else {
                    fail("$starts_with: prefix not found")
                }
            }
            _ => fail("$starts_with: both arg and value must be strings"),
        },
        "$ends_with" => match (arg, actual) {
            (Value::String(p), Value::String(s)) => {
                if s.ends_with(p.as_str()) {
                    None
                } else {
                    fail("$ends_with: suffix not found")
                }
            }
            _ => fail("$ends_with: both arg and value must be strings"),
        },
        "$exists" => {
            let want = arg.as_bool().unwrap_or(true);
            let present = !matches!(actual, Value::Null);
            if want == present {
                None
            } else if want {
                fail("$exists(true): value is null")
            } else {
                fail("$exists(false): value is present")
            }
        }
        "$any_of" => {
            let alternatives = match arg {
                Value::Array(a) => a,
                _ => return fail("$any_of: argument must be an array of patterns"),
            };
            for alt in alternatives {
                match_value(alt, actual, "")?;
            }
            fail("$any_of: no alternative matched")
        }
        _ => fail(&format!("unknown operator: {op}")),
    }
}

// -- tests -------------------------------------------------------------

#[cfg(test)]
mod matcher_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_equality() {
        assert!(match_value(&json!(7), &json!(7), "").is_none());
        assert!(match_value(&json!(7), &json!(8), "").is_some());
    }

    #[test]
    fn object_is_partial_match() {
        let expected = json!({ "name": "x" });
        let actual = json!({ "name": "x", "extra": 1 });
        assert!(match_value(&expected, &actual, "").is_none());
    }

    #[test]
    fn object_missing_key_fails_with_path() {
        let expected = json!({ "name": "x" });
        let actual = json!({});
        let m = match_value(&expected, &actual, "").unwrap();
        assert_eq!(m.path, "name");
        assert_eq!(m.reason, "key missing in response");
    }

    #[test]
    fn array_length_must_match() {
        assert!(match_value(&json!([1, 2]), &json!([1, 2, 3]), "").is_some());
        assert!(match_value(&json!([1, 2]), &json!([1, 2]), "").is_none());
    }

    #[test]
    fn array_path_carries_index() {
        let expected = json!([{ "v": 1 }, { "v": 2 }]);
        let actual = json!([{ "v": 1 }, { "v": 99 }]);
        let m = match_value(&expected, &actual, "items").unwrap();
        assert_eq!(m.path, "items[1].v");
    }

    #[test]
    fn op_regex_matches() {
        let expected = json!({ "$regex": "^0x[0-9a-fA-F]+$" });
        assert!(match_value(&expected, &json!("0xDEADBEEF"), "").is_none());
        assert!(match_value(&expected, &json!("not an addr"), "").is_some());
    }

    #[test]
    fn op_contains_substring() {
        let expected = json!({ "$contains": "Point" });
        assert!(match_value(&expected, &json!("Box<dyn T> [→ Point]"), "").is_none());
        assert!(match_value(&expected, &json!("Box<dyn T>"), "").is_some());
    }

    #[test]
    fn op_starts_ends_with() {
        assert!(
            match_value(
                &json!({ "$starts_with": "Result::" }),
                &json!("Result::Ok(7)"),
                ""
            )
            .is_none()
        );
        assert!(
            match_value(&json!({ "$ends_with": "(7)" }), &json!("Result::Ok(7)"), "").is_none()
        );
    }

    #[test]
    fn op_exists_true_false() {
        assert!(match_value(&json!({ "$exists": true }), &json!("anything"), "").is_none());
        assert!(match_value(&json!({ "$exists": true }), &json!(null), "").is_some());
        assert!(match_value(&json!({ "$exists": false }), &json!(null), "").is_none());
    }

    #[test]
    fn op_any_of() {
        let expected = json!({ "$any_of": ["a", "b", "c"] });
        assert!(match_value(&expected, &json!("a"), "").is_none());
        assert!(match_value(&expected, &json!("c"), "").is_none());
        assert!(match_value(&expected, &json!("d"), "").is_some());
    }

    #[test]
    fn op_mixed_with_keys_is_a_script_error() {
        // `{ $regex: "...", trailing: 1 }` is rejected — single op per
        // object. Without this, callers would write ambiguous patterns.
        let expected = json!({ "$regex": "x", "trailing": 1 });
        let m = match_value(&expected, &json!("x"), "").unwrap();
        assert!(
            m.reason
                .contains("operator object must contain exactly one")
        );
    }

    #[test]
    fn nested_operator_inside_object() {
        // The pattern people actually want to write.
        let expected = json!({
            "items": [{
                "name": "tuple_1",
                "type": { "$regex": r"^\(f64, f64\)$" },
                "value_text": "(0, 1.1)"
            }]
        });
        let actual = json!({
            "items": [{
                "name": "tuple_1",
                "type": "(f64, f64)",
                "value_text": "(0, 1.1)",
                "address": "0x12345"
            }],
            "total": 1,
            "truncated": false
        });
        assert!(match_value(&expected, &actual, "").is_none());
    }

    #[test]
    fn nested_operator_failure_reports_inner_path() {
        let expected = json!({
            "items": [{ "value_text": { "$contains": "Point" } }]
        });
        let actual = json!({
            "items": [{ "value_text": "Some(0x123)" }]
        });
        let m = match_value(&expected, &actual, "").unwrap();
        assert_eq!(m.path, "items[0].value_text");
    }

    #[test]
    fn run_match_passes_when_expect_is_empty() {
        // Empty `expect:` blocks (synthesised by --record before bless)
        // pass against any response.
        let result = run_match(json!({}), json!({ "anything": 1 }), None);
        assert!(result.passed);
        assert!(result.mismatch.is_none());
    }

    #[test]
    fn run_match_returns_first_mismatch_with_got() {
        let result = run_match(
            json!({ "x": 1 }),
            json!({ "x": 2, "y": 3 }),
            Some("hello".into()),
        );
        assert!(!result.passed);
        assert_eq!(result.hint.as_deref(), Some("hello"));
        let m = result.mismatch.unwrap();
        assert_eq!(m.path, "x");
        // got carries the full response so --bless knows what to write.
        assert_eq!(result.got, json!({ "x": 2, "y": 3 }));
    }
}

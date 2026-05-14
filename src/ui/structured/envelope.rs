// SPDX-License-Identifier: MIT
//! Envelope types for structured responses.
//!
//! Two cross-cutting concerns live here: per-request output budgeting and
//! list-shaped paginated responses. Both are needed because agents have
//! finite context windows and a `var print local_map` on a 100k-entry
//! HashMap is destructive.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-request hint from the agent about how much it can absorb. The
/// transport reads `max_response_bytes` from the JSON-RPC envelope (not
/// from the method's `params`) and threads it through.
#[derive(Debug, Clone, Default)]
pub struct ResponseBudget {
    pub max_response_bytes: Option<usize>,
    pub include_timestamps: bool,
}

impl ResponseBudget {
    /// Truncation budget for list responses, in items. Heuristic: split
    /// the byte budget into ~256 B per item. Without a budget, return a
    /// generous default (1024) — agents that want unbounded output should
    /// page rather than rely on this default.
    pub fn item_cap(&self) -> usize {
        match self.max_response_bytes {
            Some(b) => (b / 256).max(1),
            None => 1024,
        }
    }
}

/// A list-shaped response. Every list-returning command uses this so
/// callers can detect truncation and page through.
///
/// The `cursor` is opaque — agents must not parse it. By convention it
/// starts with `"bs:"` (see `Cursor`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListResponse<T: serde::Serialize + JsonSchema> {
    pub items: Vec<T>,
    pub total: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

impl<T: serde::Serialize + JsonSchema> ListResponse<T> {
    pub fn from_iter<I: IntoIterator<Item = T>>(iter: I, cap: usize) -> Self {
        let mut items: Vec<T> = iter.into_iter().collect();
        let total = items.len();
        let truncated = total > cap;
        if truncated {
            items.truncate(cap);
        }
        Self {
            items,
            total,
            truncated,
            cursor: if truncated {
                Some(Cursor::offset(cap))
            } else {
                None
            },
        }
    }

    pub fn full(items: Vec<T>) -> Self {
        let total = items.len();
        Self {
            items,
            total,
            truncated: false,
            cursor: None,
        }
    }
}

/// Opaque continuation token. Agents must not parse it. Encoded
/// `bs:offset:<n>` for v1; future schemes prefix differently.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    pub fn offset(n: usize) -> Self {
        Self(format!("bs:offset:{n}"))
    }

    /// Parse offset cursor back. Returns None if the cursor isn't an
    /// offset cursor, so older clients passing a stale opaque blob are
    /// rejected loudly rather than silently misinterpreted.
    pub fn parse_offset(&self) -> Option<usize> {
        self.0.strip_prefix("bs:offset:")?.parse().ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

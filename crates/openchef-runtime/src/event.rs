//! Normalized events of a turn (§6.1).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

/// One normalized occurrence in a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// When it happened.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// What happened.
    #[serde(flatten)]
    pub body: EventBody,
}

impl Event {
    /// An event stamped now.
    #[must_use]
    pub fn now(body: EventBody) -> Self {
        Self {
            at: OffsetDateTime::now_utc(),
            body,
        }
    }

    /// Whether the event ends a turn.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.body.is_terminal()
    }
}

/// The kinds of event. `TurnCompleted` and `TurnFailed` are terminal: a turn delivers
/// exactly one of them, last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventBody {
    /// Streamed assistant text.
    Text(String),
    /// The agent invokes a tool.
    ToolCall(ToolCall),
    /// A tool call finished.
    ToolResult(ToolResult),
    /// Quota information observed in-stream (L1, §10.1).
    Quota(QuotaSignal),
    /// The turn completed.
    TurnCompleted(TurnResult),
    /// The turn failed.
    TurnFailed(Failure),
}

impl EventBody {
    /// Whether this ends a turn.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::TurnCompleted(_) | Self::TurnFailed(_))
    }
}

/// The agent invoking a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The CLI's id for the call.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The tool name in Claude Code vocabulary where one exists.
    pub name: String,
    /// The input as the CLI reported it.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input: Value,
    /// Identifies "the same call" for loop detection: name plus a canonical input.
    pub signature: String,
}

/// A finished tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The call it answers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Output, possibly truncated by the CLI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// Quota information observed in-stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSignal {
    /// The windows reported.
    pub windows: Vec<QuotaWindow>,
    /// The CLI reports that the limit is reached and cannot be extended.
    pub exhausted: bool,
}

/// One rate-limit window, normalized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// `five_hour`, `weekly` or a CLI-specific name.
    pub name: String,
    /// In `[0, 1]`.
    pub utilization: f64,
    /// The absolute reset time, if known.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<OffsetDateTime>,
    /// The window length, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
}

/// Token and cost accounting of one turn, from the CLI's turn-level totals only (§6.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    /// Tokens written to the prompt cache.
    pub cache_write_tokens: u64,
    /// Cost the CLI reported, if any (API pools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// The outcome of a completed turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnResult {
    /// The final answer from a single source (never streamed text plus the result).
    pub final_text: String,
    /// The schema-validated answer when the turn had an output schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Accounting.
    pub usage: Usage,
    /// Permission requests refused during the turn.
    pub denials: u32,
    /// The models that served the turn, where the CLI reports it (D-008).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Wall time of the turn.
    pub duration: Duration,
}

/// Why a turn did not complete (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The pool's quota is exhausted.
    QuotaExhausted,
    /// Rate limited; retry after a backoff.
    RateLimited,
    /// Not logged in, or the login expired.
    Auth,
    /// The conversation no longer fits the context window.
    ContextOverflow,
    /// The CLI process died.
    Crashed,
    /// The turn exceeded its time limit.
    Timeout,
    /// The turn was interrupted.
    Interrupted,
    /// The CLI broke the protocol.
    Protocol,
    /// Anything else.
    Unknown,
}

impl FailureKind {
    /// The string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QuotaExhausted => "quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::Auth => "auth",
            Self::ContextOverflow => "context_overflow",
            Self::Crashed => "crashed",
            Self::Timeout => "timeout",
            Self::Interrupted => "interrupted",
            Self::Protocol => "protocol",
            Self::Unknown => "unknown",
        }
    }
}

/// A failed turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    /// The classification.
    pub kind: FailureKind,
    /// The CLI's message.
    pub message: String,
    /// For quota and rate-limit failures: when retrying makes sense.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<OffsetDateTime>,
    /// Usage spent before the failure, if reported.
    pub usage: Usage,
}

impl Failure {
    /// A failure with a message and no usage or reset time.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            resets_at: None,
            usage: Usage::default(),
        }
    }
}

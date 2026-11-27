//! Approval queue methods (§9.2), ported from Go `api/approvals.go`. Requests reach
//! viewers as task events (`approval_requested`, `approval_decided`).

use openchef_core::Approval;
use serde::{Deserialize, Serialize};

use crate::{Client, Error};

/// Lists queued requests.
pub const METHOD_APPROVAL_LIST: &str = "approval/list";
/// Settles a request.
pub const METHOD_APPROVAL_DECIDE: &str = "approval/decide";
/// Queues an agent's question and waits for the answer (the `ask_user` MCP server).
pub const METHOD_APPROVAL_ASK: &str = "approval/ask";

/// A request was already settled; the message says how.
pub const CODE_APPROVAL_SETTLED: i64 = -32011;

/// `approval/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalListParams {
    /// Only this task's requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Include settled requests; by default only pending ones.
    #[serde(default)]
    pub all: bool,
    /// At most the newest this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// How a person settles a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Allow a permission request.
    Allow,
    /// Deny a permission request.
    Deny,
    /// Answer a question.
    Answer,
}

/// `approval/decide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecideParams {
    /// The request.
    pub id: i64,
    /// The verdict.
    pub verdict: Verdict,
    /// The answer to a question, or the message a denied agent sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `approval/ask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskParams {
    /// The task asking.
    pub task: String,
    /// The agent asking.
    pub agent: String,
    /// The question.
    pub question: String,
    /// Suggested answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// How a question was settled; `answer` is set only when it was answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskResult {
    /// The approval status.
    pub status: String,
    /// The answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

impl Client {
    /// Queued requests, oldest first.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn approvals(&self, p: &ApprovalListParams) -> Result<Vec<Approval>, Error> {
        self.call(METHOD_APPROVAL_LIST, p).await
    }

    /// Settles a request.
    ///
    /// # Errors
    ///
    /// When the call fails; a late decision carries [`CODE_APPROVAL_SETTLED`].
    pub async fn decide(&self, p: &DecideParams) -> Result<Approval, Error> {
        self.call(METHOD_APPROVAL_DECIDE, p).await
    }

    /// Puts a question to the user and waits.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn ask(&self, p: &AskParams) -> Result<AskResult, Error> {
        self.call(METHOD_APPROVAL_ASK, p).await
    }
}

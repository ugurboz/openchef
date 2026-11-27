//! Permission requests and questions from agents (§9).

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionRef;

/// A boxed, sendable future: the shape of every callback the adapters await.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the agent wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Run a shell command.
    Command,
    /// Write, edit or delete files.
    FileChange,
    /// Any other tool (MCP, web, …).
    Tool,
    /// Change the permission mode (e.g. Claude's `ExitPlanMode`).
    ModeChange,
}

impl From<RequestKind> for openchef_core::ApprovalKind {
    fn from(k: RequestKind) -> Self {
        match k {
            RequestKind::Command => Self::Command,
            RequestKind::FileChange => Self::FileChange,
            RequestKind::Tool => Self::Tool,
            RequestKind::ModeChange => Self::ModeChange,
        }
    }
}

/// A normalized permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The session asking.
    pub session: SessionRef,
    /// What is asked.
    pub kind: RequestKind,
    /// The tool in Claude Code vocabulary (`Bash`, `Edit`, `Write`, `Read`, `WebFetch`,
    /// `mcp__server__tool`), so permission rules apply uniformly.
    pub tool: String,
    /// The shell command, for `Command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The files concerned, absolute where the CLI reports them so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The CLI's description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The tool input.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input: Value,
    /// The CLI's original request, for audit.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
}

/// The answer to an [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// Whether the request is allowed.
    pub allow: bool,
    /// Remember an allow for the rest of the session where the CLI supports it.
    #[serde(default)]
    pub for_session: bool,
    /// Shown to the agent on deny.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

impl Decision {
    /// Allow once.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            allow: true,
            for_session: false,
            message: String::new(),
        }
    }

    /// Refuse, with a message for the agent.
    #[must_use]
    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            allow: false,
            for_session: false,
            message: message.into(),
        }
    }
}

/// The agent asking the user something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The session asking.
    pub session: SessionRef,
    /// The question.
    pub text: String,
    /// Suggested answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// Why an approver could not decide (the adapter then denies).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("approver: {0}")]
pub struct ApproverError(pub String);

/// Decides permission requests and answers questions. Adapters call it while the
/// agent waits; implementations may wait for a person and must stop when `cancel`
/// fires.
pub trait Approver: Send + Sync + 'static {
    /// Decides one request.
    fn approve(
        &self,
        request: ApprovalRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>>;

    /// Answers one question.
    fn answer(
        &self,
        question: Question,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>>;
}

//! Queued permission requests and questions (§9.2, D-022).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Error, TaskId};

/// How long a request waits before it expires (§9.2).
pub const APPROVAL_TIMEOUT: Duration = Duration::from_mins(15);

string_enum! {
    /// What a queued request asks the user for: a permission for one of the runtime's
    /// approval kinds, or an answer to an agent's question.
    pub enum ApprovalKind: "approval kind" {
        /// Run a shell command.
        Command => "command",
        /// Change a file.
        FileChange => "file_change",
        /// Use another tool.
        Tool => "tool",
        /// Change the session's permission mode.
        ModeChange => "mode_change",
        /// Answer an agent's question.
        Question => "question",
    }
}

string_enum! {
    /// Where a queued request stands. `Pending` is the only non-final state.
    pub enum ApprovalStatus: "approval status" {
        /// Waiting for a decision.
        Pending => "pending",
        /// Permission granted.
        Allowed => "allowed",
        /// Permission refused.
        Denied => "denied",
        /// The question was answered.
        Answered => "answered",
        /// Nobody answered in time, or the daemon that asked is gone. A permission is
        /// denied; a question gets the "no one is available" answer.
        Expired => "expired",
        /// The agent stopped waiting (its turn ended, the task was cancelled).
        Withdrawn => "withdrawn",
    }
}

impl ApprovalStatus {
    /// Whether the request is settled.
    #[must_use]
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

string_enum! {
    /// Who settled a request.
    pub enum DecidedBy: "decider" {
        /// A person, through the TUI or CLI.
        User => "user",
        /// The timeout expired.
        Timeout => "timeout",
        /// The daemon: a restart, a cancel or a turn that ended.
        Daemon => "daemon",
    }
}

/// One queued permission request or question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// Store id, typed by users (`openchef approve 12`).
    pub id: i64,
    /// The task that asked.
    #[serde(rename = "task")]
    pub task_id: TaskId,
    /// The agent that asked.
    pub agent: String,
    /// What is asked.
    pub kind: ApprovalKind,
    /// Where it stands.
    pub status: ApprovalStatus,
    /// The tool in Claude Code vocabulary (`Bash`, `Edit`, `WebFetch`, `mcp__…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The one line a person decides on: the command, the paths, the question.
    pub summary: String,
    /// A question's suggested answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The user's text (question) or message to the agent (deny).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Who settled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<DecidedBy>,
    /// When it was queued.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it expires.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// When it was settled.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub decided_at: Option<OffsetDateTime>,
}

/// How a pending request is settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The final status.
    pub status: ApprovalStatus,
    /// The answer or the message to the agent.
    pub answer: Option<String>,
    /// Who decided.
    pub decided_by: DecidedBy,
}

impl Decision {
    /// Checks that this decision can settle a request of `kind`.
    pub fn validate(&self, kind: ApprovalKind) -> Result<(), Error> {
        match self.status {
            ApprovalStatus::Allowed | ApprovalStatus::Denied => {
                if kind == ApprovalKind::Question {
                    return Err(Error::Invalid(format!(
                        "a question is answered, not {}",
                        self.status
                    )));
                }
            }
            ApprovalStatus::Answered => {
                if kind != ApprovalKind::Question {
                    return Err(Error::Invalid(format!(
                        "only a question can be answered; approve or deny a {kind} request"
                    )));
                }
                if self.answer.as_deref().is_none_or(str::is_empty) {
                    return Err(Error::Invalid("empty answer".to_owned()));
                }
            }
            ApprovalStatus::Expired | ApprovalStatus::Withdrawn => {}
            ApprovalStatus::Pending => {
                return Err(Error::Invalid(format!(
                    "approval status {:?} is not a decision",
                    self.status.as_str()
                )));
            }
        }
        Ok(())
    }
}

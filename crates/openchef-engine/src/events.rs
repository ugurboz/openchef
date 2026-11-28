//! What the engine reports while it works, streamed live to `run`, `logs -f` and
//! `status` (D-014).

use openchef_core::{Approval, TaskId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::verify::Outcome;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// `status`, `message` (the reason).
    TaskStatus,
    /// `step`, `message` (the title).
    StepStarted,
    /// `attempt`, `agent`.
    AttemptStarted,
    /// `runtime`: a normalized agent event.
    Agent,
    /// `message` (the command).
    VerifyStarted,
    /// `verify`.
    VerifyFinished,
    /// `status` (attempt status), `message` (outcome).
    AttemptEnded,
    /// `status` (step status), `message` (commit or reason).
    StepFinished,
    /// `message`: retries, backoff, failover, nudges.
    Notice,
    /// `agent`, `approval`: a request or question waits for the user (§9.2).
    ApprovalRequested,
    /// `agent`, `approval`: a request was settled.
    ApprovalDecided,
    /// `step`, `attempt`, `agent` (the reviewer), `message` (the round).
    ReviewStarted,
    /// `step`, `attempt`, `agent`, `status` (`clean`, `findings`, `skipped`), `message`
    /// (the counts or why it was skipped).
    ReviewFinished,
    /// `step` 0, `attempt`, `agent` (the critic), `message` (the round): a plan critique.
    CouncilStarted,
    /// `step` 0, `status` (`locked`, `findings`, `skipped`), `message` (the counts or
    /// why): the council's outcome for the plan.
    CouncilFinished,
    /// `step`, `attempt`, `agent` (the arbiter), `message` (the disputed findings).
    ArbitrationStarted,
    /// `step`, `attempt`, `agent`, `status` (`ruled`, `skipped`), `message` (upheld and
    /// dismissed counts, or why it was skipped).
    ArbitrationFinished,
}

impl EventKind {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TaskStatus => "task_status",
            Self::StepStarted => "step_started",
            Self::AttemptStarted => "attempt_started",
            Self::Agent => "agent",
            Self::VerifyStarted => "verify_started",
            Self::VerifyFinished => "verify_finished",
            Self::AttemptEnded => "attempt_ended",
            Self::StepFinished => "step_finished",
            Self::Notice => "notice",
            Self::ApprovalRequested => "approval_requested",
            Self::ApprovalDecided => "approval_decided",
            Self::ReviewStarted => "review_started",
            Self::ReviewFinished => "review_finished",
            Self::CouncilStarted => "council_started",
            Self::CouncilFinished => "council_finished",
            Self::ArbitrationStarted => "arbitration_started",
            Self::ArbitrationFinished => "arbitration_finished",
        }
    }
}

/// One observable thing the engine did. Only the fields listed for its kind are set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The task.
    pub task: TaskId,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// What.
    pub kind: EventKind,
    /// The step, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// The attempt id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i64>,
    /// The agent's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// A status, as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// A human-readable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// A normalized runtime event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<openchef_runtime::Event>,
    /// A verification outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Outcome>,
    /// An approval request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
}

impl Event {
    /// An event with only its kind set.
    #[must_use]
    pub fn new(task: &TaskId, at: OffsetDateTime, kind: EventKind) -> Self {
        Self {
            task: task.clone(),
            at,
            kind,
            step: None,
            attempt: None,
            agent: None,
            status: None,
            message: None,
            runtime: None,
            verify: None,
            approval: None,
        }
    }
}

/// Receives engine events. `publish` must not block: a slow viewer must never slow an
/// agent down. Implementations buffer, drop or fan out as they see fit; it may be
/// called from several tasks.
pub trait Sink: Send + Sync {
    /// Delivers one event.
    fn publish(&self, event: Event);
}

/// Drops every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct Discard;

impl Sink for Discard {
    fn publish(&self, _: Event) {}
}

//! What the engine reports while it works, streamed live to `run`, `logs -f` and
//! `status` (D-014).

use openchef_core::TaskId;
use time::OffsetDateTime;

use crate::verify::Outcome;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        }
    }
}

/// One observable thing the engine did. Only the fields listed for its kind are set.
#[derive(Debug, Clone)]
pub struct Event {
    /// The task.
    pub task: TaskId,
    /// When.
    pub at: OffsetDateTime,
    /// What.
    pub kind: EventKind,
    /// The step, from 1.
    pub step: Option<u32>,
    /// The attempt id.
    pub attempt: Option<i64>,
    /// The agent's name.
    pub agent: Option<String>,
    /// A status, as text.
    pub status: Option<String>,
    /// A human-readable message.
    pub message: Option<String>,
    /// A normalized runtime event.
    pub runtime: Option<openchef_runtime::Event>,
    /// A verification outcome.
    pub verify: Option<Outcome>,
}

impl Event {
    pub(crate) fn new(task: &TaskId, at: OffsetDateTime, kind: EventKind) -> Self {
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

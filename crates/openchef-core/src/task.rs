//! Tasks, steps, attempts, sessions and turns (§4).

use std::path::PathBuf;

use time::OffsetDateTime;

use crate::{
    AttemptRole, AttemptStatus, Backend, Error, Importance, Kind, Mode, PermissionMode,
    SessionState, StepStatus, TaskId, TaskStatus, TurnKind, TurnStatus,
};

/// The largest number of steps a plan may have (§4: keeps plans reviewable).
pub const MAX_STEPS: usize = 7;

/// One user request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task's id.
    pub id: TaskId,
    /// How it was started.
    pub kind: Kind,
    /// Where it stands.
    pub status: TaskStatus,
    /// The user's request as typed.
    pub prompt: String,
    /// The absolute path of the user's repository.
    pub repo_root: PathBuf,
    /// The commit the task's branch was created from, once known.
    pub base_commit: Option<String>,
    /// The workspace mode.
    pub mode: Mode,
    /// Where agents and verification run.
    pub backend: Backend,
    /// The user's floor for every step (§7.4).
    pub importance: Importance,
    /// If set, disables routing for this task (§7.3).
    pub agent_pin: Option<String>,
    /// The task's permission mode (§9.1).
    pub permissions: PermissionMode,
    /// The task branch, once the workspace exists.
    pub branch: Option<String>,
    /// The task worktree, once the workspace exists.
    pub worktree: Option<PathBuf>,
    /// When a `waiting_quota` task is rescheduled.
    pub resume_at: Option<OffsetDateTime>,
    /// Why the task needs attention, failed or was cancelled.
    pub reason: Option<String>,
    /// When the task was created.
    pub created_at: OffsetDateTime,
    /// When the task last changed.
    pub updated_at: OffsetDateTime,
}

impl Task {
    /// Checks the fields a new task must have. Vocabulary fields are enums and cannot be
    /// invalid; this checks the free-form ones.
    pub fn validate(&self) -> Result<(), Error> {
        if self.prompt.trim().is_empty() {
            return Err(Error::Invalid(format!("task {}: empty prompt", self.id)));
        }
        if self.repo_root.as_os_str().is_empty() {
            return Err(Error::Invalid(format!(
                "task {}: empty repository root",
                self.id
            )));
        }
        Ok(())
    }
}

/// One unit of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The task the step belongs to.
    pub task_id: TaskId,
    /// Position in the plan, starting at 1.
    pub index: u32,
    /// Short title.
    pub title: String,
    /// What the agent is asked to do.
    pub prompt: String,
    /// Optional scope of paths or globs the step may touch.
    pub files: Vec<String>,
    /// The verification command; `None` accepts the step on completion.
    pub verify: Option<String>,
    /// How much a mistake here would hurt.
    pub importance: Importance,
    /// Where the step stands.
    pub status: StepStatus,
    /// Verification failures fed back to the agent so far.
    pub retries: u32,
    /// The workspace commit that recorded this step.
    pub verified_commit: Option<String>,
    /// When the step last changed.
    pub updated_at: OffsetDateTime,
}

/// Checks a task's steps: 1 to [`MAX_STEPS`] steps, numbered 1..=n in order, each with a
/// title and a prompt.
pub fn validate_plan(steps: &[Step]) -> Result<(), Error> {
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(Error::Invalid(format!(
            "plan has {} steps; want 1..={MAX_STEPS}",
            steps.len()
        )));
    }
    for (position, step) in (1_u32..).zip(steps) {
        if step.index != position {
            return Err(Error::Invalid(format!(
                "step {position} has index {}",
                step.index
            )));
        }
        if step.title.trim().is_empty() {
            return Err(Error::Invalid(format!("step {position}: empty title")));
        }
        if step.prompt.trim().is_empty() {
            return Err(Error::Invalid(format!("step {position}: empty prompt")));
        }
    }
    Ok(())
}

/// One agent working on one step. A failover starts a new attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Store id.
    pub id: i64,
    /// The task.
    pub task_id: TaskId,
    /// The step's index; 0 for task-level roles.
    pub step_index: u32,
    /// What the agent does.
    pub role: AttemptRole,
    /// The routed agent's name.
    pub agent: String,
    /// The agent's runtime (`claude`, `codex`, `agy`).
    pub runtime: String,
    /// The model passed to the runtime.
    pub model: String,
    /// The pool whose quota the attempt spends.
    pub pool: String,
    /// Where the attempt stands.
    pub status: AttemptStatus,
    /// Why the attempt ended (e.g. `quota_exhausted`, `verify_failed`).
    pub outcome: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it ended.
    pub ended_at: Option<OffsetDateTime>,
}

/// One agent CLI process and the conversation it hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Store id.
    pub id: i64,
    /// The attempt the session serves.
    pub attempt_id: i64,
    /// The closed session whose conversation this one continues after a daemon restart
    /// (§5.3).
    pub resumed_from: Option<i64>,
    /// The runtime that runs it.
    pub runtime: String,
    /// The CLI's own id (Claude session id, Codex thread id, ACP session id), once
    /// reported.
    pub native_id: Option<String>,
    /// The process id. A session is recorded once its process exists (§5.1, D-019).
    pub pid: u32,
    /// The process start token; with `pid` it identifies the process exactly, so a later
    /// daemon never kills a reused PID.
    pub start_token: u64,
    /// Where the session stands.
    pub state: SessionState,
    /// Holds `raw.jsonl` and `events.jsonl` (§11).
    pub log_dir: Option<PathBuf>,
    /// When it was opened.
    pub opened_at: OffsetDateTime,
    /// When it was closed.
    pub closed_at: Option<OffsetDateTime>,
}

/// One message sent to a session and everything until the CLI ends the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// Store id.
    pub id: i64,
    /// The session.
    pub session_id: i64,
    /// Numbers the turns of one session from 1.
    pub seq: u32,
    /// Why the message was sent.
    pub kind: TurnKind,
    /// The message.
    pub message: String,
    /// Where the turn stands.
    pub status: TurnStatus,
    /// The normalized result (e.g. `completed`, `rate_limited`, `crashed`).
    pub outcome: Option<String>,
    /// When it was sent.
    pub started_at: OffsetDateTime,
    /// When it ended.
    pub ended_at: Option<OffsetDateTime>,
}

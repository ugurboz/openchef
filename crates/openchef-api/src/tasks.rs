//! Task methods, notifications and wire types (§12.3), ported from Go `api/tasks.go`.

use openchef_core::{Attempt, Step, Task};
use openchef_engine::Event;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Client, Error};

/// Starts a single-step task.
pub const METHOD_TASK_RUN: &str = "task/run";
/// Lists tasks.
pub const METHOD_TASK_LIST: &str = "task/list";
/// One task's detail.
pub const METHOD_TASK_GET: &str = "task/get";
/// Cancels a task.
pub const METHOD_TASK_CANCEL: &str = "task/cancel";
/// Restarts a task that needs attention.
pub const METHOD_TASK_RETRY: &str = "task/retry";
/// Returns a task's past events, then streams new ones as [`NOTIFY_EVENT`] until
/// [`METHOD_TASK_UNWATCH`] or the connection closes.
pub const METHOD_TASK_WATCH: &str = "task/watch";
/// Ends a watch.
pub const METHOD_TASK_UNWATCH: &str = "task/unwatch";
/// Carries one engine event.
pub const NOTIFY_EVENT: &str = "event/appended";
/// Carries [`WatchEnded`] when the daemon ends a watch.
pub const NOTIFY_WATCH_ENDED: &str = "watch/ended";

/// A request the daemon refuses (bad prompt, not a repository, cancelling a finished
/// task); its message is meant for the user as is.
pub const CODE_TASK_INVALID: i64 = -32010;

/// `task/run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunParams {
    /// What to do.
    pub prompt: String,
    /// The caller's working directory, absolute; the task runs on its repository.
    pub dir: String,
    /// The verification command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Pins an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Overrides the repository's default permission mode for this task.
    #[serde(
        rename = "permissionMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<String>,
    /// Runs inside OpenChef's OS sandbox (§21.3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sandbox: bool,
}

/// Names a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskParams {
    /// The task id.
    pub id: String,
}

/// `task/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListParams {
    /// Only tasks that are not finished.
    #[serde(default)]
    pub active: bool,
    /// At most this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `task/watch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchParams {
    /// The task id.
    pub id: String,
    /// Include the task's past events.
    #[serde(default)]
    pub replay: bool,
}

/// The replayed events; live ones follow as notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchResult {
    /// Past events, oldest first.
    pub events: Vec<Event>,
    /// The task's status in the store when the watch began.
    pub status: String,
    /// That status produces no further events by itself.
    pub finished: bool,
    /// Early events left out of a long replay.
    #[serde(default)]
    pub omitted: usize,
}

/// Tells a client its watch was ended by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEnded {
    /// The task id.
    pub id: String,
    /// Why.
    pub reason: String,
}

fn opt_time(t: Option<OffsetDateTime>) -> Option<String> {
    t.and_then(|t| {
        t.format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
}

fn time_text(t: OffsetDateTime) -> String {
    opt_time(Some(t)).unwrap_or_default()
}

/// The wire form of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWire {
    /// The id.
    pub id: String,
    /// `run` or `pipeline`.
    pub kind: String,
    /// Its status.
    pub status: String,
    /// The request.
    pub prompt: String,
    /// The repository.
    pub repo: String,
    /// Its branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Its worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The commit the branch started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The pinned agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Why it needs attention, failed or was cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Its permission mode (§9.1).
    pub permission_mode: String,
    /// When a `waiting_quota` task resumes (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<String>,
    /// Created (RFC 3339).
    pub created_at: String,
    /// Last changed (RFC 3339).
    pub updated_at: String,
}

impl From<&Task> for TaskWire {
    fn from(t: &Task) -> Self {
        Self {
            id: t.id.to_string(),
            kind: t.kind.to_string(),
            status: t.status.to_string(),
            prompt: t.prompt.clone(),
            repo: t.repo_root.to_string_lossy().into_owned(),
            branch: t.branch.clone(),
            worktree: t
                .worktree
                .as_ref()
                .map(|w| w.to_string_lossy().into_owned()),
            base_commit: t.base_commit.clone(),
            agent: t.agent_pin.clone(),
            reason: t.reason.clone(),
            permission_mode: t.permissions.to_string(),
            resume_at: opt_time(t.resume_at),
            created_at: time_text(t.created_at),
            updated_at: time_text(t.updated_at),
        }
    }
}

/// The wire form of a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepWire {
    /// From 1.
    pub index: u32,
    /// Its title.
    pub title: String,
    /// Its status.
    pub status: String,
    /// Its verification command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Feedback turns so far.
    pub retries: u32,
    /// The commit that recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl From<&Step> for StepWire {
    fn from(s: &Step) -> Self {
        Self {
            index: s.index,
            title: s.title.clone(),
            status: s.status.to_string(),
            verify: s.verify.clone(),
            retries: s.retries,
            commit: s.verified_commit.clone(),
        }
    }
}

/// The wire form of an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptWire {
    /// Its id.
    pub id: i64,
    /// The step.
    pub step: u32,
    /// The agent.
    pub agent: String,
    /// Its pool.
    pub pool: String,
    /// Its status.
    pub status: String,
    /// How it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Started (RFC 3339).
    pub started_at: String,
    /// Ended (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

impl From<&Attempt> for AttemptWire {
    fn from(a: &Attempt) -> Self {
        Self {
            id: a.id,
            step: a.step_index,
            agent: a.agent.clone(),
            pool: a.pool.clone(),
            status: a.status.to_string(),
            outcome: a.outcome.clone(),
            started_at: time_text(a.started_at),
            ended_at: opt_time(a.ended_at),
        }
    }
}

/// The result of `task/get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDetail {
    /// The task.
    pub task: TaskWire,
    /// Its steps.
    pub steps: Vec<StepWire>,
    /// Its attempts.
    pub attempts: Vec<AttemptWire>,
    /// An engine run is in progress.
    pub driving: bool,
}

/// Whether a task in `status` no longer produces events by itself: it ended or waits
/// for the user. A watch of a `waiting_quota` task also stops here; the task wakes
/// later and can be watched again.
#[must_use]
pub fn finished(status: &str) -> bool {
    !matches!(status, "running" | "reviewing" | "planning")
}

impl Client {
    /// Starts a task.
    ///
    /// # Errors
    ///
    /// When the call fails; a refusal carries [`CODE_TASK_INVALID`].
    pub async fn run(&self, p: &RunParams) -> Result<TaskWire, Error> {
        self.call(METHOD_TASK_RUN, p).await
    }

    /// Tasks, newest first.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn list(&self, p: &ListParams) -> Result<Vec<TaskWire>, Error> {
        self.call(METHOD_TASK_LIST, p).await
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn get(&self, id: &str) -> Result<TaskDetail, Error> {
        self.call(METHOD_TASK_GET, TaskParams { id: id.to_owned() })
            .await
    }

    /// Cancels a task.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn cancel(&self, id: &str) -> Result<TaskWire, Error> {
        self.call(METHOD_TASK_CANCEL, TaskParams { id: id.to_owned() })
            .await
    }

    /// Restarts a task that needs attention or waits for quota.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn retry(&self, id: &str) -> Result<TaskWire, Error> {
        self.call(METHOD_TASK_RETRY, TaskParams { id: id.to_owned() })
            .await
    }

    /// Starts watching a task; live events arrive as notifications.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn watch(&self, p: &WatchParams) -> Result<WatchResult, Error> {
        self.call(METHOD_TASK_WATCH, p).await
    }

    /// Ends a watch.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn unwatch(&self, id: &str) -> Result<(), Error> {
        self.call::<serde_json::Value>(METHOD_TASK_UNWATCH, TaskParams { id: id.to_owned() })
            .await
            .map(drop)
    }
}

/// The user-facing message of a refusal, or the error as it is.
#[must_use]
pub fn friendly(e: &Error) -> String {
    if let Error::Rpc(openchef_runtime::linerpc::Error::Rpc { error, .. }) = e
        && (error.code == CODE_TASK_INVALID || error.code == crate::CODE_APPROVAL_SETTLED)
    {
        return error.message.clone();
    }
    e.to_string()
}

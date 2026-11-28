//! Configuration, errors and the life of one `run` call.
//!
//! Every state change goes through the store before the side effect it authorizes
//! (write-ahead rule, §11). A store conflict on the task means someone else (a CLI
//! cancel) moved it out of the active states; the engine then stops without touching
//! it. A shutdown stops the engine without changing any state, so recovery can resume
//! exactly where it stopped (§5.3).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openchef_core::{Step, Task, TaskId, TaskStatus};
use openchef_gitws::{Repo, Worktree};
use openchef_runtime::{BoxFuture, SessionSpec};
use openchef_store::{Store, TaskChange};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::events::{Discard, Event, EventKind, Sink};
use crate::router::{Agent, Router};
use crate::runtimes::Runtimes;

/// Bounds the work on one step (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How many times a step's failure (verification, loop, stall) is fed back to the
    /// agent before the task needs attention.
    pub max_retries: u32,
    /// Bounds one turn.
    pub turn_timeout: Duration,
    /// Bounds the time without any event in a turn.
    pub idle_timeout: Duration,
    /// Identical consecutive tool calls that count as a loop.
    pub loop_repeats: u32,
    /// Backoffs on `rate_limited` per attempt.
    pub rate_limit_retries: u32,
    /// Caps one rate-limit wait.
    pub max_backoff: Duration,
    /// Bounds one verification run.
    pub verify_timeout: Duration,
    /// Bounds waiting for a turn to end after an interrupt.
    pub stop_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_retries: 2,
            turn_timeout: Duration::from_mins(30),
            idle_timeout: Duration::from_mins(10),
            loop_repeats: 5,
            rate_limit_retries: 3,
            max_backoff: Duration::from_mins(5),
            verify_timeout: Duration::from_mins(10),
            stop_wait: Duration::from_secs(30),
        }
    }
}

/// Returns the session settings for an agent working on a task in a role: permission
/// mode and rules, env, approver, `ask_user`. Every role but `implement` reads only and
/// gets `plan` mode (§7.5). The engine sets dir, model, effort, log path and `on_spawn`
/// itself.
pub type SpecFor = Arc<
    dyn Fn(&Task, &Agent, openchef_core::AttemptRole) -> Result<SessionSpec, String> + Send + Sync,
>;

/// A sandboxed task's verification sandbox and the environment it adds.
pub type ConfineVerify = Arc<
    dyn Fn(&Task) -> Result<(openchef_procs::Confine, Vec<(OsString, OsString)>), String>
        + Send
        + Sync,
>;

/// Learns that a pool ran out of quota until a time (`None`: unknown), so routing can
/// cool it down (§10.3).
pub type PoolExhausted = Arc<dyn Fn(&str, Option<OffsetDateTime>) + Send + Sync>;
/// Waits; tests replace it.
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;
/// The clock.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Wires the engine to the rest of the daemon.
#[derive(Clone)]
pub struct Config {
    /// The state database.
    pub store: Store,
    /// The runtimes by name.
    pub runtimes: Runtimes,
    /// Agent selection.
    pub router: Arc<dyn Router>,
    /// Session settings per agent and task.
    pub spec_for: SpecFor,
    /// The environment of verification commands (see [`crate::verify::minimal_env`]).
    pub verify_env: Vec<(OsString, OsString)>,
    /// The sandbox for a sandboxed task's verification commands, with the variables
    /// it adds (private `TMPDIR`, redirected caches); `None` runs them unconfined.
    pub confine_verify: Option<ConfineVerify>,
    /// `~/.openchef/worktrees`.
    pub worktree_root: PathBuf,
    /// `~/.openchef/sessions`.
    pub session_root: PathBuf,
    /// Limits.
    pub limits: Limits,
    /// Quota cooldown feedback.
    pub pool_exhausted: Option<PoolExhausted>,
    /// Live events.
    pub sink: Arc<dyn Sink>,
    /// Waits (default: a Tokio timer).
    pub sleep: Option<Sleep>,
    /// The clock (default: the system time).
    pub now: Option<Clock>,
}

impl Config {
    /// A configuration with default limits, no live events and the system clock.
    #[must_use]
    pub fn new(
        store: Store,
        runtimes: Runtimes,
        router: Arc<dyn Router>,
        spec_for: SpecFor,
        home: &std::path::Path,
    ) -> Self {
        Self {
            store,
            runtimes,
            router,
            spec_for,
            verify_env: Vec::new(),
            confine_verify: None,
            worktree_root: home.join("worktrees"),
            session_root: home.join("sessions"),
            limits: Limits::default(),
            pool_exhausted: None,
            sink: Arc::new(Discard),
            sleep: None,
            now: None,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("runtimes", &self.runtimes.keys().collect::<Vec<_>>())
            .field("worktree_root", &self.worktree_root)
            .field("session_root", &self.session_root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Why the engine stopped without finishing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The task is not (or no longer) running: run on a parked or finished task, or a
    /// cancel moved it under the engine.
    #[error("engine: task {task} is {status}")]
    NotActive {
        /// The task.
        task: TaskId,
        /// Its status.
        status: TaskStatus,
    },
    /// The daemon is shutting down; the task stays as it is for recovery.
    #[error("engine: shutting down")]
    Shutdown,
    /// The configuration cannot drive tasks.
    #[error("engine: {0}")]
    Config(String),
    /// A store operation failed.
    #[error("engine: {0}")]
    Store(#[from] openchef_store::Error),
    /// A git operation failed.
    #[error("engine: {0}")]
    Git(#[from] openchef_gitws::Error),
    /// Verification could not run.
    #[error("engine: {0}")]
    Verify(#[from] crate::verify::Error),
    /// A session could not be used.
    #[error("engine: {0}")]
    Session(String),
}

impl Error {
    /// Whether the task is no longer the engine's to drive.
    pub(crate) fn stopped(&self) -> bool {
        matches!(
            self,
            Self::NotActive { .. }
                | Self::Store(openchef_store::Error::Conflict { entity: "task", .. })
        )
    }
}

/// Drives tasks. It holds no per-task state, so one engine serves every task.
#[derive(Debug, Clone)]
pub struct Engine {
    pub(crate) cfg: Arc<Config>,
}

impl Engine {
    /// Checks `cfg`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when it cannot drive tasks.
    pub fn new(cfg: Config) -> Result<Self, Error> {
        if cfg.runtimes.is_empty() {
            return Err(Error::Config("no runtimes are configured".into()));
        }
        if !cfg.worktree_root.is_absolute() || !cfg.session_root.is_absolute() {
            return Err(Error::Config(
                "worktree and session roots must be absolute".into(),
            ));
        }
        Ok(Self { cfg: Arc::new(cfg) })
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        self.cfg
            .now
            .as_ref()
            .map_or_else(OffsetDateTime::now_utc, |c| c())
    }

    /// Waits `d` unless `shutdown` fires first.
    pub(crate) async fn sleep(
        &self,
        d: Duration,
        shutdown: &CancellationToken,
    ) -> Result<(), Error> {
        let wait: BoxFuture<'static, ()> = match &self.cfg.sleep {
            Some(s) => s(d),
            None => Box::pin(tokio::time::sleep(d)),
        };
        tokio::select! {
            () = wait => Ok(()),
            () = shutdown.cancelled() => Err(Error::Shutdown),
        }
    }

    /// Drives a task until it leaves the active states. `Ok` when it reached done,
    /// `waiting_quota` or `needs_attention`, or someone else moved it; [`Error::Shutdown`]
    /// when `shutdown` fired, leaving it for recovery; other errors after parking the
    /// task in `needs_attention` (or when even that failed).
    ///
    /// # Errors
    ///
    /// See above.
    pub async fn run(&self, id: &TaskId, shutdown: &CancellationToken) -> Result<(), Error> {
        let mut r = Run::load(self, id, shutdown).await?;
        let err = match r.drive().await {
            Ok(()) => return Ok(()),
            Err(e) if e.stopped() => return Ok(()),
            Err(e) => e,
        };
        // A cancel moves the task out of the active states, which makes the engine's next
        // write conflict on a turn, attempt or step. Either way the task is no longer ours.
        if self
            .cfg
            .store
            .task(id)
            .await
            .is_ok_and(|t| !t.status.is_active())
        {
            return Ok(());
        }
        if matches!(err, Error::Shutdown) || shutdown.is_cancelled() {
            return Err(Error::Shutdown);
        }
        // Something unexpected (store, git, verification): park the task so a human sees
        // it, and report the error.
        match r
            .park(TaskStatus::NeedsAttention, &err.to_string(), None)
            .await
        {
            Ok(()) => Err(err),
            Err(e) if e.stopped() => Err(err),
            Err(e) => Err(Error::Session(format!(
                "{err}; parking it also failed: {e}"
            ))),
        }
    }
}

/// The state of one `run` call.
pub(crate) struct Run<'e> {
    pub(crate) e: &'e Engine,
    pub(crate) shutdown: &'e CancellationToken,
    pub(crate) task: Task,
    pub(crate) steps: Vec<Step>,
    pub(crate) wt: Option<Worktree>,
}

impl<'e> Run<'e> {
    async fn load(
        e: &'e Engine,
        id: &TaskId,
        shutdown: &'e CancellationToken,
    ) -> Result<Self, Error> {
        let task = e.cfg.store.task(id).await?;
        if !task.status.is_active() {
            return Err(Error::NotActive {
                task: id.clone(),
                status: task.status,
            });
        }
        let steps = e.cfg.store.steps(id).await?;
        Ok(Self {
            e,
            shutdown,
            task,
            steps,
            wt: None,
        })
    }

    pub(crate) fn store(&self) -> &Store {
        &self.e.cfg.store
    }

    pub(crate) fn event(&self, kind: EventKind) -> Event {
        Event::new(&self.task.id, self.e.now(), kind)
    }

    pub(crate) fn publish(&self, event: Event) {
        self.e.cfg.sink.publish(event);
    }

    pub(crate) fn notice(&self, step: u32, attempt: Option<i64>, message: impl Into<String>) {
        let mut ev = self.event(EventKind::Notice);
        ev.step = Some(step);
        ev.attempt = attempt;
        ev.message = Some(message.into());
        self.publish(ev);
    }

    pub(crate) fn worktree(&self) -> Result<&Worktree, Error> {
        self.wt
            .as_ref()
            .ok_or_else(|| Error::Config("the workspace is not prepared".into()))
    }

    pub(crate) fn base_commit(&self) -> &str {
        self.task.base_commit.as_deref().unwrap_or("")
    }

    async fn drive(&mut self) -> Result<(), Error> {
        if self.task.status == TaskStatus::Planning {
            self.workspace().await?;
            if !self.plan().await? {
                return Ok(());
            }
        }
        if self.task.status != TaskStatus::Running {
            return Err(Error::Config(format!(
                "task {} is {}; only running tasks are driven",
                self.task.id, self.task.status
            )));
        }
        self.workspace().await?;
        for i in 0..self.steps.len() {
            if self.steps[i].status == openchef_core::StepStatus::Verified {
                continue;
            }
            if !self.run_step(i).await? {
                return Ok(());
            }
        }
        // Review and the integration audit (R5.2) will send pipelines through
        // `reviewing`; until then both kinds are done after their last step.
        self.set_status(TaskStatus::Done, "", None).await
    }

    /// Creates (or, after a crash, finds) the task's worktree. The paths are recorded
    /// before the worktree exists, so a crash in between leaves something to clean.
    async fn workspace(&mut self) -> Result<(), Error> {
        let repo = Repo::open(&self.task.repo_root).await?;
        if self.task.worktree.is_none() {
            let base = repo.head().await?;
            let branch = format!("openchef/{}", self.task.id);
            let path = self
                .e
                .cfg
                .worktree_root
                .join(repo.id())
                .join(self.task.id.as_str());
            self.store()
                .set_workspace(&self.task.id, &branch, &path, &base)
                .await?;
            self.task.branch = Some(branch);
            self.task.worktree = Some(path);
            self.task.base_commit = Some(base);
        }
        let (Some(path), Some(branch)) = (&self.task.worktree, &self.task.branch) else {
            return Err(Error::Config(
                "the task's workspace is half recorded".into(),
            ));
        };
        self.wt = Some(
            repo.create_worktree(path, branch, self.base_commit())
                .await?,
        );
        Ok(())
    }

    /// The commit the workspace returns to when work on step `i` is discarded.
    pub(crate) fn last_verified(&self, i: usize) -> String {
        self.steps[..i]
            .iter()
            .rev()
            .find_map(|s| s.verified_commit.clone())
            .unwrap_or_else(|| self.base_commit().to_owned())
    }

    /// Re-reads the task right before a side effect the store cannot guard (a git
    /// commit or reset).
    pub(crate) async fn ensure_active(&self) -> Result<(), Error> {
        let t = self.store().task(&self.task.id).await?;
        if t.status.is_active() {
            Ok(())
        } else {
            Err(Error::NotActive {
                task: t.id,
                status: t.status,
            })
        }
    }

    pub(crate) async fn set_status(
        &mut self,
        to: TaskStatus,
        reason: &str,
        resume_at: Option<OffsetDateTime>,
    ) -> Result<(), Error> {
        let change = TaskChange {
            reason: (!reason.is_empty()).then(|| reason.to_owned()),
            resume_at,
        };
        self.task = self
            .store()
            .transition_task(&self.task.id, self.task.status, to, change)
            .await?;
        let mut ev = self.event(EventKind::TaskStatus);
        ev.status = Some(to.to_string());
        ev.message = (!reason.is_empty()).then(|| reason.to_owned());
        self.publish(ev);
        Ok(())
    }

    /// Moves the task to a waiting state; the run ends.
    pub(crate) async fn park(
        &mut self,
        to: TaskStatus,
        reason: &str,
        resume_at: Option<OffsetDateTime>,
    ) -> Result<(), Error> {
        self.set_status(to, reason, resume_at).await
    }
}

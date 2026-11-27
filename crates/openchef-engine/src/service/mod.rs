//! OpenChef's application layer, ported from Go `internal/service`: it turns user
//! requests (run, cancel, retry, watch) into store transitions and engine runs, resumes
//! work after a daemon restart, and wakes tasks parked in `waiting_quota` when their
//! quota resets. The daemon's API handlers and the TUI call it; it knows nothing about
//! sockets or terminals.

mod approvals;
mod events;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub use approvals::{AskRequest, Notify};
pub use events::{EventLog, OnFail, SUBSCRIBER_BUFFER, Subscription};
use openchef_core::StateMachine as _;
use openchef_core::{
    Approval, Attempt, Backend, Importance, Kind, Mode, PermissionMode, Step, StepStatus, Task,
    TaskId, TaskStatus,
};
use openchef_gitws::Repo;
use openchef_store::{Store, TaskChange, TaskFilter};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::engine::{Clock, Config, Engine};
use crate::events::{Event, EventKind, Sink};

/// Returns a repository's default permission mode (its settings' `defaultMode`).
pub type DefaultPermissions = Arc<dyn Fn(&Path) -> Result<PermissionMode, String> + Send + Sync>;

/// Why a service request failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request cannot be carried out as given.
    #[error("{0}")]
    Invalid(String),
    /// A store operation failed.
    #[error("service: {0}")]
    Store(#[from] openchef_store::Error),
    /// The engine could not be built.
    #[error("service: {0}")]
    Engine(#[from] crate::engine::Error),
    /// The event log could not be read.
    #[error("service: event log: {0}")]
    EventLog(String),
    /// `stop` returned while engine runs were still stopping.
    #[error("service: engine runs did not stop in time")]
    StopTimeout,
    /// A request was already settled (by the user, a timeout or the daemon) when
    /// another decision arrived.
    #[error("request {id} was already {status}")]
    Settled {
        /// The request.
        id: i64,
        /// How it was settled.
        status: openchef_core::ApprovalStatus,
    },
    /// The agent stopped waiting; the request was withdrawn.
    #[error("service: request {0} withdrawn")]
    Withdrawn(i64),
    /// The service is stopping; the request stays pending for the next start to expire.
    #[error("service: stopping")]
    Stopping,
}

/// What the service needs.
pub struct Deps {
    /// The engine's configuration; its sink is replaced by the service's event log.
    pub engine: Config,
    /// `~/.openchef/sessions`; each task's `events.jsonl` lives below it.
    pub event_dir: PathBuf,
    /// A repository's default permission mode; `None` means manual.
    pub default_permissions: Option<DefaultPermissions>,
    /// Operational problems (event log writes); `None` drops them.
    pub on_fail: Option<OnFail>,
    /// How long a request waits for the user; `None` is `APPROVAL_TIMEOUT` (§9.2).
    pub approval_timeout: Option<Duration>,
    /// Tells the user a request waits while nobody watches its task.
    pub notify: Option<Notify>,
    /// Sees every engine event before the event log (quota tracking).
    pub tap: Option<Arc<dyn Sink>>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("engine", &self.engine)
            .field("event_dir", &self.event_dir)
            .finish_non_exhaustive()
    }
}

/// A request to start a single-step task (`openchef run`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunRequest {
    /// What to do.
    pub prompt: String,
    /// Any directory inside the user's repository.
    pub dir: PathBuf,
    /// The verification command; `None` accepts the step on completion.
    pub verify: Option<String>,
    /// Pins an agent; `None` lets the router choose.
    pub agent: Option<String>,
    /// Defaults to normal.
    pub importance: Option<Importance>,
    /// The task's permission mode; `None` uses the repository's default.
    /// `bypassPermissions` is accepted only here, asked for per task, never as a default
    /// (§9.3).
    pub permissions: Option<PermissionMode>,
}

/// Everything known about one task.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// The task.
    pub task: Task,
    /// Its steps.
    pub steps: Vec<Step>,
    /// Its attempts.
    pub attempts: Vec<Attempt>,
    /// An engine run is in progress in this daemon.
    pub driving: bool,
}

#[derive(Default)]
struct State {
    running: HashMap<TaskId, CancellationToken>,
    /// Wake-ups of `waiting_quota` tasks: a generation, so a replaced timer is ignored.
    timers: HashMap<TaskId, (u64, CancellationToken)>,
    next_timer: u64,
    stopped: bool,
    /// Who waits for which approval request; a decision is sent once.
    waiting: HashMap<i64, tokio::sync::oneshot::Sender<Approval>>,
}

struct Inner {
    store: Store,
    engine: Engine,
    events: Arc<EventLog>,
    now: Option<Clock>,
    default_permissions: Option<DefaultPermissions>,
    approval_timeout: Duration,
    notify: Option<Notify>,
    /// Fires when the service stops.
    life: CancellationToken,
    tracker: TaskTracker,
    /// A small leaf value, never held across `.await` (D-030).
    state: Mutex<State>,
}

/// Runs tasks. Cheap to clone; methods are safe for concurrent use.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

/// The first line of `prompt`, cut to 72 characters (a commit subject).
#[must_use]
pub fn title(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 72 {
        return line.to_owned();
    }
    let cut: String = line.chars().take(71).collect();
    format!("{}…", cut.trim_end())
}

/// Delivers every event to a tap, then to the event log.
struct Tee(Arc<dyn Sink>, Arc<EventLog>);

impl Sink for Tee {
    fn publish(&self, event: Event) {
        self.0.publish(event.clone());
        self.1.publish(event);
    }
}

impl Service {
    /// Builds a service; call [`Service::start`] to resume earlier work.
    ///
    /// # Errors
    ///
    /// When the engine configuration is unusable.
    pub fn new(deps: Deps) -> Result<Self, Error> {
        let on_fail: OnFail = deps.on_fail.unwrap_or_else(|| Arc::new(|_| {}));
        let events = Arc::new(EventLog::new(deps.event_dir, on_fail));
        let mut cfg = deps.engine;
        cfg.sink = match deps.tap {
            Some(tap) => Arc::new(Tee(tap, events.clone())),
            None => events.clone(),
        };
        let now = cfg.now.clone();
        let store = cfg.store.clone();
        let engine = Engine::new(cfg)?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                engine,
                events,
                now,
                default_permissions: deps.default_permissions,
                approval_timeout: deps
                    .approval_timeout
                    .unwrap_or(openchef_core::APPROVAL_TIMEOUT),
                notify: deps.notify,
                life: CancellationToken::new(),
                tracker: TaskTracker::new(),
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// Recovers after a daemon restart (§5.3): kills processes left by the previous
    /// daemon, resumes running tasks and schedules `waiting_quota` ones. Returns how
    /// many leftover processes were killed.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub async fn start(&self) -> Result<usize, Error> {
        self.expire_stale().await?;
        let killed = self.inner.engine.kill_leftovers().await.unwrap_or(0);
        let active = self
            .inner
            .store
            .tasks(TaskFilter {
                active: true,
                limit: None,
            })
            .await?;
        for t in active {
            match t.status {
                TaskStatus::Running => self.inner.drive(&t.id),
                TaskStatus::WaitingQuota => self.inner.schedule(&t.id, t.resume_at),
                // Planning, awaiting approval, reviewing and needs_attention wait for the
                // user or a later milestone.
                TaskStatus::Planning
                | TaskStatus::AwaitingApproval
                | TaskStatus::Reviewing
                | TaskStatus::NeedsAttention
                | TaskStatus::Done
                | TaskStatus::Failed
                | TaskStatus::Cancelled => {}
            }
        }
        Ok(killed)
    }

    /// Ends every engine run, leaving tasks for recovery on the next start (§5.4), and
    /// waits for them up to `timeout`.
    ///
    /// # Errors
    ///
    /// [`Error::StopTimeout`] when runs were still stopping.
    pub async fn stop(&self, timeout: Duration) -> Result<(), Error> {
        self.inner.life.cancel();
        {
            let mut st = self.inner.state();
            st.stopped = true;
            for (_, (_, t)) in st.timers.drain() {
                t.cancel();
            }
            for t in st.running.values() {
                t.cancel();
            }
        }
        self.inner.tracker.close();
        let waited = tokio::time::timeout(timeout, self.inner.tracker.wait()).await;
        self.inner.events.close();
        waited.map_err(|_| Error::StopTimeout)
    }

    /// Creates a run task and starts it.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty prompt, a directory outside a repository or a
    /// refused permission mode.
    pub async fn run(&self, req: RunRequest) -> Result<Task, Error> {
        let prompt = req.prompt.trim().to_owned();
        if prompt.is_empty() {
            return Err(Error::Invalid("empty prompt".into()));
        }
        let repo = Repo::open(&req.dir)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let permissions = self.inner.permissions(&repo.root, req.permissions)?;
        let importance = req.importance.unwrap_or(Importance::Normal);
        let id = TaskId::generate().map_err(|e| Error::Invalid(e.to_string()))?;
        let now = self.inner.now();
        let task = Task {
            id: id.clone(),
            kind: Kind::Run,
            status: TaskStatus::Running,
            prompt: prompt.clone(),
            repo_root: repo.root.clone(),
            base_commit: None,
            mode: Mode::Isolated,
            backend: Backend::Local,
            importance,
            agent_pin: req.agent.filter(|a| !a.is_empty()),
            permissions,
            branch: None,
            worktree: None,
            resume_at: None,
            reason: None,
            created_at: now,
            updated_at: now,
        };
        let step = Step {
            task_id: id.clone(),
            index: 1,
            title: title(&prompt),
            prompt,
            files: Vec::new(),
            verify: req
                .verify
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
            importance,
            status: StepStatus::Pending,
            retries: 0,
            verified_commit: None,
            updated_at: now,
        };
        let (task, _) = self
            .inner
            .store
            .create_task(task, vec![step])
            .await
            .map_err(|e| {
                if e.is_invalid() {
                    Error::Invalid(e.to_string())
                } else {
                    Error::Store(e)
                }
            })?;
        self.inner.status_event(&id, task.status, "created");
        self.inner.drive(&id);
        Ok(task)
    }

    /// A task's detail.
    ///
    /// # Errors
    ///
    /// When the task does not exist or the store fails.
    pub async fn get(&self, id: &TaskId) -> Result<Detail, Error> {
        let task = self.inner.store.task(id).await?;
        let steps = self.inner.store.steps(id).await?;
        let attempts = self.inner.store.attempts(id).await?;
        let driving = self.inner.state().running.contains_key(id);
        Ok(Detail {
            task,
            steps,
            attempts,
            driving,
        })
    }

    /// Tasks, newest first.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, Error> {
        Ok(self.inner.store.tasks(filter).await?)
    }

    /// Moves a task to cancelled and stops its engine run. The store transition comes
    /// first, so nothing the engine does afterwards is authorized (D-018).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the task already ended.
    pub async fn cancel(&self, id: &TaskId) -> Result<Task, Error> {
        loop {
            let t = self.inner.store.task(id).await?;
            if t.status.is_terminal() {
                return Err(Error::Invalid(format!("task {id} is already {}", t.status)));
            }
            let change = TaskChange {
                reason: Some("cancelled by the user".into()),
                resume_at: None,
            };
            let got = match self
                .inner
                .store
                .transition_task(id, t.status, TaskStatus::Cancelled, change)
                .await
            {
                Ok(t) => t,
                Err(e) if e.is_conflict() => continue, // the engine moved it; cancel from there
                Err(e) => return Err(e.into()),
            };
            let driving = {
                let mut st = self.inner.state();
                if let Some((_, timer)) = st.timers.remove(id) {
                    timer.cancel();
                }
                st.running.get(id).inspect(|t| t.cancel()).is_some()
            };
            self.inner.status_event(
                id,
                TaskStatus::Cancelled,
                got.reason.as_deref().unwrap_or(""),
            );
            if !driving {
                self.inner.events.close_task(id); // a driven task's log closes when its run ends
            }
            return Ok(got);
        }
    }

    /// Restarts a task that needs attention or waits for quota (`openchef retry`).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a task in any other status.
    pub async fn retry(&self, id: &TaskId) -> Result<Task, Error> {
        let t = self.inner.store.task(id).await?;
        if !matches!(
            t.status,
            TaskStatus::NeedsAttention | TaskStatus::WaitingQuota
        ) {
            return Err(Error::Invalid(format!(
                "task {id} is {}; only tasks that need attention or wait for quota can be retried",
                t.status
            )));
        }
        let got = self
            .inner
            .store
            .transition_task(id, t.status, TaskStatus::Running, TaskChange::default())
            .await?;
        if let Some((_, timer)) = self.inner.state().timers.remove(id) {
            timer.cancel();
        }
        self.inner
            .status_event(id, TaskStatus::Running, "retried by the user");
        self.inner.drive(id);
        Ok(got)
    }

    /// The task's past events (with `replay`) and a subscription to new ones. Call
    /// [`Service::unwatch`] when done.
    ///
    /// # Errors
    ///
    /// When the task does not exist or its log cannot be read.
    pub async fn watch(
        &self,
        id: &TaskId,
        replay: bool,
    ) -> Result<(Vec<Event>, Subscription), Error> {
        let _exists = self.inner.store.task(id).await?;
        self.inner
            .events
            .subscribe(id, replay)
            .map_err(Error::EventLog)
    }

    /// Ends a subscription from [`Service::watch`].
    pub fn unwatch(&self, sub: &Subscription) {
        self.inner.events.unsubscribe(sub);
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> OffsetDateTime {
        self.now
            .as_ref()
            .map_or_else(OffsetDateTime::now_utc, |c| c())
    }

    fn status_event(&self, id: &TaskId, status: TaskStatus, message: &str) {
        let mut ev = Event::new(id, self.now(), EventKind::TaskStatus);
        ev.status = Some(status.to_string());
        ev.message = (!message.is_empty()).then(|| message.to_owned());
        self.events.publish(ev);
    }

    fn permissions(
        &self,
        repo: &Path,
        asked: Option<PermissionMode>,
    ) -> Result<PermissionMode, Error> {
        if let Some(mode) = asked {
            return Ok(mode);
        }
        let Some(default) = &self.default_permissions else {
            return Ok(PermissionMode::Manual);
        };
        match default(repo).map_err(Error::Invalid)? {
            PermissionMode::BypassPermissions => Err(Error::Invalid(
                "bypassPermissions cannot be a default; ask for it per task with --permission-mode"
                    .into(),
            )),
            mode @ (PermissionMode::Manual
            | PermissionMode::AcceptEdits
            | PermissionMode::Plan
            | PermissionMode::Auto
            | PermissionMode::DontAsk) => Ok(mode),
        }
    }

    /// Starts an engine run for a task unless one is in progress.
    fn drive(self: &Arc<Self>, id: &TaskId) {
        let token = {
            let mut st = self.state();
            if st.stopped || st.running.contains_key(id) {
                return;
            }
            let token = CancellationToken::new();
            drop(st.running.insert(id.clone(), token.clone()));
            token
        };
        let (inner, id) = (Arc::clone(self), id.clone());
        drop(self.tracker.spawn(async move {
            let result = inner.engine.run(&id, &token).await;
            let stopped = {
                let mut st = inner.state();
                drop(st.running.remove(&id));
                st.stopped
            };
            let ok = match result {
                Ok(()) => true,
                Err(crate::engine::Error::Shutdown) if stopped => return, // recovery resumes it
                Err(e) => {
                    let mut ev = Event::new(&id, inner.now(), EventKind::Notice);
                    ev.message = Some(format!("run failed: {e}"));
                    inner.events.publish(ev);
                    false
                }
            };
            let Ok(t) = inner.store.task(&id).await else {
                return;
            };
            if t.status == TaskStatus::WaitingQuota {
                inner.schedule(&id, t.resume_at);
            } else if t.status.is_terminal() {
                inner.events.close_task(&id);
            } else if t.status == TaskStatus::Running && ok && !stopped {
                // A retry landed while this run was still cleaning up: its drive was a
                // no-op, so run again. A failed run is not restarted here, or a
                // persistent error would spin; the next start recovers it.
                inner.drive(&id);
            }
        }));
    }

    /// Wakes a `waiting_quota` task at its resume time.
    fn schedule(self: &Arc<Self>, id: &TaskId, at: Option<OffsetDateTime>) {
        let wait = at.map_or(Duration::ZERO, |at| {
            Duration::try_from(at - self.now()).unwrap_or(Duration::ZERO)
        });
        let (generation, cancel) = {
            let mut st = self.state();
            if st.stopped {
                return;
            }
            st.next_timer += 1;
            let generation = st.next_timer;
            let cancel = CancellationToken::new();
            if let Some((_, old)) = st.timers.insert(id.clone(), (generation, cancel.clone())) {
                old.cancel();
            }
            (generation, cancel)
        };
        let (inner, id) = (Arc::clone(self), id.clone());
        drop(self.tracker.spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            {
                let mut st = inner.state();
                if st.stopped || st.timers.get(&id).is_none_or(|(g, _)| *g != generation) {
                    return; // replaced, cancelled or shutting down
                }
                drop(st.timers.remove(&id));
            }
            match inner
                .store
                .transition_task(
                    &id,
                    TaskStatus::WaitingQuota,
                    TaskStatus::Running,
                    TaskChange::default(),
                )
                .await
            {
                Ok(_) => {
                    inner.status_event(&id, TaskStatus::Running, "quota reset; resuming");
                    inner.drive(&id);
                }
                Err(e) if e.is_conflict() => {}
                Err(e) => {
                    let mut ev = Event::new(&id, inner.now(), EventKind::Notice);
                    ev.message = Some(format!("wake after quota reset failed: {e}"));
                    inner.events.publish(ev);
                }
            }
        }));
    }
}

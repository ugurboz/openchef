//! Tasks and steps.
//!
//! Every state change is a compare-and-set on the state the caller believes the row is
//! in, checked against the `openchef-core` state machine and committed before the caller
//! performs the side effect it authorizes (write-ahead rule, §11).

use openchef_core::{
    Kind, StateMachine as _, Step, StepStatus, Task, TaskId, TaskStatus, validate_plan,
};
use rusqlite::{OptionalExtension as _, Row, Transaction, params};
use time::OffsetDateTime;

use crate::sql::{
    cas_failed, exec_one, format_time, non_empty, opt_path_text, parse_enum, parse_opt_time,
    parse_time, path_text, terminal_sql,
};
use crate::{Error, Store};

/// Selects tasks for [`Store::tasks`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskFilter {
    /// Only tasks that are not done, failed or cancelled.
    pub active: bool,
    /// At most this many tasks.
    pub limit: Option<u32>,
}

/// Fields a task transition may set alongside its new status.
#[derive(Debug, Clone, Default)]
pub struct TaskChange {
    /// Why the task needs attention, failed or was cancelled; cleared on any other move.
    pub reason: Option<String>,
    /// Required when moving to `waiting_quota`; cleared on any other move.
    pub resume_at: Option<OffsetDateTime>,
}

/// Fields a step transition may set alongside its new status.
#[derive(Debug, Clone, Default)]
pub struct StepChange {
    /// Required when moving to `verified`.
    pub verified_commit: Option<String>,
    /// Counts a verification failure fed back to the agent (`running → running`).
    pub count_retry: bool,
}

const TASK_COLUMNS: &str = "id, kind, status, prompt, repo_root, base_commit, mode, backend, importance, \
     permission_mode, agent_pin, branch, worktree, resume_at, reason, created_at, updated_at";

const STEP_COLUMNS: &str = "task_id, idx, title, prompt, files, verify, importance, status, retries, verified_commit, updated_at";

pub(crate) fn scan_task(r: &Row<'_>) -> Result<Task, Error> {
    let text = |i: usize| -> Result<String, Error> { Ok(r.get::<_, String>(i)?) };
    let opt = |i: usize| -> Result<Option<String>, Error> { Ok(r.get::<_, Option<String>>(i)?) };
    Ok(Task {
        id: parse_enum("tasks.id", &text(0)?)?,
        kind: parse_enum("tasks.kind", &text(1)?)?,
        status: parse_enum("tasks.status", &text(2)?)?,
        prompt: text(3)?,
        repo_root: text(4)?.into(),
        base_commit: opt(5)?,
        mode: parse_enum("tasks.mode", &text(6)?)?,
        backend: parse_enum("tasks.backend", &text(7)?)?,
        importance: parse_enum("tasks.importance", &text(8)?)?,
        permissions: parse_enum("tasks.permission_mode", &text(9)?)?,
        agent_pin: opt(10)?,
        branch: opt(11)?,
        worktree: opt(12)?.map(Into::into),
        resume_at: parse_opt_time("tasks.resume_at", opt(13)?.as_deref())?,
        reason: opt(14)?,
        created_at: parse_time("tasks.created_at", &text(15)?)?,
        updated_at: parse_time("tasks.updated_at", &text(16)?)?,
    })
}

pub(crate) fn scan_step(r: &Row<'_>) -> Result<Step, Error> {
    let files: String = r.get(4)?;
    Ok(Step {
        task_id: parse_enum("steps.task_id", &r.get::<_, String>(0)?)?,
        index: r.get(1)?,
        title: r.get(2)?,
        prompt: r.get(3)?,
        files: serde_json::from_str(&files).map_err(|_| Error::Corrupt {
            column: "steps.files",
            value: files.clone(),
        })?,
        verify: r.get(5)?,
        importance: parse_enum("steps.importance", &r.get::<_, String>(6)?)?,
        status: parse_enum("steps.status", &r.get::<_, String>(7)?)?,
        retries: r.get(8)?,
        verified_commit: r.get(9)?,
        updated_at: parse_time("steps.updated_at", &r.get::<_, String>(10)?)?,
    })
}

fn insert_step(tx: &Transaction<'_>, id: &TaskId, s: &Step, now: &str) -> Result<(), Error> {
    let files = serde_json::to_string(&s.files)
        .map_err(|e| Error::Invalid(format!("step {} files: {e}", s.index)))?;
    let _ = tx.execute(
        "INSERT INTO steps (task_id, idx, title, prompt, files, verify, importance, status, retries, \
         verified_commit, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_str(),
            s.index,
            s.title,
            s.prompt,
            files,
            s.verify.as_deref().and_then(non_empty),
            s.importance.as_str(),
            s.status.as_str(),
            s.retries,
            s.verified_commit.as_deref().and_then(non_empty),
            now
        ],
    )?;
    Ok(())
}

fn read_task(tx: &Transaction<'_>, id: &TaskId) -> Result<Task, Error> {
    tx.query_row(
        &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
        [id.as_str()],
        |r| Ok(scan_task(r)),
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("task {id}")))?
}

/// Fails with `Conflict` unless the task is running or reviewing.
pub(crate) fn require_active(tx: &Transaction<'_>, id: &str) -> Result<(), Error> {
    let got: Option<String> = tx
        .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    let got = got.ok_or_else(|| Error::NotFound(format!("task {id}")))?;
    if parse_enum::<TaskStatus>("tasks.status", &got)?.is_active() {
        Ok(())
    } else {
        Err(Error::Conflict {
            entity: "task",
            key: id.to_owned(),
            want: "running or reviewing".to_owned(),
            got,
        })
    }
}

impl Store {
    /// Inserts a new task and its steps in one transaction and returns them with their
    /// timestamps set. A run task has exactly one step; a pipeline task has none until its
    /// plan is stored.
    pub async fn create_task(
        &self,
        mut task: Task,
        mut steps: Vec<Step>,
    ) -> Result<(Task, Vec<Step>), Error> {
        task.validate()?;
        if task.status != task.kind.initial_status() {
            return Err(Error::Invalid(format!(
                "{} task must start {}, not {}",
                task.kind,
                task.kind.initial_status(),
                task.status
            )));
        }
        match task.kind {
            Kind::Run if steps.len() != 1 => {
                return Err(Error::Invalid(format!(
                    "run task needs exactly one step, got {}",
                    steps.len()
                )));
            }
            Kind::Pipeline if !steps.is_empty() => {
                return Err(Error::Invalid(
                    "pipeline steps come from its plan".to_owned(),
                ));
            }
            Kind::Run | Kind::Pipeline => {}
        }
        if !steps.is_empty() {
            validate_plan(&steps)?;
        }
        let repo_root = path_text("repository root", &task.repo_root)?;
        let worktree = opt_path_text("worktree", task.worktree.as_ref())?;
        let now = self.now();
        task.created_at = now;
        task.updated_at = now;
        for s in &mut steps {
            s.task_id = task.id.clone();
            s.updated_at = now;
        }
        self.write(move |tx| {
            let stamp = format_time(now);
            let _ = tx.execute(
                &format!("INSERT INTO tasks ({TASK_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"),
                params![
                    task.id.as_str(),
                    task.kind.as_str(),
                    task.status.as_str(),
                    task.prompt,
                    repo_root,
                    task.base_commit.as_deref().and_then(non_empty),
                    task.mode.as_str(),
                    task.backend.as_str(),
                    task.importance.as_str(),
                    task.permissions.as_str(),
                    task.agent_pin.as_deref().and_then(non_empty),
                    task.branch.as_deref().and_then(non_empty),
                    worktree,
                    task.resume_at.map(format_time),
                    task.reason.as_deref().and_then(non_empty),
                    stamp,
                    stamp
                ],
            )?;
            for s in &steps {
                insert_step(tx, &task.id, s, &stamp)?;
            }
            Ok((task, steps))
        })
        .await
    }

    /// One task.
    pub async fn task(&self, id: &TaskId) -> Result<Task, Error> {
        let id = id.clone();
        self.read(move |c| {
            c.query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                [id.as_str()],
                |r| Ok(scan_task(r)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("task {id}")))?
        })
        .await
    }

    /// Tasks, newest first.
    pub async fn tasks(&self, filter: TaskFilter) -> Result<Vec<Task>, Error> {
        let mut sql = format!("SELECT {TASK_COLUMNS} FROM tasks");
        if filter.active {
            sql.push_str(" WHERE status NOT IN ");
            sql.push_str(&terminal_sql());
        }
        sql.push_str(" ORDER BY created_at DESC, id");
        if let Some(limit) = filter.limit {
            sql.push_str(" LIMIT ");
            sql.push_str(&limit.to_string());
        }
        self.read(move |c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], |r| Ok(scan_task(r)))?;
            rows.map(|r| r?).collect()
        })
        .await
    }

    /// Moves a task from `from` to `to` and returns it. Moving to done requires every
    /// step verified and no attempt running. Leaving the active states stops the task's
    /// work in the same transaction, so nothing the engine does afterwards is authorized:
    /// running attempts end (cancelled on cancel, interrupted otherwise), turns in flight
    /// end interrupted and running steps go back to pending. Sessions stay recorded until
    /// their processes are gone; the caller stops them and calls `close_session`.
    pub async fn transition_task(
        &self,
        id: &TaskId,
        from: TaskStatus,
        to: TaskStatus,
        mut change: TaskChange,
    ) -> Result<Task, Error> {
        from.check_transition(to)?;
        if to == TaskStatus::WaitingQuota && change.resume_at.is_none() {
            return Err(Error::Invalid(
                "waiting_quota needs a resume time".to_owned(),
            ));
        }
        if to != TaskStatus::WaitingQuota {
            change.resume_at = None;
        }
        if !matches!(
            to,
            TaskStatus::NeedsAttention | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            change.reason = None;
        }
        let id = id.clone();
        let now = format_time(self.now());
        self.write(move |tx| {
            let ok = exec_one(
                tx,
                "UPDATE tasks SET status = ?1, reason = ?2, resume_at = ?3, updated_at = ?4 WHERE id = ?5 AND status = ?6",
                params![
                    to.as_str(),
                    change.reason.as_deref().and_then(non_empty),
                    change.resume_at.map(format_time),
                    now,
                    id.as_str(),
                    from.as_str()
                ],
            )?;
            if !ok {
                return Err(cas_failed(tx, "task", id.as_str(), from.as_str(), "SELECT status FROM tasks WHERE id = ?1", [id.as_str()]));
            }
            if to == TaskStatus::Done {
                require_finished(tx, &id)?;
            }
            if !to.is_active() {
                stop_work(tx, &id, to, &now)?;
            }
            read_task(tx, &id)
        })
        .await
    }

    /// Stores a pipeline task's plan, replacing any earlier version (approval may edit
    /// it). The task must be planning or awaiting approval and every step fresh.
    pub async fn set_plan(&self, id: &TaskId, mut steps: Vec<Step>) -> Result<Vec<Step>, Error> {
        validate_plan(&steps)?;
        if let Some(s) = steps.iter().find(|s| {
            s.status != StepStatus::Pending || s.retries != 0 || s.verified_commit.is_some()
        }) {
            return Err(Error::Invalid(format!(
                "plan of {id}: step {} is not a fresh pending step",
                s.index
            )));
        }
        let now = self.now();
        for s in &mut steps {
            s.task_id = id.clone();
            s.updated_at = now;
        }
        let id = id.clone();
        self.write(move |tx| {
            let row: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind, status FROM tasks WHERE id = ?1",
                    [id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (kind, status) = row.ok_or_else(|| Error::NotFound(format!("task {id}")))?;
            if kind != Kind::Pipeline.as_str() {
                return Err(Error::Invalid(format!(
                    "plan of {id}: {kind} task has no plan"
                )));
            }
            if status != TaskStatus::Planning.as_str()
                && status != TaskStatus::AwaitingApproval.as_str()
            {
                return Err(Error::Conflict {
                    entity: "task",
                    key: id.to_string(),
                    want: "planning or awaiting_approval".to_owned(),
                    got: status,
                });
            }
            let _ = tx.execute("DELETE FROM steps WHERE task_id = ?1", [id.as_str()])?;
            let stamp = format_time(now);
            for s in &steps {
                insert_step(tx, &id, s, &stamp)?;
            }
            Ok(steps)
        })
        .await
    }

    /// Records the branch, worktree and base commit of a task's workspace. It is written
    /// before the worktree is created, so a crash in between leaves a record of the path
    /// to clean up. The task must not be terminal.
    pub async fn set_workspace(
        &self,
        id: &TaskId,
        branch: &str,
        worktree: &std::path::Path,
        base_commit: &str,
    ) -> Result<(), Error> {
        if branch.is_empty() || worktree.as_os_str().is_empty() || base_commit.is_empty() {
            return Err(Error::Invalid(
                "workspace needs branch, worktree and base commit".to_owned(),
            ));
        }
        let (id, branch, base) = (id.clone(), branch.to_owned(), base_commit.to_owned());
        let worktree = path_text("worktree", worktree)?;
        let now = format_time(self.now());
        self.write(move |tx| {
            let ok = exec_one(
                tx,
                &format!(
                    "UPDATE tasks SET branch = ?1, worktree = ?2, base_commit = ?3, updated_at = ?4 \
                     WHERE id = ?5 AND status NOT IN {}",
                    terminal_sql()
                ),
                params![branch, worktree, base, now, id.as_str()],
            )?;
            if ok {
                Ok(())
            } else {
                Err(cas_failed(tx, "task", id.as_str(), "not terminal", "SELECT status FROM tasks WHERE id = ?1", [id.as_str()]))
            }
        })
        .await
    }

    /// A task's steps in order.
    pub async fn steps(&self, id: &TaskId) -> Result<Vec<Step>, Error> {
        let id = id.clone();
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {STEP_COLUMNS} FROM steps WHERE task_id = ?1 ORDER BY idx"
            ))?;
            let rows = stmt.query_map([id.as_str()], |r| Ok(scan_step(r)))?;
            rows.map(|r| r?).collect()
        })
        .await
    }

    /// Moves a step from `from` to `to` and returns it. The task must be active: steps
    /// only change while the engine drives the task.
    pub async fn transition_step(
        &self,
        id: &TaskId,
        index: u32,
        from: StepStatus,
        to: StepStatus,
        change: StepChange,
    ) -> Result<Step, Error> {
        let key = format!("{id}/{index}");
        // `running → running` is not a state change; it is only allowed to count a retry.
        if from != to || !change.count_retry {
            from.check_transition(to)?;
        }
        if change.count_retry && (from != StepStatus::Running || to != StepStatus::Running) {
            return Err(Error::Invalid(format!(
                "step {key}: a retry keeps the step running"
            )));
        }
        let commit = change.verified_commit.filter(|c| !c.is_empty());
        if to == StepStatus::Verified && commit.is_none() {
            return Err(Error::Invalid(format!(
                "step {key}: a verified step needs its commit"
            )));
        }
        if to != StepStatus::Verified && commit.is_some() {
            return Err(Error::Invalid(format!(
                "step {key}: only a verified step has a commit"
            )));
        }
        let retry = u32::from(change.count_retry);
        let id = id.clone();
        let now = format_time(self.now());
        self.write(move |tx| {
            require_active(tx, id.as_str())?;
            let ok = exec_one(
                tx,
                "UPDATE steps SET status = ?1, retries = retries + ?2, verified_commit = COALESCE(?3, verified_commit), \
                 updated_at = ?4 WHERE task_id = ?5 AND idx = ?6 AND status = ?7",
                params![to.as_str(), retry, commit, now, id.as_str(), index, from.as_str()],
            )?;
            if !ok {
                return Err(cas_failed(
                    tx,
                    "step",
                    &key,
                    from.as_str(),
                    "SELECT status FROM steps WHERE task_id = ?1 AND idx = ?2",
                    params![id.as_str(), index],
                ));
            }
            tx.query_row(
                &format!("SELECT {STEP_COLUMNS} FROM steps WHERE task_id = ?1 AND idx = ?2"),
                params![id.as_str(), index],
                |r| Ok(scan_step(r)),
            )?
        })
        .await
    }
}

/// Rejects done while a step is unverified or an attempt still runs.
fn require_finished(tx: &Transaction<'_>, id: &TaskId) -> Result<(), Error> {
    let (steps, unverified, running): (i64, i64, i64) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM steps WHERE task_id = ?1), \
                (SELECT COUNT(*) FROM steps WHERE task_id = ?1 AND status <> 'verified'), \
                (SELECT COUNT(*) FROM attempts WHERE task_id = ?1 AND status = 'running')",
        [id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if steps == 0 || unverified > 0 {
        return Err(Error::Invalid(format!(
            "task {id}: done needs every step verified ({unverified} of {steps} are not)"
        )));
    }
    if running > 0 {
        return Err(Error::Invalid(format!(
            "task {id}: done while an attempt is running"
        )));
    }
    Ok(())
}

/// Ends a task's in-flight work when it leaves the active states.
fn stop_work(tx: &Transaction<'_>, id: &TaskId, to: TaskStatus, now: &str) -> Result<(), Error> {
    let end = if to == TaskStatus::Cancelled {
        "cancelled"
    } else {
        "interrupted"
    };
    let outcome = format!("task {to}");
    let _ = tx.execute(
        "UPDATE turns SET status = 'interrupted', outcome = ?1, ended_at = ?2 WHERE status = 'sent' AND session_id IN \
         (SELECT s.id FROM sessions s JOIN attempts a ON a.id = s.attempt_id WHERE a.task_id = ?3)",
        params![outcome, now, id.as_str()],
    )?;
    let _ = tx.execute(
        "UPDATE attempts SET status = ?1, outcome = ?2, ended_at = ?3 WHERE task_id = ?4 AND status = 'running'",
        params![end, outcome, now, id.as_str()],
    )?;
    let _ = tx.execute(
        "UPDATE steps SET status = 'pending', updated_at = ?1 WHERE task_id = ?2 AND status = 'running'",
        params![now, id.as_str()],
    )?;
    Ok(())
}

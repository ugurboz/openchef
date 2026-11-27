//! Ported from `internal/store/tasks_test.go` and `lifecycle_test.go` (Go reference):
//! the task, step, attempt, session and turn repositories; `approvals_test.go`: the
//! approval queue.
//!
//! Dropped: `TestPermissionModeMigration` (the Go-era migration it covers does not exist;
//! the Rust schema starts with `permission_mode`), the "bad kind" case of
//! `TestSessionsAndTurns` (`TurnKind` is an enum) and `TestTimeFormatSorts` /
//! `TestTerminalListMatchesDomain` (unit tests in `src/sql.rs`).

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use openchef_core::{
    APPROVAL_TIMEOUT, ApprovalKind, ApprovalStatus, AttemptStatus, Backend, DecidedBy, Decision,
    Importance, Kind, Mode, PermissionMode, SessionState, Step, StepStatus, Task, TaskId,
    TaskStatus, TurnKind, TurnStatus,
};
use openchef_store::{
    ApprovalFilter, Error, NewApproval, NewAttempt, NewSession, OpenOptions, StepChange, Store,
    TaskChange, TaskFilter,
};
use time::OffsetDateTime;
use time::macros::datetime;

struct Db {
    store: Store,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Db {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.store
    }
}

/// A store whose clock is deterministic and strictly increasing (one second per call).
async fn open_domain() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let tick = Arc::new(AtomicI64::new(0));
    let start = datetime!(2026-11-25 10:00:00 UTC);
    let clock =
        Arc::new(move || start + time::Duration::seconds(tick.fetch_add(1, Ordering::SeqCst) + 1));
    let store = Store::open_with(
        &dir.path().join("state.db"),
        OpenOptions {
            migrations: None,
            clock: Some(clock),
        },
    )
    .await
    .unwrap();
    Db { store, _dir: dir }
}

fn new_run_task() -> (Task, Vec<Step>) {
    let id = TaskId::generate().unwrap();
    let epoch = OffsetDateTime::UNIX_EPOCH;
    let task = Task {
        id: id.clone(),
        kind: Kind::Run,
        status: TaskStatus::Running,
        prompt: "add rate limiting".to_owned(),
        repo_root: PathBuf::from("/repo"),
        base_commit: None,
        mode: Mode::Isolated,
        backend: Backend::Local,
        importance: Importance::Normal,
        agent_pin: None,
        permissions: PermissionMode::AcceptEdits,
        branch: None,
        worktree: None,
        resume_at: None,
        reason: None,
        created_at: epoch,
        updated_at: epoch,
    };
    let step = Step {
        task_id: id,
        index: 1,
        title: "add rate limiting".to_owned(),
        prompt: "add rate limiting".to_owned(),
        files: vec!["api/**".to_owned()],
        verify: Some("cargo test".to_owned()),
        importance: Importance::Normal,
        status: StepStatus::Pending,
        retries: 0,
        verified_commit: None,
        updated_at: epoch,
    };
    (task, vec![step])
}

async fn create_run(db: &Store) -> Task {
    let (task, steps) = new_run_task();
    db.create_task(task, steps).await.unwrap().0
}

fn plan_step(index: u32) -> Step {
    Step {
        index,
        title: "t".into(),
        prompt: "p".into(),
        ..new_run_task().1.remove(0)
    }
}

/// Drives a fresh run task to the point where an agent session can open.
async fn start_attempt(db: &Store) -> (Task, openchef_core::Attempt) {
    let task = create_run(db).await;
    let _ = db
        .transition_step(
            &task.id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap();
    let a = db
        .start_attempt(NewAttempt {
            task_id: task.id.clone(),
            step_index: 1,
            agent: "opus".to_owned(),
            runtime: "claude".to_owned(),
            model: "claude-opus-5-5".to_owned(),
            pool: "claude-max".to_owned(),
        })
        .await
        .unwrap();
    (task, a)
}

fn session(attempt: i64, runtime: &str, pid: u32) -> NewSession {
    NewSession {
        attempt_id: attempt,
        resumed_from: None,
        runtime: runtime.to_owned(),
        native_id: None,
        pid,
        start_token: 7,
        log_dir: None,
    }
}

/// Records and opens a session for a running attempt.
async fn open_session(db: &Store, a: &openchef_core::Attempt) -> openchef_core::Session {
    let s = db
        .record_session(session(a.id, &a.runtime, 4242))
        .await
        .unwrap();
    db.open_session(s.id, "n1").await.unwrap();
    s
}

fn change() -> TaskChange {
    TaskChange::default()
}

fn reason(r: &str) -> TaskChange {
    TaskChange {
        reason: Some(r.to_owned()),
        resume_at: None,
    }
}

#[tokio::test]
async fn create_and_read_task() {
    let db = open_domain().await;
    let (mut task, steps) = new_run_task();
    task.agent_pin = Some("opus".to_owned());
    let (created, _) = db.create_task(task, steps).await.unwrap();
    assert_ne!(created.created_at, OffsetDateTime::UNIX_EPOCH);
    assert_eq!(created.created_at, created.updated_at);
    assert_eq!(db.task(&created.id).await.unwrap(), created);
    let steps = db.steps(&created.id).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].files, ["api/**"]);
    assert_eq!(steps[0].verify.as_deref(), Some("cargo test"));
    assert_eq!(steps[0].task_id, created.id);
}

/// A named change that makes a valid new task invalid.
type Mutation = (&'static str, fn(&mut Task, &mut Vec<Step>));

#[tokio::test]
async fn create_task_rejects() {
    let db = open_domain().await;
    let cases: [Mutation; 6] = [
        ("invalid task", |t, _| t.prompt = String::new()),
        ("wrong initial", |t, _| t.status = TaskStatus::Planning),
        ("run without step", |_, s| s.clear()),
        ("run with two steps", |_, s| s.push(s[0].clone())),
        ("bad step", |_, s| s[0].title = String::new()),
        ("pipeline with steps", |t, _| {
            t.kind = Kind::Pipeline;
            t.status = TaskStatus::Planning;
        }),
    ];
    for (name, mutate) in cases {
        let (mut task, mut steps) = new_run_task();
        mutate(&mut task, &mut steps);
        let err = db.create_task(task, steps).await.unwrap_err();
        assert!(err.is_invalid(), "{name}: {err:?}");
    }
    assert!(
        db.tasks(TaskFilter::default()).await.unwrap().is_empty(),
        "rejected creates left rows"
    );
}

#[tokio::test]
async fn create_pipeline_without_steps() {
    let db = open_domain().await;
    let (mut task, _) = new_run_task();
    task.kind = Kind::Pipeline;
    task.status = TaskStatus::Planning;
    let _ = db.create_task(task, Vec::new()).await.unwrap();
}

#[tokio::test]
async fn create_duplicate_id() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let (mut dup, steps) = new_run_task();
    dup.id = task.id.clone();
    assert!(
        db.create_task(dup, steps).await.is_err(),
        "duplicate id accepted"
    );
    assert_eq!(db.steps(&task.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn task_not_found() {
    let db = open_domain().await;
    let missing: TaskId = "0000000000".parse().unwrap();
    assert!(db.task(&missing).await.unwrap_err().is_not_found());
    let err = db
        .transition_task(&missing, TaskStatus::Running, TaskStatus::Failed, change())
        .await
        .unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
}

#[tokio::test]
async fn transition_task() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let resume = datetime!(2026-11-25 15:00:00 UTC);
    let got = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::WaitingQuota,
            TaskChange {
                reason: None,
                resume_at: Some(resume),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.status, TaskStatus::WaitingQuota);
    assert_eq!(got.resume_at, Some(resume));
    assert!(got.updated_at > task.updated_at);
    let got = db
        .transition_task(
            &task.id,
            TaskStatus::WaitingQuota,
            TaskStatus::Running,
            reason("ignored"),
        )
        .await
        .unwrap();
    assert_eq!(
        (got.resume_at, got.reason),
        (None, None),
        "resume_at/reason not cleared"
    );
    let got = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::NeedsAttention,
            reason("retries exhausted"),
        )
        .await
        .unwrap();
    assert_eq!(got.reason.as_deref(), Some("retries exhausted"));
}

#[tokio::test]
async fn transition_task_rejects() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let err = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::Planning,
            change(),
        )
        .await
        .unwrap_err();
    assert!(err.is_invalid_transition(), "forbidden move: {err:?}");
    let err = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::WaitingQuota,
            change(),
        )
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "waiting_quota without resume: {err:?}");
    let err = db
        .transition_task(
            &task.id,
            TaskStatus::NeedsAttention,
            TaskStatus::Running,
            change(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Conflict { got, .. } if got == "running"),
        "stale from: {err:?}"
    );
    let _ = db
        .transition_task(&task.id, TaskStatus::Running, TaskStatus::Failed, change())
        .await
        .unwrap();
    let err = db
        .transition_task(&task.id, TaskStatus::Failed, TaskStatus::Running, change())
        .await
        .unwrap_err();
    assert!(err.is_invalid_transition(), "terminal task moved: {err:?}");
}

/// Exactly one of many concurrent writers wins a compare-and-set; the rest conflict.
#[tokio::test(flavor = "multi_thread")]
async fn transition_task_concurrent() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let targets = [
        TaskStatus::Cancelled,
        TaskStatus::NeedsAttention,
        TaskStatus::Failed,
    ];
    let mut handles = Vec::new();
    for i in 0..16 {
        let (store, id) = (db.store.clone(), task.id.clone());
        handles.push(tokio::spawn(async move {
            store
                .transition_task(
                    &id,
                    TaskStatus::Running,
                    targets[i % targets.len()],
                    TaskChange::default(),
                )
                .await
        }));
    }
    let (mut wins, mut conflicts) = (0, 0);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => wins += 1,
            Err(e) if e.is_conflict() => conflicts += 1,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    assert_eq!((wins, conflicts), (1, 15));
}

#[tokio::test]
async fn tasks_filter() {
    let db = open_domain().await;
    let first = create_run(&db).await;
    let second = create_run(&db).await;
    let third = create_run(&db).await;
    let _ = db
        .transition_task(
            &second.id,
            TaskStatus::Running,
            TaskStatus::Failed,
            change(),
        )
        .await
        .unwrap();
    let ids = |ts: Vec<Task>| ts.into_iter().map(|t| t.id).collect::<Vec<_>>();
    assert_eq!(
        ids(db.tasks(TaskFilter::default()).await.unwrap()),
        [third.id.clone(), second.id.clone(), first.id.clone()]
    );
    assert_eq!(
        ids(db
            .tasks(TaskFilter {
                active: true,
                limit: None
            })
            .await
            .unwrap()),
        [third.id.clone(), first.id.clone()]
    );
    assert_eq!(
        ids(db
            .tasks(TaskFilter {
                active: false,
                limit: Some(1)
            })
            .await
            .unwrap()),
        [third.id]
    );
}

#[tokio::test]
async fn set_workspace() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let wt = std::path::Path::new("/wt");
    assert!(
        db.set_workspace(&task.id, "", wt, "abc")
            .await
            .unwrap_err()
            .is_invalid()
    );
    let branch = format!("openchef/{}", task.id);
    db.set_workspace(&task.id, &branch, wt, "abc123")
        .await
        .unwrap();
    let got = db.task(&task.id).await.unwrap();
    assert_eq!(got.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(got.worktree.as_deref(), Some(wt));
    assert_eq!(got.base_commit.as_deref(), Some("abc123"));
    let _ = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::Cancelled,
            change(),
        )
        .await
        .unwrap();
    assert!(
        db.set_workspace(&task.id, "b", wt, "abc")
            .await
            .unwrap_err()
            .is_conflict(),
        "terminal task"
    );
}

#[tokio::test]
async fn transition_step() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let id = &task.id;
    let s = db
        .transition_step(
            id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap();
    assert_eq!(s.status, StepStatus::Running);
    let retry = || StepChange {
        verified_commit: None,
        count_retry: true,
    };
    let mut s = s;
    for _ in 0..2 {
        s = db
            .transition_step(id, 1, StepStatus::Running, StepStatus::Running, retry())
            .await
            .unwrap();
    }
    assert_eq!(s.retries, 2);
    let commit = |c: &str| StepChange {
        verified_commit: Some(c.to_owned()),
        count_retry: false,
    };
    let err = db
        .transition_step(
            id,
            1,
            StepStatus::Running,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap_err();
    assert!(
        err.is_invalid_transition(),
        "self move without retry: {err:?}"
    );
    let err = db
        .transition_step(
            id,
            1,
            StepStatus::Running,
            StepStatus::Verified,
            StepChange::default(),
        )
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "verified without commit: {err:?}");
    let err = db
        .transition_step(id, 1, StepStatus::Running, StepStatus::Pending, commit("x"))
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "commit on pending: {err:?}");
    let err = db
        .transition_step(
            id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap_err();
    assert!(err.is_conflict(), "stale from: {err:?}");
    let err = db
        .transition_step(
            id,
            2,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap_err();
    assert!(err.is_not_found(), "missing step: {err:?}");
    let s = db
        .transition_step(
            id,
            1,
            StepStatus::Running,
            StepStatus::Verified,
            commit("deadbeef"),
        )
        .await
        .unwrap();
    assert_eq!(
        (s.verified_commit.as_deref(), s.retries),
        (Some("deadbeef"), 2)
    );
}

#[tokio::test]
async fn step_needs_running_task() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let _ = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::NeedsAttention,
            reason("x"),
        )
        .await
        .unwrap();
    let err = db
        .transition_step(
            &task.id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Conflict { entity: "task", .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn retry_only_while_running() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let err = db
        .transition_step(
            &task.id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange {
                verified_commit: None,
                count_retry: true,
            },
        )
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "{err:?}");
}

#[tokio::test]
async fn attempts() {
    let db = open_domain().await;
    let (task, a) = start_attempt(&db).await;
    assert!(a.id > 0 && a.status == AttemptStatus::Running);
    let again = NewAttempt {
        task_id: task.id.clone(),
        step_index: 1,
        agent: "opus".to_owned(),
        runtime: "claude".to_owned(),
        model: String::new(),
        pool: "claude-max".to_owned(),
    };
    assert!(
        db.start_attempt(again.clone())
            .await
            .unwrap_err()
            .is_conflict(),
        "second running attempt"
    );
    assert!(
        db.end_attempt(a.id, AttemptStatus::Running, "")
            .await
            .unwrap_err()
            .is_invalid_transition()
    );
    db.end_attempt(a.id, AttemptStatus::FailedOver, "quota_exhausted")
        .await
        .unwrap();
    assert!(
        db.end_attempt(a.id, AttemptStatus::Failed, "")
            .await
            .unwrap_err()
            .is_conflict(),
        "end twice"
    );
    let next = NewAttempt {
        agent: "gpt".to_owned(),
        runtime: "codex".to_owned(),
        pool: "chatgpt-plus".to_owned(),
        ..again
    };
    let _ = db.start_attempt(next).await.unwrap();
    let all = db.attempts(&task.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].status, AttemptStatus::FailedOver);
    assert_eq!(all[0].outcome.as_deref(), Some("quota_exhausted"));
    assert!(all[0].ended_at.is_some());
    assert_eq!(
        (all[0].model.as_str(), all[1].model.as_str()),
        ("claude-opus-5-5", "")
    );
}

#[tokio::test]
async fn start_attempt_rejects() {
    let db = open_domain().await;
    let task = create_run(&db).await;
    let mut a = NewAttempt {
        task_id: task.id.clone(),
        step_index: 1,
        agent: "opus".to_owned(),
        runtime: "claude".to_owned(),
        model: String::new(),
        pool: "claude-max".to_owned(),
    };
    assert!(
        db.start_attempt(a.clone()).await.unwrap_err().is_conflict(),
        "pending step"
    );
    a.step_index = 5;
    assert!(
        db.start_attempt(a.clone())
            .await
            .unwrap_err()
            .is_not_found(),
        "missing step"
    );
    a.pool = String::new();
    assert!(
        db.start_attempt(a).await.unwrap_err().is_invalid(),
        "no pool"
    );
}

#[tokio::test]
async fn sessions_and_turns() {
    let db = open_domain().await;
    let (_, a) = start_attempt(&db).await;
    let mut new = session(a.id, "claude", 4242);
    new.start_token = u64::MAX;
    new.log_dir = Some(PathBuf::from("/logs/1"));
    let s = db.record_session(new).await.unwrap();
    assert!(
        db.begin_turn(s.id, TurnKind::Prompt, "do it")
            .await
            .unwrap_err()
            .is_conflict(),
        "turn on starting session"
    );
    assert!(db.open_session(s.id, "").await.unwrap_err().is_invalid());
    db.open_session(s.id, "sess-1").await.unwrap();
    assert!(
        db.open_session(s.id, "sess-1")
            .await
            .unwrap_err()
            .is_conflict(),
        "open twice"
    );

    let turn = db
        .begin_turn(s.id, TurnKind::Prompt, "do it")
        .await
        .unwrap();
    assert_eq!((turn.seq, turn.status), (1, TurnStatus::Sent));
    assert!(
        db.begin_turn(s.id, TurnKind::Retry, "retry")
            .await
            .unwrap_err()
            .is_conflict(),
        "second turn in flight"
    );
    db.end_turn(turn.id, TurnStatus::Completed, "completed")
        .await
        .unwrap();
    assert!(
        db.end_turn(turn.id, TurnStatus::Failed, "")
            .await
            .unwrap_err()
            .is_conflict(),
        "end twice"
    );
    let again = db.begin_turn(s.id, TurnKind::Retry, "retry").await.unwrap();
    assert_eq!(again.seq, 2);

    let live = db.live_sessions().await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].start_token, u64::MAX);
    assert_eq!(live[0].native_id.as_deref(), Some("sess-1"));
    assert_eq!(live[0].state, SessionState::Open);
    assert_eq!(
        live[0].log_dir.as_deref(),
        Some(std::path::Path::new("/logs/1"))
    );
    let turns = db.turns(s.id).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].outcome.as_deref(), Some("completed"));
    assert_eq!(
        (turns[1].status, turns[1].kind),
        (TurnStatus::Sent, TurnKind::Retry)
    );

    db.close_session(s.id).await.unwrap();
    db.close_session(s.id).await.unwrap();
    assert!(db.close_session(999).await.unwrap_err().is_not_found());
    assert_eq!(db.live_sessions().await.unwrap(), Vec::new());
    let sessions = db.sessions(a.id).await.unwrap();
    assert!(sessions.len() == 1 && sessions[0].closed_at.is_some());
}

#[tokio::test]
async fn record_session_rejects() {
    let db = open_domain().await;
    let (_, a) = start_attempt(&db).await;
    assert!(
        db.record_session(session(a.id, "claude", 0))
            .await
            .unwrap_err()
            .is_invalid(),
        "no pid"
    );
    assert!(
        db.record_session(session(999, "claude", 1))
            .await
            .unwrap_err()
            .is_not_found(),
        "missing attempt"
    );
    db.end_attempt(a.id, AttemptStatus::Interrupted, "daemon restart")
        .await
        .unwrap();
    assert!(
        db.record_session(session(a.id, "claude", 1))
            .await
            .unwrap_err()
            .is_conflict(),
        "ended attempt"
    );
}

/// One CLI conversation can't have two live sessions: their turns would interleave.
#[tokio::test]
async fn native_id_unique() {
    let db = open_domain().await;
    let (_, a) = start_attempt(&db).await;
    for (i, want_ok) in [(0, true), (1, false)] {
        let s = db
            .record_session(session(a.id, "claude", 100 + i))
            .await
            .unwrap();
        let result = db.open_session(s.id, "same").await;
        assert_eq!(result.is_ok(), want_ok, "open {i}: {result:?}");
        if let Err(e) = result {
            assert!(e.is_conflict());
        }
    }
    let mut dup = session(a.id, "claude", 200);
    dup.native_id = Some("same".to_owned());
    assert!(
        db.record_session(dup).await.unwrap_err().is_conflict(),
        "record live native id"
    );
    let mut other = session(a.id, "codex", 201);
    other.native_id = Some("same".to_owned());
    let _ = db.record_session(other).await.unwrap();
}

#[tokio::test]
async fn cancel_stops_work() {
    let db = open_domain().await;
    let (task, a) = start_attempt(&db).await;
    let s = open_session(&db, &a).await;
    let _ = db.begin_turn(s.id, TurnKind::Prompt, "go").await.unwrap();
    let _ = db
        .transition_task(
            &task.id,
            TaskStatus::Running,
            TaskStatus::Cancelled,
            reason("user"),
        )
        .await
        .unwrap();
    let attempts = db.attempts(&task.id).await.unwrap();
    assert_eq!(attempts[0].status, AttemptStatus::Cancelled);
    assert_eq!(attempts[0].outcome.as_deref(), Some("task cancelled"));
    assert!(
        db.in_flight_turn(s.id).await.unwrap_err().is_not_found(),
        "turn still in flight"
    );
    assert_eq!(
        db.turns(s.id).await.unwrap()[0].status,
        TurnStatus::Interrupted
    );
    assert_eq!(
        db.steps(&task.id).await.unwrap()[0].status,
        StepStatus::Pending
    );
    let live = db.task_sessions(&task.id, true).await.unwrap();
    assert!(live.len() == 1 && live[0].id == s.id);

    // Nothing the engine tries afterwards is authorized.
    assert!(
        db.begin_turn(s.id, TurnKind::Retry, "again")
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert!(
        db.record_session(session(a.id, "claude", 9))
            .await
            .unwrap_err()
            .is_conflict()
    );
    let next = NewAttempt {
        task_id: task.id.clone(),
        step_index: 1,
        agent: "x".to_owned(),
        runtime: "codex".to_owned(),
        model: String::new(),
        pool: "p".to_owned(),
    };
    assert!(db.start_attempt(next).await.unwrap_err().is_conflict());
    db.close_session(s.id).await.unwrap();
    assert_eq!(db.task_sessions(&task.id, true).await.unwrap(), Vec::new());
}

/// A cancel committed by the CLI is never overtaken by the engine: no turn starts after
/// the cancel returns, and nothing is left in flight.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_races_engine() {
    for _ in 0..20 {
        let db = open_domain().await;
        let (task, a) = start_attempt(&db).await;
        let s = open_session(&db, &a).await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let engine = {
            let (store, cancelled) = (db.store.clone(), Arc::clone(&cancelled));
            tokio::spawn(async move {
                loop {
                    let before = cancelled.load(Ordering::SeqCst);
                    match store.begin_turn(s.id, TurnKind::Prompt, "go").await {
                        Ok(turn) => {
                            assert!(!before, "turn began after the cancel was committed");
                            if let Err(e) = store
                                .end_turn(turn.id, TurnStatus::Completed, "completed")
                                .await
                            {
                                assert!(e.is_conflict(), "end_turn: {e:?}");
                            }
                        }
                        Err(e) => {
                            assert!(e.is_conflict(), "begin_turn: {e:?}");
                            return;
                        }
                    }
                }
            })
        };
        let _ = db
            .transition_task(
                &task.id,
                TaskStatus::Running,
                TaskStatus::Cancelled,
                change(),
            )
            .await
            .unwrap();
        cancelled.store(true, Ordering::SeqCst);
        engine.await.unwrap();
        assert!(
            db.in_flight_turn(s.id).await.unwrap_err().is_not_found(),
            "turn left in flight"
        );
    }
}

#[tokio::test]
async fn done_requires_finished_work() {
    let db = open_domain().await;
    let (task, a) = start_attempt(&db).await;
    let err = db
        .transition_task(&task.id, TaskStatus::Running, TaskStatus::Done, change())
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "done with running step: {err:?}");
    assert_eq!(
        db.task(&task.id).await.unwrap().status,
        TaskStatus::Running,
        "rejected done changed the task"
    );
    let s = open_session(&db, &a).await;
    let turn = db.begin_turn(s.id, TurnKind::Prompt, "go").await.unwrap();
    assert!(
        db.end_attempt(a.id, AttemptStatus::Succeeded, "")
            .await
            .unwrap_err()
            .is_invalid(),
        "unverified step"
    );
    let commit = StepChange {
        verified_commit: Some("c1".to_owned()),
        count_retry: false,
    };
    let _ = db
        .transition_step(
            &task.id,
            1,
            StepStatus::Running,
            StepStatus::Verified,
            commit,
        )
        .await
        .unwrap();
    assert!(
        db.end_attempt(a.id, AttemptStatus::Succeeded, "")
            .await
            .unwrap_err()
            .is_invalid(),
        "turn in flight"
    );
    db.end_turn(turn.id, TurnStatus::Completed, "completed")
        .await
        .unwrap();
    let err = db
        .transition_task(&task.id, TaskStatus::Running, TaskStatus::Done, change())
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "done with running attempt: {err:?}");
    db.end_attempt(a.id, AttemptStatus::Succeeded, "verified")
        .await
        .unwrap();
    db.close_session(s.id).await.unwrap();
    let _ = db
        .transition_task(&task.id, TaskStatus::Running, TaskStatus::Done, change())
        .await
        .unwrap();
}

/// The recovery flow of §5.3: the old process is killed and its session closed, the
/// interrupted turn is found, and a new session continues the same conversation.
#[tokio::test]
async fn resume_after_restart() {
    let db = open_domain().await;
    let (task, a) = start_attempt(&db).await;
    let old = open_session(&db, &a).await;
    let turn = db.begin_turn(old.id, TurnKind::Prompt, "go").await.unwrap();
    let running = db.running_attempts().await.unwrap();
    assert!(running.len() == 1 && running[0].id == a.id);
    let resume = |runtime: &str, pid: u32| NewSession {
        resumed_from: Some(old.id),
        start_token: 8,
        ..session(a.id, runtime, pid)
    };
    assert!(
        db.record_session(resume("claude", 5))
            .await
            .unwrap_err()
            .is_conflict(),
        "resume of a live session"
    );
    let in_flight = db.in_flight_turn(old.id).await.unwrap();
    assert_eq!((in_flight.id, in_flight.message.as_str()), (turn.id, "go"));
    db.end_turn(turn.id, TurnStatus::Interrupted, "daemon restart")
        .await
        .unwrap();
    db.close_session(old.id).await.unwrap();

    assert!(
        db.record_session(resume("codex", 5))
            .await
            .unwrap_err()
            .is_invalid(),
        "resume on another runtime"
    );
    let resumed = db.record_session(resume("claude", 6)).await.unwrap();
    assert_eq!(resumed.native_id.as_deref(), Some("n1"));
    assert!(
        db.open_session(resumed.id, "n2")
            .await
            .unwrap_err()
            .is_invalid(),
        "open with another native id"
    );
    db.open_session(resumed.id, "n1").await.unwrap();
    let retry = db
        .begin_turn(
            resumed.id,
            TurnKind::Recovery,
            "The orchestrator restarted…\n\ngo",
        )
        .await
        .unwrap();
    assert_eq!(retry.seq, 1);
    let all = db.task_sessions(&task.id, false).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        (all[0].resumed_from, all[1].resumed_from),
        (None, Some(old.id))
    );
}

/// A session that never reported its native id cannot be resumed.
#[tokio::test]
async fn resume_needs_native_id() {
    let db = open_domain().await;
    let (_, a) = start_attempt(&db).await;
    let s = db.record_session(session(a.id, "claude", 5)).await.unwrap();
    db.close_session(s.id).await.unwrap();
    let err = db
        .record_session(NewSession {
            resumed_from: Some(s.id),
            ..session(a.id, "claude", 6)
        })
        .await
        .unwrap_err();
    assert!(err.is_invalid(), "{err:?}");
}

#[tokio::test]
async fn set_session_process() {
    let db = open_domain().await;
    let (_, a) = start_attempt(&db).await;
    let s = open_session(&db, &a).await;
    assert!(
        db.set_session_process(s.id, 0, 1)
            .await
            .unwrap_err()
            .is_invalid()
    );
    db.set_session_process(s.id, 777, 99).await.unwrap();
    let live = db.live_sessions().await.unwrap();
    assert_eq!((live[0].pid, live[0].start_token), (777, 99));
    db.close_session(s.id).await.unwrap();
    assert!(
        db.set_session_process(s.id, 778, 100)
            .await
            .unwrap_err()
            .is_conflict(),
        "closed session"
    );
}

#[tokio::test]
async fn set_plan() {
    let db = open_domain().await;
    let (mut task, _) = new_run_task();
    task.kind = Kind::Pipeline;
    task.status = TaskStatus::Planning;
    let (task, _) = db.create_task(task, Vec::new()).await.unwrap();
    let _ = db
        .set_plan(&task.id, vec![plan_step(1), plan_step(2)])
        .await
        .unwrap();
    let _ = db
        .transition_task(
            &task.id,
            TaskStatus::Planning,
            TaskStatus::AwaitingApproval,
            change(),
        )
        .await
        .unwrap();
    let mut edited = plan_step(1);
    edited.title = "edited".to_owned();
    let _ = db.set_plan(&task.id, vec![edited]).await.unwrap();
    let steps = db.steps(&task.id).await.unwrap();
    assert!(steps.len() == 1 && steps[0].title == "edited");
    let _ = db
        .transition_task(
            &task.id,
            TaskStatus::AwaitingApproval,
            TaskStatus::Running,
            change(),
        )
        .await
        .unwrap();
    assert!(
        db.set_plan(&task.id, vec![plan_step(1)])
            .await
            .unwrap_err()
            .is_conflict(),
        "while running"
    );
    let mut used = plan_step(1);
    used.retries = 1;
    assert!(
        db.set_plan(&task.id, vec![used])
            .await
            .unwrap_err()
            .is_invalid(),
        "non-fresh step"
    );
    let run = create_run(&db).await;
    assert!(
        db.set_plan(&run.id, vec![plan_step(1)])
            .await
            .unwrap_err()
            .is_invalid(),
        "plan for a run task"
    );
}

#[tokio::test]
async fn delete_task_cascades() {
    let db = open_domain().await;
    let (task, a) = start_attempt(&db).await;
    let s = open_session(&db, &a).await;
    let _ = db.begin_turn(s.id, TurnKind::Prompt, "go").await.unwrap();
    let id = task.id.to_string();
    let _ = db
        .write(move |tx| Ok(tx.execute("DELETE FROM tasks WHERE id = ?1", [id])?))
        .await
        .unwrap();
    for table in ["steps", "attempts", "sessions", "turns"] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let n: i64 = db
            .read(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}

// --- approvals (approvals_test.go) --------------------------------------------------

async fn queue(db: &Store, task: &TaskId, kind: ApprovalKind) -> openchef_core::Approval {
    let question = kind == ApprovalKind::Question;
    db.create_approval(NewApproval {
        task_id: task.clone(),
        agent: "opus".into(),
        kind,
        tool: (!question).then(|| "Bash".to_owned()),
        summary: if question {
            "Which port?"
        } else {
            "go test ./..."
        }
        .into(),
        options: if question {
            vec!["8080".into(), "9090".into()]
        } else {
            Vec::new()
        },
        request: Some(r#"{"command":"go test ./..."}"#.into()),
        expires_at: None,
    })
    .await
    .unwrap()
}

fn decision(status: ApprovalStatus, answer: Option<&str>) -> Decision {
    Decision {
        status,
        answer: answer.map(str::to_owned),
        decided_by: DecidedBy::User,
    }
}

#[tokio::test]
async fn approval_lifecycle() {
    let db = open_domain().await;
    let t = create_run(&db).await;
    let cmd = queue(&db, &t.id, ApprovalKind::Command).await;
    let q = queue(&db, &t.id, ApprovalKind::Question).await;
    assert!(
        cmd.id != 0 && cmd.status == ApprovalStatus::Pending,
        "{cmd:?}"
    );
    assert_eq!(cmd.expires_at - cmd.created_at, APPROVAL_TIMEOUT);
    let pending = db
        .approvals(ApprovalFilter {
            pending: true,
            ..ApprovalFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].id, cmd.id, "oldest first");
    assert_eq!(pending[1].options, q.options);

    let got = db
        .decide_approval(cmd.id, decision(ApprovalStatus::Allowed, None))
        .await
        .unwrap();
    assert!(
        got.status == ApprovalStatus::Allowed && got.decided_at.is_some(),
        "{got:?}"
    );
    // A second answer reports the first one instead of overwriting it.
    match db
        .decide_approval(cmd.id, decision(ApprovalStatus::Denied, None))
        .await
    {
        Err(Error::Conflict { got, .. }) => assert_eq!(got, "allowed"),
        other => panic!("second decision: {other:?}"),
    }
    // Kinds and decisions must match.
    for d in [
        decision(ApprovalStatus::Allowed, None),
        decision(ApprovalStatus::Answered, None),
        decision(ApprovalStatus::Pending, None),
    ] {
        let r = db.decide_approval(q.id, d.clone()).await;
        assert!(
            matches!(r, Err(Error::Domain(_))),
            "{d:?} on a question: {r:?}"
        );
    }
    let got = db
        .decide_approval(q.id, decision(ApprovalStatus::Answered, Some("8080")))
        .await
        .unwrap();
    assert_eq!(got.answer.as_deref(), Some("8080"));
    let missing = db
        .decide_approval(999, decision(ApprovalStatus::Denied, None))
        .await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    let newest = db
        .approvals(ApprovalFilter {
            task: Some(t.id.clone()),
            pending: false,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].id, q.id);
}

#[tokio::test]
async fn approval_needs_active_task() {
    let db = open_domain().await;
    let t = create_run(&db).await;
    db.transition_task(
        &t.id,
        TaskStatus::Running,
        TaskStatus::Cancelled,
        reason("test"),
    )
    .await
    .unwrap();
    let r = db
        .create_approval(NewApproval {
            task_id: t.id.clone(),
            agent: "opus".into(),
            kind: ApprovalKind::Command,
            tool: None,
            summary: "ls".into(),
            options: Vec::new(),
            request: None,
            expires_at: None,
        })
        .await;
    assert!(
        matches!(r, Err(Error::Conflict { .. })),
        "approval for a cancelled task: {r:?}"
    );
    let empty = db
        .create_approval(NewApproval {
            task_id: t.id.clone(),
            agent: String::new(),
            kind: ApprovalKind::Command,
            tool: None,
            summary: "ls".into(),
            options: Vec::new(),
            request: None,
            expires_at: None,
        })
        .await;
    assert!(
        matches!(empty, Err(Error::Invalid(_))),
        "no agent: {empty:?}"
    );
}

#[tokio::test]
async fn expire_pending_on_restart() {
    let db = open_domain().await;
    let t = create_run(&db).await;
    let a = queue(&db, &t.id, ApprovalKind::Command).await;
    let b = queue(&db, &t.id, ApprovalKind::Command).await;
    db.decide_approval(b.id, decision(ApprovalStatus::Denied, None))
        .await
        .unwrap();
    let expired = db.expire_pending("the daemon restarted").await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].id, a.id);
    assert_eq!(expired[0].decided_by, Some(DecidedBy::Daemon));
    // Still visible, with the reason.
    let got = db.approval(a.id).await.unwrap();
    assert_eq!(got.status, ApprovalStatus::Expired);
    assert_eq!(got.answer.as_deref(), Some("the daemon restarted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn decide_approval_race() {
    let db = Arc::new(open_domain().await);
    let t = create_run(&db).await;
    let a = queue(&db, &t.id, ApprovalKind::Command).await;
    let mut handles = Vec::new();
    for i in 0..8 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let st = if i % 2 == 1 {
                ApprovalStatus::Denied
            } else {
                ApprovalStatus::Allowed
            };
            db.decide_approval(a.id, decision(st, None)).await
        }));
    }
    let mut wins = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => wins += 1,
            Err(e) => assert!(e.is_conflict(), "{e}"),
        }
    }
    assert_eq!(wins, 1, "{wins} decisions won");
}

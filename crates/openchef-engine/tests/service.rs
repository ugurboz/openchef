//! Ported from `internal/service/{service,events}_test.go`: the service over a real
//! store and git repository with scripted runtimes, and the event log.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openchef_core::{PermissionMode, Task, TaskId, TaskStatus};
use openchef_engine::service::{
    Deps, Error, EventLog, RunRequest, SUBSCRIBER_BUFFER, Service, title,
};
use openchef_engine::{Agent, Config, DynRuntime, Event, EventKind, Ordered, Runtimes, Sink};
use openchef_fake::{FakeRuntime, Turn};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Failure, FailureKind, Question,
    SessionSpec,
};
use openchef_store::Store;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

struct AllowAll;

impl Approver for AllowAll {
    fn approve(
        &self,
        _: ApprovalRequest,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        Box::pin(async { Ok(Decision::allow()) })
    }

    fn answer(
        &self,
        _: Question,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        Box::pin(async { Ok(String::new()) })
    }
}

struct Env {
    dir: tempfile::TempDir,
    repo: PathBuf,
    store: Store,
    claude: FakeRuntime,
    codex: FakeRuntime,
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["config", "user.name", "Test"],
        &["config", "user.email", "t@example.com"],
        &["commit", "--quiet", "--allow-empty", "-m", "initial"],
    ] {
        git(&repo, args);
    }
    let store = Store::open(&dir.path().join("state.db")).await.unwrap();
    Env {
        dir,
        repo,
        store,
        claude: FakeRuntime::new("claude", []),
        codex: FakeRuntime::new("codex", []),
    }
}

impl Env {
    /// A service over the env's store and fakes (a new one per "daemon").
    fn service_with(
        &self,
        default_permissions: Option<openchef_engine::service::DefaultPermissions>,
    ) -> Service {
        let mut runtimes: Runtimes = HashMap::new();
        runtimes.insert(
            "claude".into(),
            Arc::new(self.claude.clone()) as Arc<dyn DynRuntime>,
        );
        runtimes.insert(
            "codex".into(),
            Arc::new(self.codex.clone()) as Arc<dyn DynRuntime>,
        );
        let router = Arc::new(Ordered {
            agents: vec![
                Agent {
                    name: "opus".into(),
                    runtime: "claude".into(),
                    model: None,
                    pool: "claude-max".into(),
                    effort: None,
                },
                Agent {
                    name: "gpt".into(),
                    runtime: "codex".into(),
                    model: None,
                    pool: "chatgpt-plus".into(),
                    effort: None,
                },
            ],
            cooldown: None,
        });
        let spec_for: openchef_engine::SpecFor = Arc::new(|_: &Task, _: &Agent| {
            Ok(SessionSpec {
                dir: PathBuf::new(),
                model: None,
                effort: None,
                mode: PermissionMode::Manual,
                deny_rules: Vec::new(),
                ask_rules: Vec::new(),
                web_access: false,
                env: Vec::new(),
                approver: Arc::new(AllowAll),
                ask_user: None,
                log_path: None,
                on_spawn: None,
            })
        });
        let mut cfg = Config::new(
            self.store.clone(),
            runtimes,
            router,
            spec_for,
            self.dir.path(),
        );
        cfg.sleep = Some(Arc::new(|_| Box::pin(async {})));
        Service::new(Deps {
            engine: cfg,
            event_dir: self.dir.path().join("sessions"),
            default_permissions,
            on_fail: None,
        })
        .unwrap()
    }

    fn service(&self) -> Service {
        self.service_with(None)
    }

    async fn wait_status(&self, id: &TaskId, status: TaskStatus) -> Task {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = self.store.task(id).await.unwrap();
            if t.status == status {
                return t;
            }
            assert!(
                Instant::now() < deadline,
                "task {id} is {} ({:?}); want {status}",
                t.status,
                t.reason
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn run(&self, prompt: &str) -> RunRequest {
        RunRequest {
            prompt: prompt.into(),
            dir: self.repo.clone(),
            ..RunRequest::default()
        }
    }
}

async fn stop(s: &Service) {
    s.stop(Duration::from_secs(10)).await.unwrap();
}

async fn wait_for(cond: impl AsyncFn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond().await {
        assert!(Instant::now() < deadline, "condition not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn write_file(dir: &Path) -> Result<(), String> {
    std::fs::write(dir.join("out.txt"), "x\n").map_err(|e| e.to_string())
}

fn done(events: &[Event]) -> bool {
    events
        .iter()
        .any(|e| e.kind == EventKind::TaskStatus && e.status.as_deref() == Some("done"))
}

#[tokio::test(flavor = "multi_thread")]
async fn run_and_watch() {
    let e = env().await;
    let s = e.service();
    e.claude.push([Turn::editing("done", write_file)]);
    let prompt =
        "Add a file named out.txt\n\nwith one line; this second paragraph is not the title.";
    let t = s.run(e.run(prompt)).await.unwrap();
    let (past, mut sub) = s.watch(&t.id, true).await.unwrap();
    let mut seen = past;
    while !done(&seen) {
        match tokio::time::timeout(Duration::from_secs(10), sub.rx.recv())
            .await
            .expect("no done event")
        {
            Some(ev) => seen.push(ev),
            None => panic!("subscription ended (lagged: {})", sub.lagged()),
        }
    }
    s.unwatch(&sub);
    assert!(
        seen[0].kind == EventKind::TaskStatus && seen[0].message.as_deref() == Some("created"),
        "{:?}",
        seen[0]
    );
    assert!(
        seen.iter().all(|ev| ev.task == t.id),
        "event of another task"
    );
    let d = s.get(&t.id).await.unwrap();
    assert!(
        d.task.status == TaskStatus::Done
            && d.steps[0].title == "Add a file named out.txt"
            && d.attempts.len() == 1,
        "{d:?}"
    );
    // A late viewer replays the same history from the log.
    wait_for(async || !s.get(&t.id).await.unwrap().driving).await;
    let (replayed, late) = s.watch(&t.id, true).await.unwrap();
    s.unwatch(&late);
    assert_eq!(
        replayed.len(),
        seen.len(),
        "replay differs from what the live viewer saw"
    );
    stop(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn run_rejects_bad_requests() {
    let e = env().await;
    let s = e.service();
    assert!(
        matches!(s.run(e.run("  ")).await, Err(Error::Invalid(_))),
        "empty prompt"
    );
    let elsewhere = tempfile::tempdir().unwrap();
    let req = RunRequest {
        prompt: "x".into(),
        dir: elsewhere.path().to_path_buf(),
        ..RunRequest::default()
    };
    assert!(
        matches!(s.run(req).await, Err(Error::Invalid(_))),
        "not a repository"
    );
    stop(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_the_run() {
    let e = env().await;
    let s = e.service();
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let t = s.run(e.run("hang")).await.unwrap();
    wait_for(async || e.claude.turns().len() == 1).await;
    let got = s.cancel(&t.id).await.unwrap();
    assert_eq!(got.status, TaskStatus::Cancelled);
    wait_for(async || !s.get(&t.id).await.unwrap().driving).await;
    assert_eq!(e.claude.live(), 0, "agent session still open after cancel");
    assert!(
        matches!(s.cancel(&t.id).await, Err(Error::Invalid(_))),
        "second cancel"
    );
    stop(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_after_attention() {
    let e = env().await;
    let s = e.service();
    e.claude
        .push([Turn::failing(FailureKind::Auth, "token expired")]);
    let t = s.run(e.run("p")).await.unwrap();
    e.wait_status(&t.id, TaskStatus::NeedsAttention).await;
    wait_for(async || !s.get(&t.id).await.unwrap().driving).await;
    e.claude.push([Turn::editing("", write_file)]);
    s.retry(&t.id).await.unwrap();
    e.wait_status(&t.id, TaskStatus::Done).await;
    assert!(
        matches!(s.retry(&t.id).await, Err(Error::Invalid(_))),
        "retry of a done task"
    );
    stop(&s).await;
}

/// A task parked in `waiting_quota` wakes by itself when the quota resets.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_quota_wakes_up() {
    let e = env().await;
    let s = e.service();
    let reset = OffsetDateTime::now_utc() + time::Duration::milliseconds(300);
    let exhausted = || {
        let mut f = Failure::new(FailureKind::QuotaExhausted, "out");
        f.resets_at = Some(reset);
        Turn {
            fail: Some(f),
            ..Turn::default()
        }
    };
    e.claude.push([exhausted()]);
    e.codex.push([exhausted()]);
    e.claude.push([Turn::editing("", write_file)]);
    let t = s.run(e.run("p")).await.unwrap();
    // The wait can be shorter than one poll, so the event log proves the task waited.
    e.wait_status(&t.id, TaskStatus::Done).await;
    let (past, sub) = s.watch(&t.id, true).await.unwrap();
    s.unwatch(&sub);
    let statuses: Vec<String> = past
        .iter()
        .filter(|ev| ev.kind == EventKind::TaskStatus)
        .filter_map(|ev| ev.status.clone())
        .collect();
    assert!(
        statuses.iter().any(|st| st == "waiting_quota"),
        "never waited for quota: {statuses:?}"
    );
    stop(&s).await;
}

/// A daemon restart: the new service kills leftovers and resumes the running task.
#[tokio::test(flavor = "multi_thread")]
async fn start_resumes_after_restart() {
    let e = env().await;
    let first = e.service();
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let t = first.run(e.run("p")).await.unwrap();
    wait_for(async || e.claude.turns().len() == 1).await;
    stop(&first).await;
    assert_eq!(
        e.store.task(&t.id).await.unwrap().status,
        TaskStatus::Running,
        "shutdown changed the task"
    );
    e.claude.push([Turn::editing("", write_file)]);
    let second = e.service();
    second.start().await.unwrap();
    e.wait_status(&t.id, TaskStatus::Done).await;
    let turns = e.claude.turns();
    assert!(
        turns
            .last()
            .unwrap()
            .starts_with("The orchestrator restarted"),
        "{turns:?}"
    );
    stop(&second).await;
}

#[test]
fn titles() {
    let long = "ş".repeat(100);
    let got = title(&long);
    assert!(got.chars().count() == 72 && got.ends_with('…'), "{got}");
    assert_eq!(title("  short  \nmore"), "short");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_permission_mode() {
    let e = env().await;
    let default = Arc::new(std::sync::Mutex::new(PermissionMode::AcceptEdits));
    let s = e.service_with(Some({
        let default = Arc::clone(&default);
        Arc::new(move |_: &Path| Ok(*default.lock().unwrap()))
    }));
    e.claude
        .push([Turn::editing("", write_file), Turn::editing("", write_file)]);
    let t = s.run(e.run("p")).await.unwrap();
    assert_eq!(t.permissions, PermissionMode::AcceptEdits, "default");
    e.wait_status(&t.id, TaskStatus::Done).await;
    let bypass = RunRequest {
        permissions: Some(PermissionMode::BypassPermissions),
        ..e.run("p")
    };
    let t = s.run(bypass).await.unwrap();
    assert_eq!(
        t.permissions,
        PermissionMode::BypassPermissions,
        "explicit bypass"
    );
    e.wait_status(&t.id, TaskStatus::Done).await;
    *default.lock().unwrap() = PermissionMode::BypassPermissions;
    assert!(
        matches!(s.run(e.run("p")).await, Err(Error::Invalid(_))),
        "bypass as a default"
    );
    stop(&s).await;
}

// --- the event log (events_test.go) ----------------------------------------------------

fn notice(task: &TaskId, message: &str, step: u32) -> Event {
    let mut ev = Event::new(task, OffsetDateTime::now_utc(), EventKind::Notice);
    ev.message = Some(message.into());
    ev.step = Some(step);
    ev
}

/// A crash can leave the last line without its newline; the next daemon's first event
/// starts its own line and is still replayed.
#[test]
fn event_log_after_cut_short_line() {
    let dir = tempfile::tempdir().unwrap();
    let log = EventLog::new(
        dir.path().to_path_buf(),
        Arc::new(|e| panic!("event log: {e}")),
    );
    let id: TaskId = "k3m9x2q7ab".parse().unwrap();
    let path = log.path(&id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let first = serde_json::to_string(&notice(&id, "one", 1)).unwrap();
    std::fs::write(&path, format!("{first}\n{{\"task\":\"k3m9")).unwrap();
    log.publish(notice(&id, "two", 2));
    let (past, sub) = log.subscribe(&id, true).unwrap();
    log.unsubscribe(&sub);
    let messages: Vec<_> = past.iter().filter_map(|e| e.message.clone()).collect();
    assert_eq!(messages, ["one", "two"]);
}

/// Every event reaches a viewer exactly once, by replay or live, while the engine keeps
/// publishing during the subscribe.
#[tokio::test(flavor = "multi_thread")]
async fn subscribe_replay_and_live_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(EventLog::new(
        dir.path().to_path_buf(),
        Arc::new(|e| panic!("event log: {e}")),
    ));
    let id: TaskId = "k3m9x2q7ab".parse().unwrap();
    let total = u32::try_from(SUBSCRIBER_BUFFER - 1).unwrap(); // a late reader must not lag
    let publisher = {
        let (log, id) = (Arc::clone(&log), id.clone());
        std::thread::spawn(move || {
            for i in 0..total {
                log.publish(notice(&id, "n", i));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(1)); // let some events land in the log first
    let (past, mut sub) = log.subscribe(&id, true).unwrap();
    publisher.join().unwrap();
    let mut seen: Vec<u32> = past.iter().filter_map(|e| e.step).collect();
    while seen.len() < total as usize {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.rx.recv())
            .await
            .expect("events missing")
            .expect("ended");
        seen.push(ev.step.unwrap());
    }
    log.unsubscribe(&sub);
    for (i, step) in seen.iter().enumerate() {
        assert_eq!(*step as usize, i, "missed or duplicated");
    }
}

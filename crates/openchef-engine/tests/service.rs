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

use openchef_core::{
    Approval, ApprovalKind, ApprovalStatus, DecidedBy, PermissionMode, Task, TaskId, TaskStatus,
};
use openchef_engine::service::{
    AskRequest, Deps, Error, EventLog, RunRequest, SUBSCRIBER_BUFFER, Service, title,
};
use openchef_engine::{Agent, Config, DynRuntime, Event, EventKind, Ordered, Runtimes, Sink};
use openchef_fake::{FakeRuntime, Turn};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Failure, FailureKind, Question,
    SessionSpec,
};
use openchef_store::{ApprovalFilter, Store};
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
        self.service_deps(|d| d.default_permissions = default_permissions)
    }

    /// A service whose deps `tweak` adjusts.
    fn service_deps(&self, tweak: impl FnOnce(&mut Deps)) -> Service {
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
        let spec_for: openchef_engine::SpecFor =
            Arc::new(|_: &Task, _: &Agent, _: openchef_core::AttemptRole| {
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
                    confine: None,
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
        let mut deps = Deps {
            engine: cfg,
            event_dir: self.dir.path().join("sessions"),
            default_permissions: None,
            on_fail: None,
            approval_timeout: None,
            notify: None,
            tap: None,
            sandbox_check: None,
        };
        tweak(&mut deps);
        Service::new(deps).unwrap()
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

async fn wait_for(mut cond: impl AsyncFnMut() -> bool) {
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

// --- approvals (approvals_test.go) --------------------------------------------------

/// Starts a task whose agent hangs, so requests can be queued for it.
async fn running_task(e: &Env, s: &Service) -> TaskId {
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let t = s.run(e.run("p")).await.unwrap();
    wait_for(async || e.claude.turns().len() == 1).await;
    t.id
}

fn request(task: &TaskId, kind: ApprovalKind, tool: Option<&str>, summary: &str) -> AskRequest {
    AskRequest {
        task: task.clone(),
        agent: "opus".into(),
        kind,
        tool: tool.map(str::to_owned),
        summary: summary.into(),
        options: Vec::new(),
        raw: None,
    }
}

type Asked = tokio::task::JoinHandle<Result<Approval, Error>>;

fn ask(s: &Service, req: AskRequest, cancel: CancellationToken) -> Asked {
    let s = s.clone();
    tokio::spawn(async move { s.ask(req, &cancel).await })
}

/// The asker's result; a decided request wakes it at once, never by expiring.
async fn asked(res: Asked) -> Approval {
    tokio::time::timeout(Duration::from_secs(10), res)
        .await
        .expect("the asker missed the decision and waits for the request to expire")
        .unwrap()
        .unwrap()
}

async fn pending_one(s: &Service, id: &TaskId) -> Approval {
    let filter = ApprovalFilter {
        task: Some(id.clone()),
        pending: true,
        limit: None,
    };
    let mut got = Vec::new();
    wait_for(async || {
        got = s.approvals(filter.clone()).await.unwrap();
        got.len() == 1
    })
    .await;
    got.remove(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_and_decide() {
    let e = env().await;
    let notes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s = {
        let notes = Arc::clone(&notes);
        e.service_deps(move |d| {
            d.notify = Some(Arc::new(move |_: &str, body: &str| {
                notes.lock().unwrap().push(body.to_owned());
            }));
        })
    };
    let id = running_task(&e, &s).await;
    let (_, mut sub) = s.watch(&id, false).await.unwrap();

    let res = ask(
        &s,
        request(
            &id,
            ApprovalKind::Command,
            Some("Bash"),
            "go test ./...\nrm -rf build",
        ),
        CancellationToken::new(),
    );
    let p = pending_one(&s, &id).await;
    assert_eq!(
        p.summary, "go test ./...\nrm -rf build",
        "people approve exactly what runs"
    );
    let wrong = s
        .decide(p.id, ApprovalStatus::Answered, Some("x".into()))
        .await;
    assert!(
        matches!(wrong, Err(Error::Invalid(_))),
        "answering a command: {wrong:?}"
    );
    s.decide(p.id, ApprovalStatus::Allowed, None).await.unwrap();
    let got = asked(res).await;
    assert_eq!(got.status, ApprovalStatus::Allowed);
    assert_eq!(got.decided_by, Some(DecidedBy::User));
    let second = s.decide(p.id, ApprovalStatus::Denied, None).await;
    assert!(
        matches!(
            second,
            Err(Error::Settled {
                status: ApprovalStatus::Allowed,
                ..
            })
        ),
        "{second:?}"
    );
    // The viewer saw the request and its decision; it was watched, so no notification.
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let ev = tokio::time::timeout(Duration::from_secs(10), sub.rx.recv())
            .await
            .unwrap()
            .unwrap();
        if ev.approval.is_some() {
            kinds.push(ev.kind);
        }
    }
    assert_eq!(
        kinds,
        [EventKind::ApprovalRequested, EventKind::ApprovalDecided]
    );
    s.unwatch(&sub);

    // Unwatched: a question notifies the desktop.
    let mut q = request(&id, ApprovalKind::Question, None, "Which port?");
    q.options = vec!["8080".into()];
    let res = ask(&s, q, CancellationToken::new());
    let p = pending_one(&s, &id).await;
    // Notified while pending: a request settled before that point notifies no one.
    wait_for(async || !notes.lock().unwrap().is_empty()).await;
    s.decide(p.id, ApprovalStatus::Answered, Some("9090".into()))
        .await
        .unwrap();
    assert_eq!(asked(res).await.answer.as_deref(), Some("9090"));
    let sent = notes.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    stop(&s).await;
}

/// A decision made the moment the request is listed (an automatic approver is that
/// fast) still wakes the agent: it does not wait for the request to expire.
#[tokio::test(flavor = "multi_thread")]
async fn instant_decision_wakes_the_asker() {
    let e = env().await;
    let s = e.service_deps(|d| d.approval_timeout = Some(Duration::from_secs(60)));
    let id = running_task(&e, &s).await;
    let filter = ApprovalFilter {
        task: Some(id.clone()),
        pending: true,
        limit: None,
    };
    for _ in 0..200 {
        let res = ask(
            &s,
            request(&id, ApprovalKind::Command, Some("Bash"), "make"),
            CancellationToken::new(),
        );
        let p = loop {
            if let Some(p) = s.approvals(filter.clone()).await.unwrap().pop() {
                break p;
            }
            tokio::task::yield_now().await;
        };
        s.decide(p.id, ApprovalStatus::Allowed, None).await.unwrap();
        let got = asked(res).await;
        assert_eq!(got.status, ApprovalStatus::Allowed);
        assert_eq!(got.decided_by, Some(DecidedBy::User));
    }
    stop(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_times_out() {
    let e = env().await;
    let s = e.service_deps(|d| d.approval_timeout = Some(Duration::from_millis(50)));
    let id = running_task(&e, &s).await;
    let a = s
        .ask(
            request(&id, ApprovalKind::Command, Some("Bash"), "make"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(a.status, ApprovalStatus::Expired);
    assert_eq!(a.decided_by, Some(DecidedBy::Timeout));
    stop(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_withdrawn_when_agent_stops() {
    let e = env().await;
    let s = e.service();
    let id = running_task(&e, &s).await;
    let cancel = CancellationToken::new();
    let res = ask(
        &s,
        request(
            &id,
            ApprovalKind::Tool,
            Some("WebFetch"),
            "https://example.com",
        ),
        cancel.clone(),
    );
    let pending = pending_one(&s, &id).await;
    cancel.cancel();
    let r = res.await.unwrap();
    assert!(
        matches!(r, Err(Error::Withdrawn(n)) if n == pending.id),
        "{r:?}"
    );
    assert_eq!(
        e.store.approval(pending.id).await.unwrap().status,
        ApprovalStatus::Withdrawn
    );
    stop(&s).await;
}

/// A request still pending when the daemon dies stays listed, settled as expired by the
/// next daemon, which tells the task's viewers.
#[tokio::test(flavor = "multi_thread")]
async fn restart_expires_pending_requests() {
    let e = env().await;
    let first = e.service();
    let id = running_task(&e, &first).await;
    let res = ask(
        &first,
        request(&id, ApprovalKind::Command, Some("Bash"), "make"),
        CancellationToken::new(),
    );
    let p = pending_one(&first, &id).await;
    stop(&first).await;
    let r = res.await.unwrap();
    assert!(
        matches!(r, Err(Error::Stopping)),
        "ask across shutdown: {r:?}"
    );
    assert_eq!(
        e.store.approval(p.id).await.unwrap().status,
        ApprovalStatus::Pending,
        "shutdown settled the request"
    );

    e.claude.push([Turn::editing("", write_file)]);
    let second = e.service();
    let _killed = second.start().await.unwrap();
    let got = second
        .approvals(ApprovalFilter {
            task: Some(id.clone()),
            pending: false,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].status, ApprovalStatus::Expired);
    assert_eq!(got[0].decided_by, Some(DecidedBy::Daemon));
    let (past, sub) = second.watch(&id, true).await.unwrap();
    second.unwatch(&sub);
    assert!(
        past.iter().any(|ev| ev.kind == EventKind::ApprovalDecided
            && ev.approval.as_ref().is_some_and(|a| a.id == p.id)),
        "the expiry was not logged"
    );
    stop(&second).await;
}

/// A plan document as a planner answers it (final text).
fn plan_answer(steps: &[(&str, &str)]) -> String {
    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|(title, verify)| {
            // `true` is not a command on Windows; no verification keeps the test portable.
            let verify = (*verify != "true").then_some(*verify);
            serde_json::json!({"title": title, "prompt": format!("Do: {title}"), "files": ["out.txt"],
                "verify": verify, "size": "small", "importance": "normal", "reason": "needed"})
        })
        .collect();
    format!(
        "Here is the plan:\n```json\n{}\n```",
        serde_json::json!({"title": "Build it", "steps": steps})
    )
}

/// R5.1: a planner's plan waits for approval, runs every step once approved, and the
/// planner's attempt is recorded at step 0 with the `plan` role.
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_plans_and_runs_after_approval() {
    let e = env().await;
    let s = e.service();
    e.claude.push([Turn::completed(plan_answer(&[
        ("first", "true"),
        ("second", "true"),
    ]))]);
    let t = s.pipeline(e.run("build it"), false).await.unwrap();
    assert_eq!(t.status, TaskStatus::Planning);
    e.wait_status(&t.id, TaskStatus::AwaitingApproval).await;
    let (past, sub) = s.watch(&t.id, true).await.unwrap();
    s.unwatch(&sub);
    assert!(
        past.iter()
            .any(|ev| ev.status.as_deref() == Some("awaiting_approval")
                && ev
                    .message
                    .as_deref()
                    .is_some_and(|m| m.contains("Build it (2 steps)"))),
        "the plan-ready event"
    );
    let d = s.get(&t.id).await.unwrap();
    assert_eq!(
        d.steps.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    let planner = &d.attempts[0];
    assert!(
        planner.role == openchef_core::AttemptRole::Plan
            && planner.step_index == 0
            && planner.status == openchef_core::AttemptStatus::Succeeded,
        "{planner:?}"
    );
    let first_turn = e.claude.turns().first().cloned().unwrap_or_default();
    assert!(first_turn.contains("You are the planner") && first_turn.contains("build it"));

    e.claude.push([
        Turn::editing("one", write_file),
        Turn::editing("two", write_file),
    ]);
    let _approved = s.approve(&t.id).await.unwrap();
    e.wait_status(&t.id, TaskStatus::Done).await;
    let d = s.get(&t.id).await.unwrap();
    assert!(
        d.steps
            .iter()
            .all(|st| st.status == openchef_core::StepStatus::Verified),
        "{d:?}"
    );
    assert!(s.approve(&t.id).await.is_err(), "approving twice");
    stop(&s).await;
}

/// An invalid plan gets one repair turn quoting its problems; a second invalid answer
/// parks the task with the raw answer in its log.
#[tokio::test(flavor = "multi_thread")]
async fn pipeline_repairs_once_then_needs_attention() {
    let e = env().await;
    let s = e.service();
    e.claude.push([
        Turn::completed("I think we should start with the parser."),
        Turn::completed(plan_answer(&[("only", "true")])),
    ]);
    let t = s.pipeline(e.run("repairable"), false).await.unwrap();
    e.wait_status(&t.id, TaskStatus::AwaitingApproval).await;
    let turns = e.claude.turns();
    assert!(turns[1].contains("Your plan is not valid"), "{turns:?}");
    assert!(turns[1].contains("no JSON object"), "{turns:?}");

    e.claude.push([
        Turn::completed("still prose"),
        Turn::completed("prose again"),
    ]);
    let t = s.pipeline(e.run("hopeless"), false).await.unwrap();
    let parked = e.wait_status(&t.id, TaskStatus::NeedsAttention).await;
    assert!(
        parked
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("still not valid after a repair turn"),
        "{parked:?}"
    );
    let (past, sub) = s.watch(&t.id, true).await.unwrap();
    s.unwatch(&sub);
    assert!(
        past.iter().any(|ev| ev
            .message
            .as_deref()
            .is_some_and(|m| m.contains("prose again"))),
        "the raw answer is in the log"
    );
    // Retrying a pipeline without a plan plans again.
    e.claude
        .push([Turn::completed(plan_answer(&[("only", "true")]))]);
    let again = s.retry(&t.id).await.unwrap();
    assert_eq!(again.status, TaskStatus::Planning);
    e.wait_status(&t.id, TaskStatus::AwaitingApproval).await;
    stop(&s).await;
}

/// `plan edit` stores a valid edit and refuses an invalid one; `reject` cancels; `--yes`
/// runs a valid plan without stopping.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::many_single_char_names)] // e, s, t, d, p as in the other tests
async fn plan_edit_reject_and_auto_approve() {
    use openchef_engine::plan::Plan;
    let e = env().await;
    let s = e.service();
    e.claude
        .push([Turn::completed(plan_answer(&[("a", "true")]))]);
    let t = s.pipeline(e.run("edit me"), false).await.unwrap();
    e.wait_status(&t.id, TaskStatus::AwaitingApproval).await;
    let d = s.get(&t.id).await.unwrap();
    let mut p = Plan::from_steps("Edited", &d.steps);
    p.steps[0].title = "renamed".into();
    p.steps.push(p.steps[0].clone());
    let steps = s.edit_plan(&t.id, &p).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(s.get(&t.id).await.unwrap().steps[0].title, "renamed");
    p.steps[0].files = vec!["../escape".into()];
    let err = s.edit_plan(&t.id, &p).await.unwrap_err();
    assert!(err.to_string().contains("not valid"), "{err}");
    let rejected = s.reject(&t.id, Some("too big")).await.unwrap();
    assert_eq!(rejected.status, TaskStatus::Cancelled);
    assert!(
        s.edit_plan(&t.id, &p).await.is_err(),
        "editing a cancelled task"
    );

    e.claude.push([
        Turn::completed(plan_answer(&[("auto", "true")])),
        Turn::editing("done", write_file),
    ]);
    let t = s.pipeline(e.run("auto"), true).await.unwrap();
    e.wait_status(&t.id, TaskStatus::Done).await;
    stop(&s).await;
}

/// A daemon stopped while planning: the next start ends the left planning attempt and
/// plans again in a new conversation (D-037).
#[tokio::test(flavor = "multi_thread")]
async fn planning_restarts_after_a_daemon_restart() {
    let e = env().await;
    let first = e.service();
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let t = first.pipeline(e.run("plan me"), false).await.unwrap();
    wait_for(async || e.claude.turns().len() == 1).await;
    stop(&first).await;
    assert_eq!(
        e.store.task(&t.id).await.unwrap().status,
        TaskStatus::Planning
    );
    e.claude
        .push([Turn::completed(plan_answer(&[("one", "true")]))]);
    let second = e.service();
    second.start().await.unwrap();
    e.wait_status(&t.id, TaskStatus::AwaitingApproval).await;
    let attempts = e.store.attempts(&t.id).await.unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(
        attempts[0].status,
        openchef_core::AttemptStatus::Interrupted
    );
    assert_eq!(attempts[1].status, openchef_core::AttemptStatus::Succeeded);
    assert!(
        e.claude.turns()[1].contains("You are the planner"),
        "a fresh planning turn"
    );
    stop(&second).await;
}

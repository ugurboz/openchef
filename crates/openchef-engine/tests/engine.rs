//! Ported from `internal/engine/{harness,engine,recover}_test.go`: the engine against
//! scripted in-process runtimes (`openchef-fake`), a real git repository, a real store
//! and a real verification command (`verify-test-helper check:done.txt`).
//!
//! Dropped: the nil-router case of `TestNewValidatesConfig` (the router is not optional).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openchef_core::{
    AttemptStatus, Backend, Effort, Importance, Kind, Mode, PermissionMode, SessionState, Step,
    StepStatus, Task, TaskId, TaskStatus, TurnStatus,
};
use openchef_engine::verify::minimal_env;
use openchef_engine::{
    Agent, Config, DynRuntime, Engine, Error, Event, EventKind, Limits, Ordered, Runtimes, Sink,
};
use openchef_fake::{CallKind, FakeRuntime, Turn, tool_call};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Failure, FailureKind, Question,
    SessionSpec,
};
use openchef_store::{StepChange, Store, TaskChange};
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
        Box::pin(async { Ok("yes".to_owned()) })
    }
}

type Hook = Box<dyn Fn(&Event) + Send>;

/// Collects events; an optional hook runs on each.
#[derive(Default)]
struct Collect {
    events: Mutex<Vec<Event>>,
    hook: Mutex<Option<Hook>>,
}

impl Sink for Collect {
    fn publish(&self, event: Event) {
        if let Some(h) = self.hook.lock().unwrap().as_ref() {
            h(&event);
        }
        self.events.lock().unwrap().push(event);
    }
}

impl Collect {
    fn kinds(&self) -> Vec<EventKind> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .filter(|k| *k != EventKind::Agent)
            .collect()
    }

    fn notices(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::Notice)
            .filter_map(|e| e.message.clone())
            .collect()
    }
}

struct Harness {
    store: Store,
    repo: PathBuf,
    claude: FakeRuntime,
    codex: FakeRuntime,
    sink: Arc<Collect>,
    cfg: Config,
    verify: String,
    sleeps: Arc<Mutex<Vec<Duration>>>,
    exhausted: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["config", "user.name", "Test User"],
        &["config", "user.email", "test@example.com"],
    ] {
        git(dir, args);
    }
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "initial"]);
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    git_init(&repo);
    let store = Store::open(&dir.path().join("state.db")).await.unwrap();
    let claude = FakeRuntime::new("claude", []);
    let codex = FakeRuntime::new("codex", []);
    let sink = Arc::new(Collect::default());
    let (sleeps, exhausted) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut runtimes: Runtimes = HashMap::new();
    runtimes.insert(
        "claude".into(),
        Arc::new(claude.clone()) as Arc<dyn DynRuntime>,
    );
    runtimes.insert(
        "codex".into(),
        Arc::new(codex.clone()) as Arc<dyn DynRuntime>,
    );
    let router = Arc::new(Ordered {
        agents: vec![
            Agent {
                name: "opus".into(),
                runtime: "claude".into(),
                model: Some("claude-opus-5-5".into()),
                pool: "claude-max".into(),
                effort: Some(Effort::Medium),
            },
            Agent {
                name: "gpt".into(),
                runtime: "codex".into(),
                model: Some("gpt-5".into()),
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
            mode: PermissionMode::AcceptEdits,
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
        store.clone(),
        runtimes,
        router,
        spec_for,
        &dir.path().join("home"),
    );
    cfg.verify_env = minimal_env(std::env::vars_os(), &[]);
    cfg.verify_env.push((
        OsString::from("OPENCHEF_VERIFY_HELPER"),
        OsString::from("check:done.txt"),
    ));
    cfg.sink = sink.clone();
    cfg.sleep = Some({
        let sleeps = Arc::clone(&sleeps);
        Arc::new(move |d| {
            sleeps.lock().unwrap().push(d);
            Box::pin(async {})
        })
    });
    cfg.pool_exhausted = Some({
        let exhausted = Arc::clone(&exhausted);
        Arc::new(move |pool: &str, _| exhausted.lock().unwrap().push(pool.to_owned()))
    });
    let verify = env!("CARGO_BIN_EXE_verify-test-helper").to_owned();
    assert!(!verify.contains([' ', '"']), "helper path needs quoting");
    Harness {
        store,
        repo,
        claude,
        codex,
        sink,
        cfg,
        verify,
        sleeps,
        exhausted,
        _dir: dir,
    }
}

fn write_done(dir: &Path) -> Result<(), String> {
    std::fs::write(dir.join("done.txt"), "done\n").map_err(|e| e.to_string())
}

impl Harness {
    fn engine(&self) -> Engine {
        Engine::new(self.cfg.clone()).unwrap()
    }

    async fn task_with(&self, verify: Option<&str>, pin: Option<&str>) -> TaskId {
        let id = TaskId::generate().unwrap();
        let epoch = OffsetDateTime::UNIX_EPOCH;
        let task = Task {
            id: id.clone(),
            kind: Kind::Run,
            status: TaskStatus::Running,
            prompt: "create done.txt".into(),
            repo_root: self.repo.clone(),
            base_commit: None,
            mode: Mode::Isolated,
            backend: Backend::Local,
            importance: Importance::Normal,
            agent_pin: pin.map(str::to_owned),
            permissions: PermissionMode::AcceptEdits,
            branch: None,
            worktree: None,
            resume_at: None,
            reason: None,
            created_at: epoch,
            updated_at: epoch,
        };
        let step = Step {
            task_id: id.clone(),
            index: 1,
            title: "create done.txt".into(),
            prompt: "Create a file named done.txt.".into(),
            files: Vec::new(),
            verify: verify.map(str::to_owned),
            importance: Importance::Normal,
            status: StepStatus::Pending,
            retries: 0,
            verified_commit: None,
            updated_at: epoch,
        };
        self.store.create_task(task, vec![step]).await.unwrap();
        id
    }

    async fn run_task(&self) -> TaskId {
        self.task_with(Some(&self.verify.clone()), None).await
    }

    async fn task(&self, id: &TaskId) -> Task {
        self.store.task(id).await.unwrap()
    }

    async fn step(&self, id: &TaskId) -> Step {
        let mut steps = self.store.steps(id).await.unwrap();
        assert_eq!(steps.len(), 1);
        steps.remove(0)
    }

    async fn attempts(&self, id: &TaskId) -> Vec<openchef_core::Attempt> {
        self.store.attempts(id).await.unwrap()
    }

    /// The engine closed every session, in the fakes and the store.
    async fn no_live_sessions(&self) {
        assert_eq!(
            self.claude.live() + self.codex.live(),
            0,
            "fake sessions still open"
        );
        assert!(
            self.store.live_sessions().await.unwrap().is_empty(),
            "live sessions in the store"
        );
    }

    async fn run(&self, id: &TaskId) -> Result<(), Error> {
        self.engine().run(id, &CancellationToken::new()).await
    }

    /// Runs a task until its first turn hangs, then stops the engine as a shutdown would.
    async fn shutdown_mid_turn(&self) -> TaskId {
        self.claude.push([Turn {
            hang: true,
            ..Turn::default()
        }]);
        let id = self.run_task().await;
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let claude = self.claude.clone();
        drop(tokio::spawn(async move {
            wait_for(|| claude.turns().len() == 1).await;
            trigger.cancel();
        }));
        assert!(matches!(
            self.engine().run(&id, &shutdown).await,
            Err(Error::Shutdown)
        ));
        id
    }
}

async fn wait_for(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn worktree(t: &Task) -> PathBuf {
    t.worktree.clone().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn run_verifies_and_commits() {
    let h = harness().await;
    h.claude.push([Turn::editing("created", write_done)]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert_eq!(t.status, TaskStatus::Done, "{:?}", t.reason);
    let s = h.step(&id).await;
    assert!(
        s.status == StepStatus::Verified && s.verified_commit.is_some() && s.retries == 0,
        "{s:?}"
    );
    let sha = s.verified_commit.unwrap();
    assert_eq!(
        git(&worktree(&t), &["show", "--name-only", "--format=", &sha]),
        "done.txt"
    );
    assert!(
        git(&worktree(&t), &["log", "-1", "--format=%B", &sha]).contains("OpenChef-Agent: opus")
    );
    assert_eq!(
        git(&h.repo, &["status", "--porcelain"]),
        "",
        "user tree touched"
    );
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 1 && a[0].status == AttemptStatus::Succeeded && a[0].agent == "opus",
        "{a:?}"
    );
    h.no_live_sessions().await;
    let turns = h.claude.turns();
    assert!(
        turns.len() == 1
            && turns[0].contains("Step 1 of 1: create done.txt")
            && turns[0].contains(&h.verify),
        "{turns:?}"
    );
    let open = &h.claude.calls()[0];
    let spec = open.spec.as_ref().unwrap();
    assert_eq!(open.kind, CallKind::Open);
    assert_eq!(
        (spec.dir.clone(), spec.model.as_deref(), spec.effort),
        (worktree(&t), Some("claude-opus-5-5"), Some(Effort::Medium))
    );
    assert!(
        spec.on_spawn.is_some()
            && spec
                .log_path
                .as_ref()
                .unwrap()
                .starts_with(&h.cfg.session_root)
    );
    assert_eq!(
        h.sink.kinds(),
        [
            EventKind::StepStarted,
            EventKind::AttemptStarted,
            EventKind::VerifyStarted,
            EventKind::VerifyFinished,
            EventKind::AttemptEnded,
            EventKind::StepFinished,
            EventKind::TaskStatus
        ]
    );
}

/// The session row is recorded from `on_spawn` before the first message (§5.1).
#[tokio::test(flavor = "multi_thread")]
async fn session_recorded_from_spawn() {
    let h = harness().await;
    h.claude.push([Turn::editing("", write_done)]);
    let id = h.task_with(None, None).await;
    h.run(&id).await.unwrap();
    let sessions = h.store.task_sessions(&id, false).await.unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert!(
        s.pid > 1 << 22
            && s.native_id.as_deref() == Some("claude-session-1")
            && s.state == SessionState::Closed
            && s.log_dir.is_some(),
        "{s:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn step_without_verify_is_accepted_on_completion() {
    let h = harness().await;
    h.claude.push([Turn::completed("nothing to change")]);
    let id = h.task_with(None, None).await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    assert!(
        h.step(&id).await.verified_commit.is_some(),
        "empty step not recorded by a commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn verification_failure_is_fed_back() {
    let h = harness().await;
    h.claude
        .push([Turn::completed("forgot"), Turn::editing("", write_done)]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    assert_eq!(h.step(&id).await.retries, 1);
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2
            && turns[1].contains("missing done.txt")
            && turns[1].contains("exit code 1")
            && turns[1].contains("Step 1 of 1"),
        "{turns:?}"
    );
    assert_eq!(
        h.attempts(&id).await.len(),
        1,
        "a retry stays in the same session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_exhausted_needs_attention() {
    let h = harness().await;
    h.claude.push([
        Turn::completed("a"),
        Turn::completed("b"),
        Turn::completed("c"),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert!(
        t.status == TaskStatus::NeedsAttention
            && t.reason
                .as_deref()
                .unwrap_or("")
                .contains("after 2 retries"),
        "{t:?}"
    );
    let s = h.step(&id).await;
    assert!(s.status == StepStatus::Failed && s.retries == 2, "{s:?}");
    assert_eq!(h.attempts(&id).await[0].status, AttemptStatus::Failed);
    assert_eq!(h.claude.turns().len(), 3, "1 + 2 retries");
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_fails_over_with_patch() {
    let h = harness().await;
    let reset = OffsetDateTime::now_utc() + time::Duration::hours(2);
    let partial =
        |d: &Path| std::fs::write(d.join("partial.txt"), "half done\n").map_err(|e| e.to_string());
    let mut fail = Failure::new(FailureKind::QuotaExhausted, "limit reached");
    fail.resets_at = Some(reset);
    h.claude.push([Turn {
        fail: Some(fail),
        ..Turn::editing("", partial)
    }]);
    h.codex.push([Turn::editing("", write_done)]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert_eq!(t.status, TaskStatus::Done, "{:?}", t.reason);
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 2 && a[0].status == AttemptStatus::FailedOver && a[0].pool == "claude-max",
        "{a:?}"
    );
    assert!(
        a[1].status == AttemptStatus::Succeeded && a[1].agent == "gpt",
        "{a:?}"
    );
    let patch = worktree(&t)
        .join(".openchef/patches")
        .join(format!("step-1-opus-{}.patch", a[0].id));
    assert!(
        std::fs::read_to_string(&patch)
            .unwrap()
            .contains("half done")
    );
    let sha = h.step(&id).await.verified_commit.unwrap();
    assert_eq!(
        git(&worktree(&t), &["show", "--name-only", "--format=", &sha]),
        "done.txt",
        "partial work must be reset, not committed"
    );
    let handoff = h.codex.turns();
    assert!(
        handoff.len() == 1
            && handoff[0].contains("taking over this step from another agent (opus)"),
        "{handoff:?}"
    );
    assert!(
        handoff[0].contains(&format!(".openchef/patches/step-1-opus-{}.patch", a[0].id)),
        "{handoff:?}"
    );
    assert!(
        h.sink.notices().iter().any(|m| m.contains("failing over")),
        "{:?}",
        h.sink.notices()
    );
    assert_eq!(*h.exhausted.lock().unwrap(), ["claude-max"]);
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn all_pools_exhausted_waits_for_earliest_reset() {
    let h = harness().await;
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    let (early, late) = (
        now + time::Duration::hours(1),
        now + time::Duration::hours(4),
    );
    let failing = |at| {
        let mut f = Failure::new(FailureKind::QuotaExhausted, "out");
        f.resets_at = Some(at);
        Turn {
            fail: Some(f),
            ..Turn::default()
        }
    };
    h.claude.push([failing(late)]);
    h.codex.push([failing(early)]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert!(
        t.status == TaskStatus::WaitingQuota && t.resume_at == Some(early),
        "{t:?}"
    );
    assert_eq!(
        h.step(&id).await.status,
        StepStatus::Pending,
        "waiting_quota resets running steps"
    );
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_without_reset_time_needs_attention() {
    let h = harness().await;
    h.claude
        .push([Turn::failing(FailureKind::QuotaExhausted, "out")]);
    h.codex
        .push([Turn::failing(FailureKind::QuotaExhausted, "out")]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::NeedsAttention);
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_backs_off_in_same_session() {
    let h = harness().await;
    let mut first = Failure::new(FailureKind::RateLimited, "slow");
    first.resets_at = Some(OffsetDateTime::now_utc() + time::Duration::seconds(40));
    h.claude.push([
        Turn {
            fail: Some(first),
            ..Turn::default()
        },
        Turn::failing(FailureKind::RateLimited, "slow down"),
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    let sleeps = h.sleeps.lock().unwrap().clone();
    assert!(
        sleeps.len() == 2
            && sleeps[0] >= Duration::from_secs(35)
            && sleeps[0] <= Duration::from_secs(40),
        "{sleeps:?}"
    );
    assert_eq!(sleeps[1], Duration::from_mins(1), "backoff 2");
    let turns = h.claude.turns();
    assert!(
        turns.len() == 3 && turns[0] == turns[1] && turns[1] == turns[2],
        "rate-limited turns resend the same message"
    );
    assert_eq!(h.attempts(&id).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limits_beyond_budget_fail_over() {
    let h = harness().await;
    h.claude
        .push((0..4).map(|_| Turn::failing(FailureKind::RateLimited, "slow down")));
    h.codex.push([Turn::editing("", write_done)]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let a = h.attempts(&id).await;
    assert!(a.len() == 2 && a[1].agent == "gpt", "{a:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_resumes_once() {
    let h = harness().await;
    h.claude.push([
        Turn::failing(FailureKind::Crashed, "process exited"),
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    let resumed: Vec<String> = h
        .claude
        .calls()
        .into_iter()
        .filter(|c| c.kind == CallKind::Resume)
        .map(|c| c.session)
        .collect();
    assert_eq!(resumed, ["claude-session-1"]);
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2 && turns[1].contains("previous process ended unexpectedly"),
        "{turns:?}"
    );
    let sessions = h.store.task_sessions(&id, false).await.unwrap();
    assert!(
        sessions.len() == 2
            && sessions[1].resumed_from == Some(sessions[0].id)
            && sessions[1].native_id == sessions[0].native_id,
        "{sessions:?}"
    );
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_crash_needs_attention() {
    let h = harness().await;
    h.claude.push([
        Turn::failing(FailureKind::Crashed, "boom"),
        Turn::failing(FailureKind::Protocol, "garbage"),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert!(
        t.status == TaskStatus::NeedsAttention
            && t.reason.as_deref().unwrap_or("").contains("failed twice"),
        "{t:?}"
    );
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn loop_is_interrupted_and_nudged() {
    let h = harness().await;
    let events = (0..5)
        .map(|_| tool_call("Bash", "Bash:go test ./..."))
        .collect();
    h.claude.push([
        Turn {
            events,
            hang: true,
            ..Turn::default()
        },
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2 && turns[1].contains("repeated the same tool call 5 times"),
        "{turns:?}"
    );
    assert_eq!(
        h.step(&id).await.retries,
        1,
        "a nudge uses the retry budget"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn different_tool_calls_are_not_a_loop() {
    let h = harness().await;
    let events = ["a", "a", "b", "a", "a", "a"]
        .map(|s| tool_call("Bash", s))
        .to_vec();
    h.claude.push([Turn {
        events,
        ..Turn::editing("", write_done)
    }]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.claude.turns().len(), 1, "no loop expected");
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_turn_is_interrupted() {
    let mut h = harness().await;
    h.cfg.limits = Limits {
        idle_timeout: Duration::from_millis(100),
        ..Limits::default()
    };
    h.claude.push([
        Turn {
            hang: true,
            ..Turn::default()
        },
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2 && turns[1].contains("no activity for 100ms"),
        "{turns:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_timeout() {
    let mut h = harness().await;
    h.cfg.limits = Limits {
        turn_timeout: Duration::from_millis(100),
        ..Limits::default()
    };
    h.claude.push([
        Turn {
            hang: true,
            ..Turn::default()
        },
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2 && turns[1].contains("took longer than 100ms"),
        "{turns:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_failure_needs_attention() {
    let h = harness().await;
    h.claude
        .push([Turn::failing(FailureKind::Auth, "token expired")]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    let reason = t.reason.clone().unwrap_or_default();
    assert!(
        t.status == TaskStatus::NeedsAttention
            && reason.contains("not logged in")
            && reason.contains(&format!("openchef retry {id}")),
        "{t:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_failure_needs_attention() {
    let h = harness().await;
    h.claude.fail_next_open(openchef_runtime::Error::Start {
        runtime: "claude",
        message: "version 1.0 is too old".into(),
    });
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert!(
        t.status == TaskStatus::NeedsAttention
            && t.reason.as_deref().unwrap_or("").contains("too old"),
        "{t:?}"
    );
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 1 && a[0].status == AttemptStatus::Failed,
        "{a:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_agent() {
    let h = harness().await;
    h.codex.push([Turn::editing("", write_done)]);
    let id = h.task_with(Some(&h.verify.clone()), Some("gpt")).await;
    h.run(&id).await.unwrap();
    assert!(h.claude.calls().is_empty(), "claude used despite the pin");
}

/// A cancel from the CLI: the store moves the task first, then the daemon stops it.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_the_task() {
    let h = harness().await;
    h.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let id = h.run_task().await;
    let shutdown = CancellationToken::new();
    let (trigger, store, claude, task) = (
        shutdown.clone(),
        h.store.clone(),
        h.claude.clone(),
        id.clone(),
    );
    drop(tokio::spawn(async move {
        wait_for(|| claude.turns().len() == 1).await;
        let change = TaskChange {
            reason: Some("user".into()),
            resume_at: None,
        };
        store
            .transition_task(&task, TaskStatus::Running, TaskStatus::Cancelled, change)
            .await
            .unwrap();
        trigger.cancel();
    }));
    h.engine()
        .run(&id, &shutdown)
        .await
        .expect("a cancelled task is not an error");
    assert_eq!(h.task(&id).await.status, TaskStatus::Cancelled);
    assert_eq!(h.attempts(&id).await[0].status, AttemptStatus::Cancelled);
    h.no_live_sessions().await;
}

/// A shutdown leaves task, step, attempt and turn as they were for recovery, and stops
/// the agent.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_leaves_state_for_recovery() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    assert_eq!(h.task(&id).await.status, TaskStatus::Running);
    assert_eq!(h.step(&id).await.status, StepStatus::Running);
    assert_eq!(h.store.running_attempts().await.unwrap().len(), 1);
    let sessions = h.store.task_sessions(&id, false).await.unwrap();
    assert_eq!(sessions.len(), 1);
    h.store
        .in_flight_turn(sessions[0].id)
        .await
        .expect("in-flight turn for recovery");
    h.no_live_sessions().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn run_rejects_inactive_task() {
    let h = harness().await;
    let id = h.run_task().await;
    h.store
        .transition_task(
            &id,
            TaskStatus::Running,
            TaskStatus::Cancelled,
            TaskChange::default(),
        )
        .await
        .unwrap();
    assert!(matches!(h.run(&id).await, Err(Error::NotActive { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_validates_config() {
    let h = harness().await;
    let mut bad = h.cfg.clone();
    bad.worktree_root = PathBuf::from("relative");
    assert!(Engine::new(bad).is_err(), "relative worktree root accepted");
    let mut none = h.cfg.clone();
    none.runtimes = HashMap::new();
    assert!(Engine::new(none).is_err(), "no runtimes accepted");
}

/// A cancel during verification: the engine's next write conflicts and it returns
/// quietly without committing.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_during_verification() {
    let h = harness().await;
    h.claude.push([Turn::editing("", write_done)]);
    let id = h.run_task().await;
    {
        let (store, task) = (h.store.clone(), id.clone());
        *h.sink.hook.lock().unwrap() = Some(Box::new(move |e: &Event| {
            if e.kind == EventKind::VerifyStarted {
                let (store, task) = (store.clone(), task.clone());
                // Run the cancel synchronously, as a CLI would race the engine.
                let done = std::thread::spawn(move || {
                    tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(async move {
                            store
                                .transition_task(
                                    &task,
                                    TaskStatus::Running,
                                    TaskStatus::Cancelled,
                                    TaskChange::default(),
                                )
                                .await
                                .unwrap();
                        });
                });
                done.join().unwrap();
            }
        }));
    }
    h.run(&id).await.unwrap();
    let t = h.task(&id).await;
    assert_eq!(t.status, TaskStatus::Cancelled);
    let base = t.base_commit.clone().unwrap();
    assert_eq!(
        git(
            &worktree(&t),
            &["rev-list", "--count", &format!("{base}..HEAD")]
        ),
        "0",
        "commits after cancel"
    );
    h.no_live_sessions().await;
}

/// An agent that ignores the interrupt is killed (session closed) and resumed once.
#[tokio::test(flavor = "multi_thread")]
async fn stubborn_agent_is_replaced() {
    let mut h = harness().await;
    h.cfg.limits = Limits {
        idle_timeout: Duration::from_millis(50),
        stop_wait: Duration::from_millis(100),
        ..Limits::default()
    };
    h.claude.push([
        Turn {
            hang: true,
            ignore_interrupt: true,
            ..Turn::default()
        },
        Turn::editing("", write_done),
    ]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    assert!(
        h.claude.calls().iter().any(|c| c.kind == CallKind::Resume),
        "stubborn session was not replaced by a resume"
    );
    h.no_live_sessions().await;
}

/// A crash between the step commit and marking the step verified: the next run adopts
/// the commit instead of redoing the step.
#[tokio::test(flavor = "multi_thread")]
async fn committed_step_is_adopted() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    let t = h.task(&id).await;
    let repo = openchef_gitws::Repo::open(&h.repo).await.unwrap();
    let base = t.base_commit.clone().unwrap();
    let wt = repo
        .create_worktree(&worktree(&t), t.branch.as_deref().unwrap(), &base)
        .await
        .unwrap();
    write_done(&wt.path).unwrap();
    let commit = openchef_gitws::StepCommit {
        task: id.to_string(),
        step: 1,
        agent: "opus".into(),
        pool: "claude-max".into(),
        subject: String::new(),
    };
    let sha = wt.commit_step(&base, &commit).await.unwrap();
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    assert_eq!(h.step(&id).await.verified_commit, Some(sha));
    assert_eq!(h.claude.turns().len(), 1, "the adopted step needs no agent");
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 1 && a[0].status == AttemptStatus::Succeeded,
        "{a:?}"
    );
    h.no_live_sessions().await;
}

/// The retry budget belongs to the step, not to each attempt.
#[tokio::test(flavor = "multi_thread")]
async fn retry_budget_spans_failover() {
    let h = harness().await;
    h.claude.push([
        Turn::completed("a"),
        Turn::completed("b"),
        Turn::failing(FailureKind::QuotaExhausted, "out"),
    ]);
    h.codex.push([Turn::completed("c")]);
    let id = h.run_task().await;
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::NeedsAttention);
    assert_eq!(h.codex.turns().len(), 1, "the step had used its retries");
}

// --- recovery (recover_test.go) --------------------------------------------------------

/// §5.3: after a restart the interrupted turn is retried in the same conversation with a
/// recovery note, and the attempt continues.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_resumes_interrupted_turn() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    let first = h.claude.turns()[0].clone();
    let e = h.engine();
    e.kill_leftovers().await.unwrap();
    h.claude.push([Turn::editing("", write_done)]);
    e.run(&id, &CancellationToken::new()).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    let resumed: Vec<String> = h
        .claude
        .calls()
        .into_iter()
        .filter(|c| c.kind == CallKind::Resume)
        .map(|c| c.session)
        .collect();
    assert_eq!(
        resumed,
        ["claude-session-1"],
        "no resume of the interrupted conversation"
    );
    let turns = h.claude.turns();
    assert!(
        turns.len() == 2
            && turns[1].starts_with("The orchestrator restarted during your last turn"),
        "{turns:?}"
    );
    assert!(
        turns[1].contains("continue Step 1") && turns[1].ends_with(&first),
        "{turns:?}"
    );
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 1 && a[0].status == AttemptStatus::Succeeded,
        "the same attempt continues: {a:?}"
    );
    let sessions = h.store.task_sessions(&id, false).await.unwrap();
    assert!(
        sessions.len() == 2 && sessions[1].resumed_from == Some(sessions[0].id),
        "{sessions:?}"
    );
    let old = h.store.turns(sessions[0].id).await.unwrap();
    assert!(
        old[0].status == TurnStatus::Interrupted
            && old[0].outcome.as_deref() == Some("daemon restart"),
        "{old:?}"
    );
    h.no_live_sessions().await;
}

/// Recovery works even without the explicit sweep: `run` settles what it finds.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_without_sweep() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    h.claude.push([Turn::editing("", write_done)]);
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
}

/// A conversation that cannot be resumed is replaced by a fresh attempt.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_falls_back_to_fresh_attempt() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    h.claude.fail_next_open(openchef_runtime::Error::Start {
        runtime: "claude",
        message: "no conversation found".into(),
    });
    h.claude.push([Turn::editing("", write_done)]);
    h.run(&id).await.unwrap();
    assert_eq!(h.task(&id).await.status, TaskStatus::Done);
    let a = h.attempts(&id).await;
    assert!(
        a.len() == 2
            && a[0].status == AttemptStatus::Interrupted
            && a[1].status == AttemptStatus::Succeeded,
        "{a:?}"
    );
    assert!(
        a[0].outcome
            .as_deref()
            .unwrap_or("")
            .contains("no conversation found"),
        "{a:?}"
    );
    let turns = h.claude.turns();
    assert!(
        turns.last().unwrap().starts_with("Step 1 of 1"),
        "fresh attempt starts with the step prompt: {turns:?}"
    );
    h.no_live_sessions().await;
}

/// A session that never reported its native id cannot be resumed: its attempt ends and
/// a fresh one starts.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_of_session_without_native_id() {
    let h = harness().await;
    let id = h.run_task().await;
    h.store
        .transition_step(
            &id,
            1,
            StepStatus::Pending,
            StepStatus::Running,
            StepChange::default(),
        )
        .await
        .unwrap();
    let a = h
        .store
        .start_attempt(openchef_store::NewAttempt {
            task_id: id.clone(),
            step_index: 1,
            agent: "opus".into(),
            runtime: "claude".into(),
            model: String::new(),
            pool: "claude-max".into(),
        })
        .await
        .unwrap();
    h.store
        .record_session(openchef_store::NewSession {
            attempt_id: a.id,
            resumed_from: None,
            runtime: "claude".into(),
            native_id: None,
            pid: 1 << 23,
            start_token: 0,
            log_dir: None,
        })
        .await
        .unwrap();
    h.claude.push([Turn::editing("", write_done)]);
    h.run(&id).await.unwrap();
    let got = h.attempts(&id).await;
    assert!(
        got.len() == 2
            && got[0].status == AttemptStatus::Interrupted
            && got[1].status == AttemptStatus::Succeeded,
        "{got:?}"
    );
    assert!(
        !h.claude.calls().iter().any(|c| c.kind == CallKind::Resume),
        "resumed a session without a native id"
    );
    h.no_live_sessions().await;
}

/// `kill_leftovers` kills a recorded process that is still running and closes its
/// session.
#[tokio::test(flavor = "multi_thread")]
async fn kill_leftovers() {
    let h = harness().await;
    let id = h.shutdown_mid_turn().await;
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    env.push(("OPENCHEF_VERIFY_HELPER".into(), "sleep".into()));
    let spec = openchef_procs::Spec {
        program: h.verify.clone().into(),
        args: Vec::new(),
        dir: None,
        env,
        stderr: openchef_procs::Stderr::Discard,
    };
    let p = openchef_procs::Process::start(spec).unwrap();
    let pid = p.identity();
    let attempts = h.attempts(&id).await;
    // The attempt still runs after the shutdown, so a second live session can stand in
    // for a process the old daemon could not stop.
    h.store
        .record_session(openchef_store::NewSession {
            attempt_id: attempts[0].id,
            resumed_from: None,
            runtime: "claude".into(),
            native_id: None,
            pid: pid.pid,
            start_token: pid.start_token,
            log_dir: None,
        })
        .await
        .unwrap();
    assert_eq!(h.engine().kill_leftovers().await.unwrap(), 1);
    tokio::time::timeout(Duration::from_secs(10), p.wait())
        .await
        .expect("leftover still running");
    assert_eq!(h.store.live_sessions().await.unwrap().len(), 0);
}

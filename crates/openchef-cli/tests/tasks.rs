//! Ported from `internal/cli/{tasks,daemon}_test.go`: the task and daemon commands
//! against an in-process daemon with a fake agent instead of real CLIs.
#![cfg(unix)] // the daemon socket arrives on Windows with R3.3
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use openchef_cli::{Deps, StartDaemon, exit, run};
use openchef_config::{Detector, Paths};
use openchef_core::{Approval, ApprovalKind, ApprovalStatus, PermissionMode, Task, TaskId};
use openchef_daemon::quota::{Quota, Settings as QuotaSettings, quota_methods};
use openchef_daemon::{Options, Services, service_methods};
use openchef_engine::service::{AskRequest, Deps as ServiceDeps, Service};
use openchef_engine::{Agent, Config, DynRuntime, Ordered, Runtimes};
use openchef_fake::{FakeRuntime, Turn};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, EventBody, FailureKind,
    Question, SessionSpec, ToolCall,
};
use openchef_store::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Sends every request and question to the daemon's approval queue, like the
/// daemon's own approver does for whatever its evaluator cannot decide.
struct Queued {
    service: Arc<OnceLock<Service>>,
    task: TaskId,
    agent: String,
}

impl Queued {
    fn ask(
        &self,
        req: AskRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Approval, ApproverError>> {
        let service = self.service.get().cloned();
        Box::pin(async move {
            let service = service.ok_or_else(|| ApproverError("no service yet".into()))?;
            service
                .ask(req, &cancel)
                .await
                .map_err(|e| ApproverError(e.to_string()))
        })
    }

    fn request(&self, kind: ApprovalKind, summary: String) -> AskRequest {
        AskRequest {
            task: self.task.clone(),
            agent: self.agent.clone(),
            kind,
            tool: None,
            summary,
            options: Vec::new(),
            raw: None,
        }
    }
}

impl Approver for Queued {
    fn approve(
        &self,
        req: ApprovalRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let mut ask = self.request(req.kind.into(), req.command.unwrap_or_default());
        ask.tool = Some(req.tool);
        let asked = self.ask(ask, cancel);
        Box::pin(async move {
            let a = asked.await?;
            Ok(if a.status == ApprovalStatus::Allowed {
                Decision::allow()
            } else {
                Decision::deny(a.answer.unwrap_or_default())
            })
        })
    }

    fn answer(
        &self,
        q: Question,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        let mut ask = self.request(ApprovalKind::Question, q.text);
        ask.options = q.options;
        let asked = self.ask(ask, cancel);
        Box::pin(async move { Ok(asked.await?.answer.unwrap_or_default()) })
    }
}

/// The running daemon's quota monitor, for tests that feed it observations.
type QuotaSlot = Arc<Mutex<Option<Quota>>>;

/// A daemon thread: its shutdown token and the thread to join.
type Running = (CancellationToken, std::thread::JoinHandle<()>);

/// A repository, a home directory and a daemon that starts on demand, with a fake
/// agent instead of real CLIs.
struct CliEnv {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    claude: FakeRuntime,
    /// The reviewer's runtime, when the env has one.
    codex: Option<FakeRuntime>,
    deps: Deps,
    daemon: Arc<Mutex<Option<Running>>>,
    quota: QuotaSlot,
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

struct NoCli;

impl openchef_config::Runner for NoCli {
    fn run(
        &self,
        _: &Path,
        _: &[&str],
        _: Duration,
    ) -> Result<openchef_config::CommandOutput, String> {
        Err("no CLIs in tests".to_owned())
    }
}

fn no_detector() -> Detector {
    Detector::new(|_| None, NoCli, Duration::from_secs(1))
}

fn deps_for(home: &Path) -> Deps {
    Deps {
        detector: no_detector(),
        home: Some(home.to_path_buf()),
        interactive: false,
        start_daemon: None,
        cwd: None,
        mcp_stdio: None,
        sandbox: Some(std::sync::Arc::new(|| Ok("test sandbox".to_owned()))),
    }
}

fn spec(approver: Arc<dyn Approver>) -> SessionSpec {
    SessionSpec {
        dir: PathBuf::new(),
        model: None,
        effort: None,
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env: Vec::new(),
        approver,
        ask_user: None,
        log_path: None,
        on_spawn: None,
        confine: None,
    }
}

/// Serves `paths` with the fake agent until the token is cancelled.
/// The fakes and their agents: `opus` on claude implements; with `codex`, `gpt` on it
/// is the reviewer of another provider.
fn fleet(claude: FakeRuntime, codex: Option<FakeRuntime>) -> (Runtimes, Arc<Ordered>) {
    let mut runtimes: Runtimes = HashMap::new();
    runtimes.insert("claude".into(), Arc::new(claude) as Arc<dyn DynRuntime>);
    let mut agents = vec![Agent {
        name: "opus".into(),
        runtime: "claude".into(),
        model: None,
        pool: "claude-max".into(),
        effort: None,
    }];
    if let Some(codex) = codex {
        runtimes.insert("codex".into(), Arc::new(codex) as Arc<dyn DynRuntime>);
        agents.push(Agent {
            name: "gpt".into(),
            runtime: "codex".into(),
            model: None,
            pool: "chatgpt-plus".into(),
            effort: None,
        });
    }
    let router = Arc::new(Ordered {
        agents,
        cooldown: None,
    });
    (runtimes, router)
}

/// A daemon over the fakes of [`fleet`].
fn start_daemon(
    paths: &Paths,
    claude: &FakeRuntime,
    codex: Option<&FakeRuntime>,
    quota_slot: &QuotaSlot,
) -> Running {
    let shutdown = CancellationToken::new();
    let (paths, claude, token) = (paths.clone(), claude.clone(), shutdown.clone());
    let codex = codex.cloned();
    let quota_slot = Arc::clone(quota_slot);
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let init_paths = paths.clone();
            let init: openchef_daemon::Init = Box::new(move || {
                Box::pin(async move {
                    init_paths.ensure_dirs().map_err(|e| e.to_string())?;
                    let store = Store::open(&init_paths.state)
                        .await
                        .map_err(|e| e.to_string())?;
                    let (runtimes, router) = fleet(claude, codex);
                    let late: Arc<OnceLock<Service>> = Arc::new(OnceLock::new());
                    let spec_for: openchef_engine::SpecFor = {
                        let late = Arc::clone(&late);
                        Arc::new(move |t: &Task, a: &Agent, _: openchef_core::AttemptRole| {
                            Ok(spec(Arc::new(Queued {
                                service: Arc::clone(&late),
                                task: t.id.clone(),
                                agent: a.name.clone(),
                            })))
                        })
                    };
                    let mut cfg =
                        Config::new(store.clone(), runtimes, router, spec_for, &init_paths.home);
                    cfg.sleep = Some(Arc::new(|_| Box::pin(async {})));
                    let service = Service::new(ServiceDeps {
                        engine: cfg,
                        event_dir: init_paths.sessions.clone(),
                        default_permissions: None,
                        on_fail: None,
                        approval_timeout: None,
                        notify: None,
                        tap: None,
                        sandbox_check: None,
                    })
                    .map_err(|e| e.to_string())?;
                    let _first = late.set(service.clone());
                    let _killed = service.start().await.map_err(|e| e.to_string())?;
                    let quota = Quota::start(QuotaSettings {
                        store: store.clone(),
                        probes: Vec::new(),
                        interval: Duration::from_hours(1),
                        clock: Arc::new(time::OffsetDateTime::now_utc),
                        budgets: std::collections::BTreeMap::new(),
                        offset: time::UtcOffset::UTC,
                    })
                    .await?;
                    *quota_slot.lock().unwrap() = Some(quota.clone());
                    let stopping = service.clone();
                    let mut methods = service_methods(service);
                    methods.extend(quota_methods(quota.clone()));
                    Ok(Services {
                        methods,
                        on_stop: Some(Box::new(move || {
                            Box::pin(async move {
                                stopping
                                    .stop(Duration::from_secs(5))
                                    .await
                                    .map_err(|e| e.to_string())?;
                                quota.stop().await;
                                store.close().await.map_err(|e| e.to_string())
                            })
                        })),
                    })
                })
            });
            let options = Options {
                paths,
                version: "test".into(),
                methods: HashMap::new(),
                on_stop: None,
                init: Some(init),
                shutdown: token,
            };
            openchef_daemon::run(options).await.unwrap();
        });
    });
    (shutdown, thread)
}

fn wait_for_socket(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !paths.socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl CliEnv {
    fn new() -> Self {
        Self::build(false)
    }

    /// An env whose daemon also has a reviewer of another provider.
    fn with_reviewer() -> Self {
        Self::build(true)
    }

    fn build(reviewer: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("h");
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
        let claude = FakeRuntime::new("claude", []);
        let codex = reviewer.then(|| FakeRuntime::new("codex", []));
        let daemon: Arc<Mutex<Option<Running>>> = Arc::new(Mutex::new(None));
        let quota: QuotaSlot = Arc::new(Mutex::new(None));
        let start: StartDaemon = {
            let (daemon, claude, codex, quota) = (
                Arc::clone(&daemon),
                claude.clone(),
                codex.clone(),
                Arc::clone(&quota),
            );
            Arc::new(move |paths: &Paths| {
                let mut d = daemon.lock().unwrap();
                if d.is_none() {
                    *d = Some(start_daemon(paths, &claude, codex.as_ref(), &quota));
                    wait_for_socket(paths);
                }
                Ok(())
            })
        };
        let mut deps = deps_for(&home);
        deps.start_daemon = Some(start);
        deps.cwd = Some(repo.clone());
        Self {
            _dir: dir,
            repo,
            claude,
            codex,
            deps,
            daemon,
            quota,
        }
    }

    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        cli_with(&self.deps, args)
    }
}

impl Drop for CliEnv {
    fn drop(&mut self) {
        let running = self.daemon.lock().ok().and_then(|mut d| d.take());
        if let Some((token, thread)) = running {
            token.cancel();
            thread.join().unwrap();
        }
    }
}

fn cli_with(deps: &Deps, args: &[&str]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        std::iter::once("openchef").chain(args.iter().copied()),
        &mut out,
        &mut err,
        deps,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn task_id_from(out: &str) -> String {
    let f: Vec<&str> = out.split_whitespace().collect();
    assert!(f.len() >= 2 && f[0] == "task", "no task id in {out:?}");
    f[1].to_owned()
}

#[test]
fn run_streams_live_and_finishes() {
    let e = CliEnv::new();
    e.claude.push([Turn {
        events: vec![
            EventBody::Text("Adding the file.\n".into()),
            EventBody::ToolCall(ToolCall {
                id: "c1".into(),
                name: "Bash".into(),
                input: json!({"command": "cargo test"}),
                signature: String::new(),
            }),
        ],
        edit: Some(Box::new(|dir: &Path| {
            std::fs::write(dir.join("out.txt"), "x").map_err(|e| e.to_string())
        })),
        ..Turn::default()
    }]);
    let (code, out, err) = e.cli(&["run", "add", "out.txt"]);
    assert_eq!(code, exit::OK, "run: {err}\n{out}");
    for want in [
        "task ",
        "▸ Step 1: add out.txt",
        "  agent opus",
        "  Adding the file.",
        "● Bash  cargo test",
        "✓ step 1 done",
        "✔ done",
    ] {
        assert!(out.contains(want), "output lacks {want:?}:\n{out}");
    }
    let id = task_id_from(&out);

    let (code, list, _) = e.cli(&["status", "--all"]);
    assert!(
        code == exit::OK && list.contains(&id) && list.contains("done"),
        "status --all:\n{list}"
    );
    let (code, detail, _) = e.cli(&["status", &id]);
    assert!(
        code == exit::OK && detail.contains("step 1  verified") && detail.contains("attempt"),
        "status <task>:\n{detail}"
    );
    let (code, logs, _) = e.cli(&["logs", &id]);
    assert!(
        code == exit::OK && logs.contains("✓ step 1 done") && logs.contains("● Bash  cargo test"),
        "logs:\n{logs}"
    );
    let (code, json_logs, _) = e.cli(&["logs", &id, "--json"]);
    assert!(
        code == exit::OK && json_logs.contains(r#""kind":"step_started""#),
        "logs --json:\n{json_logs}"
    );
    let (code, active, _) = e.cli(&["status"]);
    assert!(
        code == exit::OK && active.contains("no active tasks"),
        "status:\n{active}"
    );
    assert!(e.repo.exists());
}

#[test]
fn run_failure_exits_non_zero() {
    let e = CliEnv::new();
    e.claude
        .push([Turn::failing(FailureKind::Auth, "token expired")]);
    let (code, out, err) = e.cli(&["run", "-q", "anything"]);
    assert_eq!(code, exit::FAILURE, "quiet failing run: {err}\n{out}");
    assert!(
        out.contains("! needs attention") && !out.contains("▸ Step"),
        "{out}"
    );
}

#[test]
fn detach_then_cancel() {
    let e = CliEnv::new();
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let (code, out, err) = e.cli(&["run", "--detach", "hang"]);
    assert!(
        code == exit::OK && out.contains("started; follow it with: openchef logs"),
        "detach: {err}\n{out}"
    );
    let id = task_id_from(&out);
    let deadline = Instant::now() + Duration::from_secs(10);
    while e.claude.turns().is_empty() {
        assert!(Instant::now() < deadline, "the agent never got the turn");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (code, out, err) = e.cli(&["cancel", &id]);
    assert!(
        code == exit::OK && out.contains("cancelled"),
        "cancel: {err}\n{out}"
    );
    let (code, _, err) = e.cli(&["cancel", &id]);
    assert!(
        code == exit::FAILURE && err.contains("already cancelled"),
        "second cancel: {err}"
    );
    let (code, out, _) = e.cli(&["status", "nonsense!"]);
    assert_eq!(code, exit::FAILURE, "bad id accepted: {out}");
    let (code, out, err) = e.cli(&["retry", &id]);
    assert!(
        code == exit::FAILURE && err.contains("can be retried"),
        "retry of a cancelled task: {err}\n{out}"
    );
}

#[test]
fn commands_without_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let deps = deps_for(&dir.path().join("h"));
    let (code, _, err) = cli_with(&deps, &["status"]);
    assert!(
        code == exit::FAILURE && err.contains("not running"),
        "status without daemon: {err}"
    );
}

#[test]
fn daemon_status_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let deps = deps_for(&home);
    let paths = Paths::in_home(&home).unwrap();

    let (code, out, _) = cli_with(&deps, &["daemon", "status"]);
    assert!(
        code == exit::NOT_RUNNING && out.contains("not running"),
        "status without daemon: {out}"
    );
    let (code, out, _) = cli_with(&deps, &["daemon", "stop"]);
    assert!(
        code == exit::OK && out.contains("not running"),
        "stop without daemon: {out}"
    );

    let (_token, thread) = start_daemon(
        &paths,
        &FakeRuntime::new("claude", []),
        None,
        &QuotaSlot::default(),
    );
    wait_for_socket(&paths);
    let (code, out, err) = cli_with(&deps, &["daemon", "status"]);
    assert!(
        code == exit::OK
            && out.contains("daemon running (pid")
            && out.contains(&*paths.socket.to_string_lossy()),
        "status: {err}\n{out}"
    );
    let (code, out, _) = cli_with(&deps, &["daemon", "status", "--json"]);
    assert!(
        code == exit::OK && out.contains(r#""daemonVersion": "test""#),
        "status --json: {out}"
    );
    let (code, out, _) = cli_with(&deps, &["daemon", "start"]);
    assert!(
        code == exit::OK && out.contains("already running"),
        "start: {out}"
    );
    let (code, out, err) = cli_with(&deps, &["daemon", "stop"]);
    assert!(
        code == exit::OK && out.contains("daemon stopped"),
        "stop: {err}\n{out}"
    );
    thread.join().unwrap(); // the daemon ended by itself
}

impl CliEnv {
    /// Polls `openchef approvals --json` until one request waits and returns it.
    fn wait_pending(&self) -> Approval {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (code, out, err) = self.cli(&["approvals", "--json"]);
            if code == exit::OK
                && let Ok(mut pending) = serde_json::from_str::<Vec<Approval>>(&out)
                && pending.len() == 1
            {
                return pending.remove(0);
            }
            assert!(
                Instant::now() < deadline,
                "no pending request: {err}\n{out}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn write_out(dir: &Path) -> Result<(), String> {
    std::fs::write(dir.join("out.txt"), "x").map_err(|e| e.to_string())
}

fn command(cmd: &str) -> ApprovalRequest {
    ApprovalRequest {
        session: openchef_runtime::SessionRef {
            runtime: "claude".into(),
            id: String::new(),
        },
        kind: openchef_runtime::RequestKind::Command,
        tool: "Bash".into(),
        command: Some(cmd.into()),
        paths: Vec::new(),
        description: String::new(),
        input: serde_json::Value::Null,
        raw: serde_json::Value::Null,
    }
}

/// The R4.2 acceptance round trip: an agent's request reaches the user through the
/// daemon, is listed, answered from another command, and the agent goes on.
#[test]
fn approval_round_trip() {
    let e = CliEnv::new();
    e.claude.push([Turn {
        approve: Some(command("make test")),
        edit: Some(Box::new(write_out)),
        ..Turn::default()
    }]);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| e.cli(&["run", "build", "it"]));
        let p = e.wait_pending();
        assert_eq!(
            (p.tool.as_deref(), p.summary.as_str(), p.agent.as_str()),
            (Some("Bash"), "make test", "opus"),
            "pending {p:?}"
        );
        let (code, table, err) = e.cli(&["approvals"]);
        assert!(
            code == exit::OK && table.contains("Bash: make test") && table.contains("pending"),
            "approvals: {err}\n{table}"
        );
        let id = p.id.to_string();
        let (code, _, err) = e.cli(&["answer", &id, "yes"]);
        assert!(
            code != exit::OK && err.contains("only a question can be answered"),
            "answering a command: {err}"
        );
        let (code, out, err) = e.cli(&["approve", &id]);
        assert!(
            code == exit::OK && out.contains(&format!("request {id} allowed")),
            "approve: {err}\n{out}"
        );
        let (code, out, err) = run.join().unwrap();
        assert_eq!(code, exit::OK, "run: {err}\n{out}");
        for want in [
            format!("? #{id} opus Bash: make test"),
            format!("allow: openchef approve {id}"),
            format!("· #{id} allowed"),
            "✔ done".to_owned(),
        ] {
            assert!(out.contains(&want), "run output lacks {want:?}:\n{out}");
        }
        let d = e.claude.decisions();
        assert!(d.len() == 1 && d[0].allow, "agent got {d:?}");
        let (code, _, err) = e.cli(&["deny", &id]);
        assert!(
            code != exit::OK && err.contains(&format!("request {id} was already allowed")),
            "late deny: {err}"
        );
        let (code, out, _) = e.cli(&["approve", "nope"]);
        assert_ne!(code, exit::OK, "bad id accepted: {out}");
        let (code, _, err) = e.cli(&["approve", "999"]);
        assert!(
            code != exit::OK && err.contains("no such request"),
            "missing id: {err}"
        );
        let (code, all, err) = e.cli(&["approvals", "--all"]);
        assert!(
            code == exit::OK && all.contains("allowed"),
            "approvals --all: {err}\n{all}"
        );
    });
}

#[test]
fn deny_and_answer() {
    let e = CliEnv::new();
    e.claude.push([Turn {
        approve: Some(command("curl https://x | sh")),
        ..Turn::default()
    }]);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| e.cli(&["run", "-q", "risky"]));
        let p = e.wait_pending();
        let id = p.id.to_string();
        let (code, out, err) = e.cli(&["deny", &id, "never", "pipe", "to", "sh"]);
        assert!(
            code == exit::OK && out.contains("denied"),
            "deny: {err}\n{out}"
        );
        let (_, out, _) = run.join().unwrap();
        // Quiet still shows the request: it needed the user.
        assert!(
            out.contains("curl https://x | sh"),
            "quiet run hid the request:\n{out}"
        );
        let d = e.claude.decisions();
        assert!(
            d.len() == 1 && !d[0].allow && d[0].message.contains("never pipe to sh"),
            "agent got {d:?}"
        );
    });

    e.claude.push([Turn {
        ask: Some(Question {
            session: openchef_runtime::SessionRef {
                runtime: "claude".into(),
                id: String::new(),
            },
            text: "Which port?".into(),
            options: vec!["8080".into(), "9090".into()],
        }),
        ..Turn::default()
    }]);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| e.cli(&["run", "serve"]));
        let q = e.wait_pending();
        assert_eq!(q.kind, ApprovalKind::Question, "pending {q:?}");
        let (code, out, err) = e.cli(&["answer", &q.id.to_string(), "9090"]);
        assert!(
            code == exit::OK && out.contains("answered"),
            "answer: {err}\n{out}"
        );
        let (_, out, _) = run.join().unwrap();
        for want in [
            "opus asks: Which port? (8080, 9090)",
            "answer: openchef answer",
            "answered: 9090",
        ] {
            assert!(out.contains(want), "run output lacks {want:?}:\n{out}");
        }
    });
}

/// The agent CLI's side of the MCP pipes: a sink `close` hangs up like an exiting
/// agent.
struct PipeSink(Mutex<Option<std::io::PipeWriter>>);

impl openchef_runtime::linerpc::LineSink for PipeSink {
    fn send_line(&self, mut line: Vec<u8>) -> BoxFuture<'static, std::io::Result<()>> {
        use std::io::Write as _;
        line.push(b'\n');
        let r = match self.0.lock().unwrap().as_mut() {
            Some(w) => w.write_all(&line),
            None => Err(std::io::ErrorKind::BrokenPipe.into()),
        };
        Box::pin(async move { r })
    }
}

/// An agent's `ask_user` call travels MCP server → daemon queue → `openchef answer`
/// and the answer comes back as the tool result (D-024 acceptance).
#[test]
fn ask_user_through_the_daemon() {
    use openchef_runtime::linerpc::{self, Peer};

    let e = CliEnv::new();
    e.claude.push([Turn {
        hang: true,
        ..Turn::default()
    }]);
    let (code, out, err) = e.cli(&["run", "--detach", "work"]);
    assert_eq!(code, exit::OK, "run: {err}\n{out}");
    let id = task_id_from(&out);

    // The agent's CLI side: pipes to an in-process `openchef mcp ask-user`.
    let (server_in, to_server) = std::io::pipe().unwrap();
    let (from_server, server_out) = std::io::pipe().unwrap();
    let stdio = Mutex::new(Some((server_in, server_out)));
    let mut deps = deps_for(e.deps.home.as_ref().unwrap());
    deps.mcp_stdio = Some(Arc::new(move || {
        let (r, w) = stdio.lock().unwrap().take().unwrap();
        (
            Box::new(r) as Box<dyn std::io::Read + Send>,
            Box::new(w) as Box<dyn std::io::Write + Send>,
        )
    }));
    std::thread::scope(|scope| {
        let served = scope.spawn(|| {
            cli_with(
                &deps,
                &["mcp", "ask-user", "--task", &id, "--agent", "opus"],
            )
        });
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let sink = Arc::new(PipeSink(Mutex::new(Some(to_server))));
        let agent = rt.block_on(async {
            let (agent, _notes) = Peer::new(
                from_server,
                Arc::clone(&sink) as Arc<dyn linerpc::LineSink>,
                linerpc::Options::default(),
            );
            let cancel = CancellationToken::new();
            agent
                .call_value(
                    "initialize",
                    json!({"protocolVersion": "2025-06-18"}),
                    &cancel,
                )
                .await
                .unwrap();
            agent
        });
        let call = rt.spawn(async move {
            agent
                .call_value(
                    "tools/call",
                    json!({"name": "ask_user",
                           "arguments": {"question": "Which port?", "options": ["8080", "9090"]}}),
                    &CancellationToken::new(),
                )
                .await
        });
        let p = e.wait_pending();
        assert_eq!(
            (p.summary.as_str(), p.agent.as_str(), p.task_id.as_str()),
            ("Which port?", "opus", id.as_str()),
            "pending {p:?}"
        );
        assert_eq!(p.options, ["8080", "9090"]);
        let (code, out, err) = e.cli(&["answer", &p.id.to_string(), "9090"]);
        assert_eq!(code, exit::OK, "answer: {err}\n{out}");
        let r = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), call).await })
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(r["content"][0]["text"], "9090", "tool result {r}");
        drop(sink.0.lock().unwrap().take()); // the agent exits: the server ends
        let (code, out, err) = served.join().unwrap();
        assert_eq!(code, exit::OK, "server: {err}\n{out}");
    });
    let (code, out, err) = e.cli(&["cancel", &id]);
    assert_eq!(code, exit::OK, "cancel: {err}\n{out}");
}

#[test]
fn quota_command() {
    use openchef_quota::{Observation, Source, Window};

    let e = CliEnv::new();
    // `quota` does not start a daemon itself.
    let home = e.deps.home.clone().unwrap();
    (e.deps.start_daemon.as_ref().unwrap())(&Paths::in_home(&home).unwrap()).unwrap();
    let (code, out, err) = e.cli(&["quota"]);
    assert!(
        code == exit::OK && out.contains("no quota data yet"),
        "empty quota: {err}\n{out}"
    );
    let now = time::OffsetDateTime::now_utc();
    let quota = e.quota.lock().unwrap().clone().unwrap();
    quota.observe(Observation {
        pool: "claude-max".into(),
        source: Source::Stream,
        at: now,
        windows: vec![
            Window {
                name: "five_hour".into(),
                utilization: 0.72,
                resets_at: Some(now + Duration::from_hours(2)),
                duration: None,
            },
            Window {
                name: "weekly".into(),
                utilization: 0.1,
                resets_at: None,
                duration: None,
            },
        ],
        exhausted: false,
    });
    let (code, out, err) = e.cli(&["quota"]);
    assert_eq!(code, exit::OK, "{err}");
    for want in [
        "claude-max",
        "yellow",
        "five_hour",
        "72%",
        "in 2h00m",
        "weekly",
        "10%",
    ] {
        assert!(out.contains(want), "quota lacks {want:?}:\n{out}");
    }
    let (code, out, _) = e.cli(&["quota", "--json"]);
    let r: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        code == exit::OK && r["pools"][0]["health"] == "yellow",
        "quota --json:\n{out}"
    );
    let (code, out, err) = e.cli(&["quota", "probes", "off"]);
    assert!(
        code == exit::OK && out.contains("usage probes off"),
        "probes off: {err}\n{out}"
    );
    let (code, _, _) = e.cli(&["quota", "probes", "maybe"]);
    assert_eq!(code, exit::USAGE, "bad switch accepted");
}

/// `--sandbox` fails closed: a daemon that cannot sandbox refuses the task instead of
/// running it unconfined.
#[test]
fn sandbox_is_refused_without_a_sandbox() {
    let e = CliEnv::new();
    let (code, out, err) = e.cli(&["run", "--sandbox", "anything"]);
    assert_ne!(code, exit::OK, "{out}");
    assert!(err.contains("cannot sandbox"), "{err}");
    assert_eq!(e.claude.remaining(), 0);
    let (_, list, _) = e.cli(&["status", "--all"]);
    assert!(!list.contains("anything"), "no task was created:\n{list}");
}

fn plan_json(titles: &[&str]) -> String {
    let steps: Vec<serde_json::Value> = titles
        .iter()
        .map(|t| {
            json!({"title": t, "prompt": format!("Do {t}"), "files": ["out.txt"], "verify": null,
                        "size": "small", "importance": "normal", "reason": "r"})
        })
        .collect();
    json!({"title": "Goal", "steps": steps}).to_string()
}

/// R5.1 acceptance: `pipeline` stops at the plan, `plan` shows it, `plan edit --file`
/// refuses an invalid plan and stores a valid one, `plan approve` runs it to done.
#[test]
fn pipeline_plan_edit_and_approve() {
    let e = CliEnv::new();
    e.claude.push([Turn::completed(plan_json(&["write out"]))]);
    let (code, out, err) = e.cli(&["pipeline", "-q", "add", "out.txt"]);
    assert_eq!(code, exit::OK, "pipeline: {err}\n{out}");
    let id = task_id_from(&out);
    for want in [
        "plan for task",
        "(awaiting_approval): add out.txt",
        "⏸ plan ready: Goal (1 step)",
        "1. write out  [normal]",
        "verify: none",
        "approve: openchef plan approve",
    ] {
        assert!(out.contains(want), "pipeline output lacks {want:?}:\n{out}");
    }
    let (code, shown, _) = e.cli(&["plan", &id, "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert!(
        code == exit::OK && doc["steps"][0]["title"] == "write out",
        "{shown}"
    );

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, plan_json(&[""])).unwrap();
    let (code, _, err) = e.cli(&["plan", "edit", &id, "--file", bad.to_str().unwrap()]);
    assert!(
        code != exit::OK && err.contains("not valid"),
        "invalid edit: {err}"
    );
    let good = dir.path().join("good.json");
    std::fs::write(&good, plan_json(&["first", "second"])).unwrap();
    let (code, out, err) = e.cli(&["plan", "edit", &id, "--file", good.to_str().unwrap()]);
    assert!(
        code == exit::OK && out.contains("2. second"),
        "edit: {err}\n{out}"
    );

    e.claude.push([
        Turn::editing("one", |d: &Path| {
            std::fs::write(d.join("out.txt"), "1").map_err(|e| e.to_string())
        }),
        Turn::editing("two", |d: &Path| {
            std::fs::write(d.join("out.txt"), "2").map_err(|e| e.to_string())
        }),
    ]);
    let (code, out, err) = e.cli(&["plan", "approve", &id]);
    assert!(
        code == exit::OK && out.contains("plan approved"),
        "approve: {err}\n{out}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, detail, _) = e.cli(&["status", &id]);
        if detail.contains("done") && detail.contains("step 2  verified") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the plan did not finish:\n{detail}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (code, _, err) = e.cli(&["plan", "approve", &id]);
    assert!(
        code != exit::OK && err.contains("only a plan awaiting approval"),
        "{err}"
    );
}

/// `pipeline --yes` runs a valid plan without stopping; `plan reject` cancels.
#[test]
fn pipeline_yes_and_reject() {
    let e = CliEnv::new();
    e.claude.push([
        Turn::completed(plan_json(&["auto"])),
        Turn::editing("done", |d: &Path| {
            std::fs::write(d.join("out.txt"), "x").map_err(|e| e.to_string())
        }),
    ]);
    let (code, out, err) = e.cli(&["pipeline", "--yes", "auto"]);
    assert_eq!(code, exit::OK, "{err}\n{out}");
    assert!(
        out.contains("✔ done"),
        "--yes followed through the approval:\n{out}"
    );

    e.claude.push([Turn::completed(plan_json(&["never"]))]);
    let (_, out, _) = e.cli(&["pipeline", "-q", "nope"]);
    let id = task_id_from(&out);
    let (code, out, err) = e.cli(&["plan", "reject", &id, "too", "risky"]);
    assert!(
        code == exit::OK && out.contains("cancelled"),
        "reject: {err}\n{out}"
    );
}

/// R5.2 acceptance: a reviewed pipeline step shows its findings in `status <task>`: the
/// reviewer's attempt by role, counts per status, and the finding still open.
#[test]
fn status_shows_review_findings() {
    let e = CliEnv::with_reviewer();
    let codex = e.codex.clone().unwrap();
    let findings = json!({"findings": [
        {"id": "F1", "severity": "blocker", "file": "out.txt", "line": 1,
         "claim": "out.txt is empty", "evidence": "the diff writes nothing", "fix": "write x"},
        {"id": "F2", "severity": "major", "file": "out.txt",
         "claim": "no trailing newline", "evidence": "POSIX text file", "fix": "add one"},
        {"id": "F3", "severity": "nit", "claim": "naming", "evidence": "style", "fix": "rename"}
    ]});
    codex.push([Turn::completed(findings.to_string())]);
    e.claude.push([
        Turn::completed(plan_json(&["write out"])),
        Turn::editing("done", |d: &Path| {
            std::fs::write(d.join("out.txt"), "x").map_err(|e| e.to_string())
        }),
        Turn::completed("FIXED F1"),
    ]);
    let (code, out, err) = e.cli(&["pipeline", "--yes", "-q", "write out.txt"]);
    assert_eq!(code, exit::OK, "{err}\n{out}");
    let id = task_id_from(&out);
    let (code, detail, err) = e.cli(&["status", &id]);
    assert_eq!(code, exit::OK, "{err}");
    for want in [
        "review gpt on chatgpt-plus: succeeded",
        "findings 1 open, 1 fixed, 1 nit",
        "open     F2 major   out.txt  no trailing newline",
    ] {
        assert!(detail.contains(want), "status lacks {want:?}:\n{detail}");
    }
    assert!(
        !detail.contains("F1 blocker"),
        "a fixed finding is only counted:\n{detail}"
    );
    let (_, json_out, _) = e.cli(&["status", &id, "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(
        doc["findings"].as_array().map(Vec::len),
        Some(3),
        "{json_out}"
    );
}

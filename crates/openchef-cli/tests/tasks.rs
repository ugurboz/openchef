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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openchef_cli::{Deps, StartDaemon, exit, run};
use openchef_config::{Detector, Paths};
use openchef_core::{PermissionMode, Task};
use openchef_daemon::{Options, Services, task_methods};
use openchef_engine::service::{Deps as ServiceDeps, Service};
use openchef_engine::{Agent, Config, DynRuntime, Ordered, Runtimes};
use openchef_fake::{FakeRuntime, Turn};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, EventBody, FailureKind,
    Question, SessionSpec, ToolCall,
};
use openchef_store::Store;
use serde_json::json;
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

/// A daemon thread: its shutdown token and the thread to join.
type Running = (CancellationToken, std::thread::JoinHandle<()>);

/// A repository, a home directory and a daemon that starts on demand, with a fake
/// agent instead of real CLIs.
struct CliEnv {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    claude: FakeRuntime,
    deps: Deps,
    daemon: Arc<Mutex<Option<Running>>>,
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
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
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
    }
}

/// Serves `paths` with the fake agent until the token is cancelled.
fn start_daemon(paths: &Paths, claude: &FakeRuntime) -> Running {
    let shutdown = CancellationToken::new();
    let (paths, claude, token) = (paths.clone(), claude.clone(), shutdown.clone());
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
                    let mut runtimes: Runtimes = HashMap::new();
                    runtimes.insert("claude".into(), Arc::new(claude) as Arc<dyn DynRuntime>);
                    let router = Arc::new(Ordered {
                        agents: vec![Agent {
                            name: "opus".into(),
                            runtime: "claude".into(),
                            model: None,
                            pool: "claude-max".into(),
                            effort: None,
                        }],
                        cooldown: None,
                    });
                    let spec_for: openchef_engine::SpecFor =
                        Arc::new(|_: &Task, _: &Agent| Ok(spec()));
                    let mut cfg =
                        Config::new(store.clone(), runtimes, router, spec_for, &init_paths.home);
                    cfg.sleep = Some(Arc::new(|_| Box::pin(async {})));
                    let service = Service::new(ServiceDeps {
                        engine: cfg,
                        event_dir: init_paths.sessions.clone(),
                        default_permissions: None,
                        on_fail: None,
                    })
                    .map_err(|e| e.to_string())?;
                    let _killed = service.start().await.map_err(|e| e.to_string())?;
                    let stopping = service.clone();
                    Ok(Services {
                        methods: task_methods(service),
                        on_stop: Some(Box::new(move || {
                            Box::pin(async move {
                                stopping
                                    .stop(Duration::from_secs(5))
                                    .await
                                    .map_err(|e| e.to_string())?;
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
        let daemon: Arc<Mutex<Option<Running>>> = Arc::new(Mutex::new(None));
        let start: StartDaemon = {
            let (daemon, claude) = (Arc::clone(&daemon), claude.clone());
            Arc::new(move |paths: &Paths| {
                let mut d = daemon.lock().unwrap();
                if d.is_none() {
                    *d = Some(start_daemon(paths, &claude));
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
            deps,
            daemon,
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

    let (_token, thread) = start_daemon(&paths, &FakeRuntime::new("claude", []));
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

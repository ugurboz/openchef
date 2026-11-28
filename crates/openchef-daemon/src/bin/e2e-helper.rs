//! A real daemon for the kill-the-daemon acceptance test (`tests/kill.rs`), wired like
//! the composition root but with a process-backed fake agent, ported from Go
//! `internal/e2e/procfake_test.go`.
//!
//! `e2e-helper <home> <flag> [stubborn]` serves `home` until stopped. Each agent
//! session is a real child process (`sh`), so the test can check that no agent outlives
//! a killed daemon. A turn completes once the `flag` file exists, writing `done.txt`
//! with the turn's prompt into the workspace; until then it waits, like an agent
//! thinking. A stubborn agent ignores SIGTERM and stdin EOF and starts a grandchild
//! whose pid it writes to `<home>/grandchild.pid`, so only the restarted daemon's sweep
//! of the recorded process group can clean up.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use openchef_config::Paths;
use openchef_core::{PermissionMode, Task};
use openchef_daemon::{Init, Options, Services, run, service_methods};
use openchef_engine::service::{Deps, Service};
use openchef_engine::{Agent, Config, DynRuntime, Ordered, Runtimes, SpecFor};
use openchef_procs::{Process, Spec};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Error, EventBody, Failure,
    FailureKind, Question, Runtime, Session, SessionRef, SessionSpec, TurnEvents, TurnInput,
    TurnResult, TurnSender, Usage,
};
use openchef_store::Store;
use tokio_util::sync::CancellationToken;

/// The polite agent: reads turns until stdin closes (as real CLIs do when their parent
/// dies, §5.1), then exits.
const AGENT: &str = "exec cat >/dev/null";

/// The stubborn agent: survives its daemon together with a grandchild.
const STUBBORN: &str = "trap '' TERM HUP; sleep 3600 & echo $! > \"$GRANDCHILD_PID_FILE\"; \
    cat >/dev/null; sleep 3600";

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

#[derive(Debug, Clone)]
struct ProcRuntime {
    flag: PathBuf,
    script: &'static str,
    pid_file: PathBuf,
    next: Arc<AtomicU64>,
}

impl ProcRuntime {
    async fn start(&self, spec: SessionSpec, id: String) -> Result<ProcSession, Error> {
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        env.push(("GRANDCHILD_PID_FILE".into(), self.pid_file.clone().into()));
        let mut process = Process::start(Spec {
            program: "sh".into(),
            args: vec!["-c".into(), self.script.into()],
            dir: Some(spec.dir.clone()),
            env,
            ..Spec::default()
        })?;
        if let Err(e) = spec.spawned(process.identity()).await {
            let _closed = process.close().await; // the spawn error is the one to report
            return Err(e);
        }
        if let Some(mut stdout) = process.take_stdout() {
            drop(std::thread::spawn(move || {
                // Draining only; a read error ends the drain.
                let _copied = std::io::copy(&mut stdout, &mut std::io::sink());
            }));
        }
        Ok(ProcSession {
            process,
            session: SessionRef {
                runtime: "claude".into(),
                id,
            },
            dir: spec.dir,
            flag: self.flag.clone(),
            interrupt: None,
        })
    }
}

impl Runtime for ProcRuntime {
    type Session = ProcSession;

    fn name(&self) -> &'static str {
        "claude"
    }

    async fn open(&self, spec: SessionSpec) -> Result<ProcSession, Error> {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        self.start(spec, format!("conv-{}-{n}", std::process::id()))
            .await
    }

    async fn resume(&self, session: SessionRef, spec: SessionSpec) -> Result<ProcSession, Error> {
        self.start(spec, session.id).await
    }
}

struct ProcSession {
    process: Process,
    session: SessionRef,
    dir: PathBuf,
    flag: PathBuf,
    interrupt: Option<CancellationToken>,
}

impl Session for ProcSession {
    fn session_ref(&self) -> &SessionRef {
        &self.session
    }

    async fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        self.process
            .stdin()
            .write(b"turn\n".to_vec())
            .await
            .map_err(|_| Error::SessionClosed)?;
        let interrupt = CancellationToken::new();
        self.interrupt = Some(interrupt.clone());
        let (mut tx, rx) = TurnSender::new();
        let (dir, flag, agent) = (self.dir.clone(), self.flag.clone(), self.process.identity());
        drop(tokio::spawn(async move {
            let body = wait_turn(&dir, &flag, &input.text, &interrupt, &cancel, agent).await;
            let _ended = tx.finish(body);
        }));
        Ok(rx)
    }

    fn interrupt(&mut self) -> impl Future<Output = Result<(), Error>> + Send {
        if let Some(t) = self.interrupt.take() {
            t.cancel();
        }
        std::future::ready(Ok(()))
    }

    async fn close(mut self) -> Result<(), Error> {
        if let Some(t) = self.interrupt.take() {
            t.cancel(); // ends a turn in progress
        }
        Ok(self.process.close().await?)
    }
}

async fn wait_turn(
    dir: &Path,
    flag: &Path,
    text: &str,
    interrupt: &CancellationToken,
    cancel: &CancellationToken,
    agent: openchef_procs::Identity,
) -> EventBody {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !openchef_procs::alive(&agent).unwrap_or(false) {
                    return EventBody::TurnFailed(Failure::new(FailureKind::Crashed, "the agent exited"));
                }
                if flag.exists() {
                    if let Err(e) = std::fs::write(dir.join("done.txt"), text) {
                        return EventBody::TurnFailed(Failure::new(FailureKind::Crashed, e.to_string()));
                    }
                    return EventBody::TurnCompleted(TurnResult {
                        final_text: "done".into(),
                        structured: None,
                        usage: Usage::default(),
                        denials: 0,
                        models: Vec::new(),
                        duration: Duration::ZERO,
                    });
                }
            }
            () = interrupt.cancelled() => return EventBody::TurnFailed(Failure::new(FailureKind::Interrupted, "interrupted")),
            () = cancel.cancelled() => return EventBody::TurnFailed(Failure::new(FailureKind::Timeout, "cancelled")),
        }
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
        confine: None,
    }
}

async fn services(paths: Paths, runtime: ProcRuntime) -> Result<Services, String> {
    paths.ensure_dirs().map_err(|e| e.to_string())?;
    let store = Store::open(&paths.state).await.map_err(|e| e.to_string())?;
    let mut runtimes: Runtimes = HashMap::new();
    runtimes.insert("claude".into(), Arc::new(runtime) as Arc<dyn DynRuntime>);
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
    let spec_for: SpecFor =
        Arc::new(|_: &Task, _: &Agent, _: openchef_core::AttemptRole| Ok(spec()));
    let cfg = Config::new(store.clone(), runtimes, router, spec_for, &paths.home);
    let service = Service::new(Deps {
        engine: cfg,
        event_dir: paths.sessions.clone(),
        default_permissions: None,
        on_fail: None,
        approval_timeout: None,
        notify: None,
        tap: None,
        sandbox_check: None,
    })
    .map_err(|e| e.to_string())?;
    let killed = service.start().await.map_err(|e| e.to_string())?;
    if killed > 0 {
        // The daemon log is best effort.
        let _logged = writeln!(
            std::io::stderr(),
            "killed processes left by a previous daemon: {killed}"
        );
    }
    let stopping = service.clone();
    Ok(Services {
        methods: service_methods(service),
        on_stop: Some(Box::new(move || {
            Box::pin(async move {
                stopping
                    .stop(Duration::from_secs(8))
                    .await
                    .map_err(|e| e.to_string())?;
                store.close().await.map_err(|e| e.to_string())
            })
        })),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let (Some(home), Some(flag)) = (args.first(), args.get(1)) else {
        return ExitCode::from(2);
    };
    let Ok(paths) = Paths::in_home(Path::new(home)) else {
        return ExitCode::from(2);
    };
    let stubborn = args.get(2).is_some_and(|a| a == "stubborn");
    let runtime = ProcRuntime {
        flag: PathBuf::from(flag),
        script: if stubborn { STUBBORN } else { AGENT },
        pid_file: paths.home.join("grandchild.pid"),
        next: Arc::new(AtomicU64::new(1)),
    };
    let init: Init = {
        let paths = paths.clone();
        Box::new(move || Box::pin(services(paths, runtime)))
    };
    let options = Options {
        paths,
        version: "e2e".to_owned(),
        methods: HashMap::new(),
        on_stop: None,
        init: Some(init),
        shutdown: CancellationToken::new(),
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _logged = writeln!(std::io::stderr(), "daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

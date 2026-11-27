//! Ported from `internal/e2e/kill_test.go`: R3's exit criterion (§19). A task survives
//! its daemon being killed mid-turn, no agent process outlives the dead daemon, and the
//! restarted daemon finishes the task in the same conversation.
#![cfg(unix)] // the daemon socket arrives on Windows with R3.3
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use openchef_api::{Client, InitializeResult, RunParams, finished};
use openchef_config::Paths;
use openchef_core::TaskId;
use openchef_daemon::{Detached, start_detached};
use openchef_procs::{Identity, alive, start_token};
use openchef_store::Store;

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

async fn wait_until(what: &str, limit: Duration, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn daemon_log(paths: &Paths) -> String {
    std::fs::read_to_string(paths.logs.join("daemon.log")).unwrap_or_default()
}

async fn start_daemon(paths: &Paths, flag: &Path, stubborn: bool) -> InitializeResult {
    let mut args = vec![paths.home.clone().into_os_string(), flag.into()];
    if stubborn {
        args.push("stubborn".into());
    }
    let d = Detached {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_e2e-helper")),
        args,
        env: None,
        log_path: paths.logs.join("daemon.log"),
        socket: paths.socket.clone(),
        timeout: Duration::from_secs(20),
    };
    match start_detached(&d).await {
        Ok(info) => info,
        Err(e) => panic!("start daemon: {e}\n{}", daemon_log(paths)),
    }
}

/// The identities of every recorded agent process of the task.
async fn agent_processes(paths: &Paths, id: &TaskId) -> Vec<Identity> {
    let store = Store::open(&paths.state).await.unwrap();
    let sessions = store.task_sessions(id, false).await.unwrap();
    store.close().await.unwrap();
    sessions
        .iter()
        .filter(|s| s.pid != 0)
        .map(|s| Identity {
            pid: s.pid,
            start_token: s.start_token,
        })
        .collect()
}

/// The stubborn agent's child, from the file the agent writes into the home.
async fn grandchild(paths: &Paths) -> Identity {
    let file = paths.home.join("grandchild.pid");
    let mut found = None;
    wait_until(
        "the grandchild pid file",
        Duration::from_secs(10),
        async || {
            let Ok(raw) = std::fs::read_to_string(&file) else {
                return false;
            };
            let Ok(pid) = raw.trim().parse::<u32>() else {
                return false;
            };
            let Ok(token) = start_token(pid) else {
                return false;
            };
            found = Some(Identity {
                pid,
                start_token: token,
            });
            true
        },
    )
    .await;
    found.unwrap()
}

fn is_alive(id: &Identity) -> bool {
    alive(id).unwrap_or(false)
}

/// Stops any daemon still serving the home, whatever the test's outcome.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let socket = self.0.clone();
        let stopped = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok(c) = Client::dial(&socket, "cleanup").await {
                    let _stopped = c.stop().await;
                    c.close();
                }
            });
        });
        let _joined = stopped.join();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_the_daemon() {
    kill_the_daemon_with(false).await;
}

/// The agent ignores SIGTERM and stdin EOF and has a grandchild, so it survives its
/// daemon; the restarted daemon's sweep (§5.3 step 2) must kill both before resuming.
#[tokio::test(flavor = "multi_thread")]
async fn kill_the_daemon_stubborn_agent() {
    kill_the_daemon_with(true).await;
}

async fn kill_the_daemon_with(stubborn: bool) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_home(&dir.path().join("h")).unwrap();
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
    let flag = dir.path().join("go.flag");

    let first = start_daemon(&paths, &flag, stubborn).await;
    let _cleanup = Cleanup(paths.socket.clone());
    let c = Client::dial(&paths.socket, "e2e").await.unwrap();
    let t = c
        .run(&RunParams {
            prompt: "write done.txt".into(),
            dir: repo.to_string_lossy().into_owned(),
            ..RunParams::default()
        })
        .await
        .unwrap();
    let id: TaskId = t.id.parse().unwrap();
    let mut agents = Vec::new();
    wait_until("the agent process", Duration::from_secs(20), async || {
        agents = agent_processes(&paths, &id).await;
        agents.first().is_some_and(is_alive)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await; // let the turn start
    c.close();

    // SIGKILL: no graceful shutdown of any kind.
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &first.pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "kill the daemon");
    wait_until("the daemon to die", Duration::from_secs(10), async || {
        matches!(
            Client::dial(&paths.socket, "probe").await,
            Err(openchef_api::Error::NotRunning)
        )
    })
    .await;
    let mut child = None;
    if stubborn {
        // It survives its daemon, which is the point of this variant.
        let g = grandchild(&paths).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        for p in [&agents[0], &g] {
            assert!(
                is_alive(p),
                "stubborn process {} died with the daemon; the test proves nothing",
                p.pid
            );
        }
        child = Some(g);
    } else {
        // No leaked processes: the agent dies with its daemon (parent-death signal on
        // Linux, stdin EOF on macOS) before any new daemon could sweep it.
        wait_until(
            "the orphaned agent to exit",
            Duration::from_secs(10),
            async || !is_alive(&agents[0]),
        )
        .await;
    }

    std::fs::write(&flag, "").unwrap();
    let second = start_daemon(&paths, &flag, stubborn).await;
    assert_ne!(second.pid, first.pid, "same daemon pid");
    if let Some(g) = child {
        wait_until(
            "the sweep to kill the leftover agent and its child",
            Duration::from_secs(10),
            async || !is_alive(&agents[0]) && !is_alive(&g),
        )
        .await;
        let log = daemon_log(&paths);
        assert!(
            log.contains("killed processes left by a previous daemon"),
            "the second daemon did not report its sweep:\n{log}"
        );
    }
    assert_finished_in_same_conversation(&paths, &id).await;
}

/// The restarted daemon finishes the task with the same attempt, the resumed turn
/// carries the recovery note, and no agent process is left.
async fn assert_finished_in_same_conversation(paths: &Paths, id: &TaskId) {
    let t = id.to_string();
    let c2 = Client::dial(&paths.socket, "e2e").await.unwrap();
    let mut detail = None;
    wait_until(
        "the task to finish after the restart",
        Duration::from_secs(30),
        async || match c2.get(&t).await {
            Ok(d) if finished(&d.task.status) => {
                detail = Some(d);
                true
            }
            _ => false,
        },
    )
    .await;
    let d = detail.unwrap();
    assert_eq!(d.task.status, "done", "task: {:?}", d.task.reason);
    let commit = d.steps[0].commit.clone().unwrap_or_default();
    assert!(
        d.attempts.len() == 1 && !commit.is_empty(),
        "detail {d:?}; the same attempt must finish"
    );
    let worktree = d.task.worktree.clone().unwrap();
    let out = std::process::Command::new("git")
        .args(["-C", &worktree, "show", &format!("{commit}:done.txt")])
        .output()
        .unwrap();
    let done = String::from_utf8_lossy(&out.stdout);
    assert!(
        done.starts_with("The orchestrator restarted during your last turn"),
        "done.txt {done:?}; the resumed turn must carry the recovery note"
    );
    for p in agent_processes(paths, id).await {
        assert!(
            !is_alive(&p),
            "agent process {} still alive after the task finished",
            p.pid
        );
    }
    c2.stop().await.unwrap();
    c2.close();
}

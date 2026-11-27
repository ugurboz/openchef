//! Ported from `internal/procs/procs_test.go` (Go reference).
//!
//! Dropped: `TestStartErrors`' cancelled-context case — `Process::start` is synchronous
//! and takes no context. Every test runs on Windows too: there the termination-signal
//! stage is skipped, so `ignored_term_signal_still_stops` exercises the forced kill.
// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader};
use std::time::{Duration, Instant};

use openchef_procs::{Error, Identity, Process, Spec, alive, kill_if_alive, start_token};
use tokio_util::sync::CancellationToken;

const HELPER: &str = env!("CARGO_BIN_EXE_procs-test-helper");

fn helper_env(mode: &str) -> Vec<(OsString, OsString)> {
    let mut env = vec![("OPENCHEF_PROCS_HELPER".into(), mode.into())];
    for k in ["SYSTEMROOT", "TMPDIR", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(k) {
            env.push((k.into(), v));
        }
    }
    env
}

fn start_helper(mode: &str) -> Process {
    Process::start(Spec {
        program: HELPER.into(),
        env: helper_env(mode),
        ..Spec::default()
    })
    .unwrap_or_else(|e| panic!("start {mode}: {e}"))
}

/// Reads one line of the child's stdout, or fails after 10 s.
async fn read_line(p: &mut Process) -> String {
    let stdout = p.take_stdout().expect("stdout already taken");
    let read = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line).unwrap();
        line.trim().to_owned()
    });
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("timed out reading child stdout")
        .unwrap()
}

fn parse_identity(line: &str) -> Identity {
    let (pid, token) = line
        .split_once(' ')
        .unwrap_or_else(|| panic!("unexpected helper output {line:?}"));
    Identity {
        pid: pid.parse().unwrap(),
        start_token: token.parse().unwrap(),
    }
}

async fn eventually_dead(id: Identity, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(&id).unwrap() {
        assert!(Instant::now() < deadline, "{what}: still alive after 10 s");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_done(p: &Process) -> openchef_procs::Exit {
    tokio::time::timeout(Duration::from_secs(10), p.wait())
        .await
        .expect("process did not finish within 10 s")
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_over_pipes() {
    let mut p = start_helper("echo");
    let stdin = p.stdin();
    stdin.write(b"hello\n".to_vec()).await.unwrap();
    assert_eq!(read_line(&mut p).await, "hello");
    stdin.close();
    assert!(wait_done(&p).await.success());
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_graceful_on_stdin_eof() {
    let mut p = start_helper("until-eof");
    let start = Instant::now();
    p.terminate(Duration::from_secs(10), &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "graceful stop took {:?}",
        start.elapsed()
    );
    assert!(
        p.exit().unwrap().success(),
        "expected a clean exit after EOF"
    );
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_escalates_when_stdin_is_ignored() {
    let mut p = start_helper("sleep");
    p.terminate(Duration::from_millis(200), &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !alive(&p.identity()).unwrap(),
        "still alive after terminate"
    );
    assert!(
        !p.exit().unwrap().success(),
        "expected a non-zero exit after a forced stop"
    );
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_kills_grandchildren() {
    let mut p = start_helper("spawn");
    let child = parse_identity(&read_line(&mut p).await);
    assert!(alive(&child).unwrap(), "grandchild should be running");
    p.terminate(Duration::from_millis(200), &CancellationToken::new())
        .await
        .unwrap();
    eventually_dead(child, "grandchild killed").await;
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn leader_exit_kills_leftovers() {
    let mut p = start_helper("spawn-and-exit");
    let child = parse_identity(&read_line(&mut p).await);
    assert!(wait_done(&p).await.success(), "leader exit");
    eventually_dead(child, "leftover grandchild killed after leader exit").await;
    p.close().await.unwrap();
}

#[test]
fn alive_cases() {
    let me = std::process::id();
    let token = start_token(me).unwrap();
    assert_eq!(start_token(me).unwrap(), token, "start token not stable");
    let cases = [
        (
            "self",
            Identity {
                pid: me,
                start_token: token,
            },
            true,
        ),
        (
            "wrong start token",
            Identity {
                pid: me,
                start_token: token + 1,
            },
            false,
        ),
        (
            "zero pid",
            Identity {
                pid: 0,
                start_token: 0,
            },
            false,
        ),
        (
            "unused pid",
            Identity {
                pid: 999_999_999,
                start_token: 0,
            },
            false,
        ),
    ];
    for (name, id, want) in cases {
        assert_eq!(alive(&id).unwrap(), want, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn alive_after_exit() {
    let mut p = start_helper("until-eof");
    let id = p.identity();
    assert!(alive(&id).unwrap(), "running process not alive");
    p.terminate(Duration::from_secs(5), &CancellationToken::new())
        .await
        .unwrap();
    assert!(!alive(&id).unwrap(), "exited process reported alive");
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_if_alive_sweeps_the_domain() {
    let mut p = start_helper("spawn");
    let child = parse_identity(&read_line(&mut p).await);
    assert!(
        kill_if_alive(&p.identity()).unwrap(),
        "first sweep should kill"
    );
    let _ = wait_done(&p).await;
    eventually_dead(child, "orphan sweep reaches grandchild").await;
    assert!(
        !kill_if_alive(&p.identity()).unwrap(),
        "second sweep should find nothing"
    );
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_term_signal_still_stops() {
    let mut p = start_helper("ignore-term");
    assert_eq!(read_line(&mut p).await, "ready");
    p.terminate(Duration::from_millis(200), &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !alive(&p.identity()).unwrap(),
        "process survived escalation"
    );
    p.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_token_skips_grace() {
    let mut p = start_helper("sleep");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let start = Instant::now();
    p.terminate(Duration::from_mins(1), &cancel).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "cancelled terminate waited {:?}",
        start.elapsed()
    );
    p.close().await.unwrap();
}

#[test]
fn start_errors() {
    assert!(matches!(
        Process::start(Spec::default()),
        Err(Error::EmptyProgram)
    ));
    let dir = tempfile::tempdir().unwrap();
    let missing = Spec {
        program: dir.path().join("does-not-exist"),
        ..Spec::default()
    };
    assert!(matches!(Process::start(missing), Err(Error::Spawn { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn close_is_idempotent() {
    let mut p = start_helper("until-eof");
    for _ in 0..3 {
        p.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_kills_the_domain() {
    let mut p = start_helper("spawn");
    let child = parse_identity(&read_line(&mut p).await);
    let leader = p.identity();
    drop(p);
    eventually_dead(leader, "leader killed on drop").await;
    eventually_dead(child, "grandchild killed on drop").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_write_after_close_fails() {
    let mut p = start_helper("until-eof");
    let stdin = p.stdin();
    stdin.close();
    let _ = wait_done(&p).await;
    assert!(matches!(
        stdin.write(b"late\n".to_vec()).await,
        Err(Error::StdinClosed { .. })
    ));
    p.close().await.unwrap();
}

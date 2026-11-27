//! Ported from `internal/verify/verify_test.go`, against `src/bin/verify-test-helper.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::ffi::OsString;
use std::time::{Duration, Instant};

use openchef_engine::verify::{self, Error, Spec, minimal_env};
use openchef_procs::{Identity, alive, start_token};
use tokio_util::sync::CancellationToken;

/// A spec that runs the helper in `mode`.
fn spec(mode: &str, dir: &tempfile::TempDir) -> Spec {
    let exe = env!("CARGO_BIN_EXE_verify-test-helper");
    assert!(
        !exe.contains([' ', '"']),
        "helper path {exe:?} needs shell quoting"
    );
    let mut env = minimal_env(std::env::vars_os(), &[]);
    env.push((
        OsString::from("OPENCHEF_VERIFY_HELPER"),
        OsString::from(mode),
    ));
    Spec {
        command: exe.to_owned(),
        dir: dir.path().to_path_buf(),
        env,
        timeout: Some(Duration::from_secs(30)),
        max_output: None,
        confine: None,
    }
}

async fn run(s: &Spec) -> verify::Outcome {
    verify::run(s, &CancellationToken::new()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn pass_and_fail() {
    let dir = tempfile::tempdir().unwrap();
    let ok = run(&spec("exit:0", &dir)).await;
    assert!(
        ok.passed && ok.exit_code == Some(0) && ok.output.contains("exiting with 0"),
        "{ok:?}"
    );
    let bad = run(&spec("exit:3", &dir)).await;
    assert!(
        !bad.passed && bad.exit_code == Some(3) && !bad.timed_out,
        "{bad:?}"
    );
    assert!(bad.summary().contains("exit code 3"), "{}", bad.summary());
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_syntax() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec("exit:0", &dir);
    s.command = format!("{0} && {0}", s.command);
    let o = run(&s).await;
    assert!(
        o.passed && o.output.matches("exiting with 0").count() == 2,
        "{o:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn output_keeps_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec("spam:200000", &dir);
    s.max_output = Some(1000);
    let o = run(&s).await;
    assert!(
        o.truncated && o.output.starts_with("BEGIN\n") && o.output.ends_with("END\n"),
        "{:?}",
        o.output
    );
    assert!(
        o.output.contains("bytes omitted") && o.output.len() <= 1100,
        "{} bytes",
        o.output.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stderr_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let o = run(&spec("stderr", &dir)).await;
    assert!(!o.passed && o.output.contains("on stderr"), "{o:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec("stdin", &dir);
    s.timeout = Some(Duration::from_secs(10));
    assert!(run(&s).await.passed);
}

/// A timeout stops the command and everything it started.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_kills_tree() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let mut s = spec(&format!("tree:{}", pid_file.display()), &dir);
    s.timeout = Some(Duration::from_secs(3));
    let watcher = {
        let pid_file = pid_file.clone();
        tokio::spawn(async move {
            for _ in 0..300 {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    && let Ok(start_token) = start_token(pid)
                {
                    return Some(Identity { pid, start_token });
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            None
        })
    };
    let start = Instant::now();
    let o = run(&s).await;
    assert!(o.timed_out && !o.passed && o.exit_code.is_none(), "{o:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3 + 2 + 2 + 2),
        "run took {:?}",
        start.elapsed()
    );
    let child = watcher.await.unwrap().expect("grandchild never started");
    // A killed process can stay a zombie for a moment until it is reaped.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(&child).unwrap_or(true) {
        assert!(
            Instant::now() < deadline,
            "grandchild {} still alive",
            child.pid
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_command() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    drop(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    }));
    let start = Instant::now();
    let err = verify::run(&spec("sleep", &dir), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled { .. }), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "run took {:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn env_is_not_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec("env", &dir);
    s.env.push(("OPENCHEF_VERIFY_EXTRA".into(), "1".into()));
    let o = run(&s).await;
    assert!(o.passed, "{o:?}");
    assert!(
        o.output.contains("OPENCHEF_VERIFY_EXTRA=1"),
        "explicit variable missing: {}",
        o.output
    );
    for line in o.output.lines() {
        let key = line.split('=').next().unwrap_or("").to_uppercase();
        assert!(
            !key.contains("API_KEY") && !key.contains("TOKEN"),
            "secret-looking variable leaked: {key}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_spec() {
    let dir = tempfile::tempdir().unwrap();
    let base = Spec {
        command: "true".into(),
        dir: dir.path().to_path_buf(),
        env: Vec::new(),
        timeout: None,
        max_output: None,
        confine: None,
    };
    let cases = [
        (
            "command",
            Spec {
                command: String::new(),
                ..base.clone()
            },
        ),
        (
            "dir",
            Spec {
                dir: std::path::PathBuf::new(),
                ..base.clone()
            },
        ),
        (
            "missing",
            Spec {
                dir: dir.path().join("nope"),
                ..base
            },
        ),
    ];
    for (name, s) in cases {
        assert!(
            matches!(
                verify::run(&s, &CancellationToken::new()).await,
                Err(Error::InvalidSpec(_))
            ),
            "{name}"
        );
    }
}

/// A sandboxed command starts through its confinement, and a refusal means it never
/// runs (§21.3: fail closed).
#[tokio::test(flavor = "multi_thread")]
async fn confined_commands_start_through_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut s = spec("exit:0", &dir);
    let flag = std::sync::Arc::clone(&seen);
    s.confine = Some(std::sync::Arc::new(move |spec| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(spec)
    }));
    assert!(run(&s).await.passed);
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        "the sandbox wrapped the command"
    );
    s.confine = Some(std::sync::Arc::new(|_| Err("no Landlock".to_owned())));
    let err = verify::run(&s, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sandbox refused"), "{err}");
}

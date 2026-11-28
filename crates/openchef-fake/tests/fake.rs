//! Ported from Go `internal/runtime/fake/fake_test.go`, plus the behaviours engine
//! tests rely on (approvals, questions, stubborn turns, deadlines, open failures).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openchef_core::PermissionMode;
use openchef_fake::{CallKind, FakeRuntime, Turn, tool_call};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Error, EventBody, FailureKind,
    OnSpawn, Question, RequestKind, Runtime as _, Session as _, SessionRef, SessionSpec,
    TurnEvents, TurnInput,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Decides with a fixed decision; `hold` makes it wait for cancellation instead.
struct Scripted {
    decision: Decision,
    answer: String,
    hold: bool,
}

impl Approver for Scripted {
    fn approve(
        &self,
        _: ApprovalRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let (d, hold) = (self.decision.clone(), self.hold);
        Box::pin(async move {
            if hold {
                cancel.cancelled().await;
                return Err(ApproverError("cancelled".into()));
            }
            Ok(d)
        })
    }

    fn answer(
        &self,
        _: Question,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        let a = self.answer.clone();
        Box::pin(async move { Ok(a) })
    }
}

fn spec_with(approver: Scripted, dir: PathBuf) -> SessionSpec {
    SessionSpec {
        dir,
        model: None,
        effort: None,
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env: Vec::new(),
        approver: Arc::new(approver),
        ask_user: None,
        log_path: None,
        on_spawn: None,
        confine: None,
    }
}

fn spec(dir: PathBuf) -> SessionSpec {
    spec_with(
        Scripted {
            decision: Decision::allow(),
            answer: String::new(),
            hold: false,
        },
        dir,
    )
}

async fn last(rx: &mut TurnEvents) -> EventBody {
    let mut end = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("turn did not end")
        {
            Some(e) => end = Some(e.body),
            None => return end.expect("no events"),
        }
    }
}

fn kind(body: &EventBody) -> Option<FailureKind> {
    match body {
        EventBody::TurnFailed(f) => Some(f.kind),
        _ => None,
    }
}

fn text(body: &EventBody) -> &str {
    match body {
        EventBody::TurnCompleted(r) => &r.final_text,
        other => panic!("not completed: {other:?}"),
    }
}

fn request() -> ApprovalRequest {
    ApprovalRequest {
        session: SessionRef {
            runtime: "claude".into(),
            id: "x".into(),
        },
        kind: RequestKind::Command,
        tool: "Bash".into(),
        command: Some("rm -rf build".into()),
        paths: Vec::new(),
        description: String::new(),
        input: Value::Null,
        raw: Value::Null,
    }
}

#[tokio::test]
async fn script_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let rt = FakeRuntime::new(
        "claude",
        [
            Turn::completed("hi"),
            Turn {
                hang: true,
                ..Turn::default()
            },
        ],
    );
    let mut s = rt.open(spec(dir.path().into())).await.unwrap();
    let mut rx = s
        .turn(TurnInput::text("one"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text(&last(&mut rx).await), "hi");
    let mut rx = s
        .turn(TurnInput::text("two"), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        s.turn(TurnInput::text("three"), CancellationToken::new())
            .await,
        Err(Error::TurnInProgress)
    ));
    s.close().await.unwrap();
    assert_eq!(
        kind(&last(&mut rx).await),
        Some(FailureKind::Interrupted),
        "hanging turn after close"
    );
    assert_eq!((rt.live(), rt.remaining()), (0, 0));
    assert_eq!(rt.turns(), ["one", "two"], "a refused turn is not recorded");
}

#[tokio::test]
async fn script_exhausted() {
    let rt = FakeRuntime::new("codex", []);
    let mut s = rt.open(spec(std::env::temp_dir())).await.unwrap();
    let mut rx = s
        .turn(TurnInput::text("x"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Protocol));
    s.close().await.unwrap();
}

#[tokio::test]
async fn events_edit_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    let rt = FakeRuntime::new("claude", []);
    rt.push([
        Turn {
            events: vec![
                EventBody::Text("working".into()),
                tool_call("Read", "Read:{}"),
            ],
            ..Turn::editing("done", |d| {
                std::fs::write(d.join("out.txt"), "x").map_err(|e| e.to_string())
            })
        },
        Turn::editing("never", |_| Err("disk full".into())),
        Turn::failing(FailureKind::QuotaExhausted, "limit"),
    ]);
    let mut s = rt.open(spec(dir.path().into())).await.unwrap();
    let mut rx = s
        .turn(TurnInput::text("a"), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(rx.recv().await.unwrap().body, EventBody::Text(t) if t == "working"));
    assert!(
        matches!(rx.recv().await.unwrap().body, EventBody::ToolCall(c) if c.signature == "Read:{}")
    );
    assert_eq!(text(&last(&mut rx).await), "done");
    assert!(dir.path().join("out.txt").exists());
    let mut rx = s
        .turn(TurnInput::text("b"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Crashed));
    let mut rx = s
        .turn(TurnInput::text("c"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        kind(&last(&mut rx).await),
        Some(FailureKind::QuotaExhausted)
    );
    s.close().await.unwrap();
}

#[tokio::test]
async fn interrupt_and_deadline() {
    let rt = FakeRuntime::new(
        "claude",
        [
            Turn {
                hang: true,
                ..Turn::default()
            },
            Turn {
                hang: true,
                ..Turn::default()
            },
        ],
    );
    let mut s = rt.open(spec(std::env::temp_dir())).await.unwrap();
    let mut rx = s
        .turn(TurnInput::text("a"), CancellationToken::new())
        .await
        .unwrap();
    s.interrupt().await.unwrap();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Interrupted));
    let cancel = CancellationToken::new();
    let mut rx = s.turn(TurnInput::text("b"), cancel.clone()).await.unwrap();
    cancel.cancel();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Timeout));
    s.close().await.unwrap();
    let kinds: Vec<CallKind> = rt.calls().iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            CallKind::Open,
            CallKind::Turn,
            CallKind::Interrupt,
            CallKind::Turn,
            CallKind::Close
        ]
    );
}

#[tokio::test]
async fn stubborn_turn_ignores_interrupt_and_close() {
    let rt = FakeRuntime::new(
        "claude",
        [Turn {
            hang: true,
            ignore_interrupt: true,
            ..Turn::default()
        }],
    );
    let mut s = rt.open(spec(std::env::temp_dir())).await.unwrap();
    let cancel = CancellationToken::new();
    let mut rx = s.turn(TurnInput::text("a"), cancel.clone()).await.unwrap();
    s.interrupt().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "stubborn turn ended on interrupt"
    );
    tokio::time::timeout(Duration::from_secs(5), s.close())
        .await
        .expect("close waited for a stubborn turn")
        .unwrap();
    cancel.cancel();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Timeout));
}

#[tokio::test]
async fn approvals_and_questions() {
    let dir = tempfile::tempdir().unwrap();
    let deny = Scripted {
        decision: Decision::deny("no rm"),
        answer: "teal".into(),
        hold: false,
    };
    let rt = FakeRuntime::new(
        "claude",
        [
            Turn {
                approve: Some(request()),
                ..Turn::editing("", |d| {
                    std::fs::write(d.join("x"), "").map_err(|e| e.to_string())
                })
            },
            Turn {
                ask: Some(Question {
                    session: SessionRef {
                        runtime: "claude".into(),
                        id: "x".into(),
                    },
                    text: "colour?".into(),
                    options: Vec::new(),
                }),
                ..Turn::default()
            },
        ],
    );
    let mut s = rt.open(spec_with(deny, dir.path().into())).await.unwrap();
    let mut rx = s
        .turn(TurnInput::text("a"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        text(&last(&mut rx).await),
        "no rm",
        "denial message becomes the final text"
    );
    assert!(!dir.path().join("x").exists(), "edit ran after a denial");
    let mut rx = s
        .turn(TurnInput::text("b"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text(&last(&mut rx).await), "teal");
    assert_eq!(rt.decisions().len(), 1);
    assert!(!rt.decisions()[0].allow);
    s.close().await.unwrap();
}

#[tokio::test]
async fn interrupt_cancels_pending_approval() {
    let hold = Scripted {
        decision: Decision::allow(),
        answer: String::new(),
        hold: true,
    };
    let rt = FakeRuntime::new(
        "claude",
        [Turn {
            approve: Some(request()),
            ..Turn::completed("x")
        }],
    );
    let mut s = rt
        .open(spec_with(hold, std::env::temp_dir()))
        .await
        .unwrap();
    let mut rx = s
        .turn(TurnInput::text("a"), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.interrupt().await.unwrap();
    assert_eq!(kind(&last(&mut rx).await), Some(FailureKind::Interrupted));
    s.close().await.unwrap();
}

#[tokio::test]
async fn open_errors_resume_and_on_spawn() {
    let rt = FakeRuntime::new("codex", [Turn::completed("ok")]);
    rt.fail_next_open(Error::Start {
        runtime: "codex",
        message: "not logged in".into(),
    });
    assert!(rt.open(spec(std::env::temp_dir())).await.is_err());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook: OnSpawn = {
        let seen = Arc::clone(&seen);
        Arc::new(move |id| {
            seen.lock().unwrap().push(id.pid);
            Box::pin(async { Ok(()) })
        })
    };
    let s = rt
        .open(SessionSpec {
            on_spawn: Some(hook),
            confine: None,
            ..spec(std::env::temp_dir())
        })
        .await
        .unwrap();
    assert_eq!(s.session_ref().id, "codex-session-2");
    assert!(
        seen.lock().unwrap()[0] >= 1 << 23,
        "fake pids must not collide with real ones"
    );
    let reference = s.session_ref().clone();
    s.close().await.unwrap();
    let r = rt
        .resume(reference.clone(), spec(std::env::temp_dir()))
        .await
        .unwrap();
    assert_eq!(r.session_ref(), &reference);
    r.close().await.unwrap();
    assert!(
        rt.resume(
            SessionRef {
                runtime: "claude".into(),
                id: "x".into()
            },
            spec(std::env::temp_dir())
        )
        .await
        .is_err()
    );
    let rejecting: OnSpawn = Arc::new(|_| Box::pin(async { Err("not in job".to_owned()) }));
    assert!(
        rt.open(SessionSpec {
            on_spawn: Some(rejecting),
            confine: None,
            ..spec(std::env::temp_dir())
        })
        .await
        .is_err()
    );
    assert_eq!(rt.live(), 0);
    let kinds: Vec<CallKind> = rt.calls().iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            CallKind::Open,
            CallKind::Open,
            CallKind::Close,
            CallKind::Resume,
            CallKind::Close,
            CallKind::Open
        ]
    );
}

/// Regression from the real adapters: the stream ending means the next turn is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn back_to_back_turns() {
    let rt = FakeRuntime::new("claude", (0..50).map(|i| Turn::completed(i.to_string())));
    let mut s = rt.open(spec(std::env::temp_dir())).await.unwrap();
    for i in 0..50 {
        let mut rx = s
            .turn(TurnInput::text("go"), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("turn {i}: {e}"));
        assert_eq!(text(&last(&mut rx).await), i.to_string());
    }
    s.close().await.unwrap();
}

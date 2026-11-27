//! Ported from `internal/runtime/codex/session_test.go`: the Codex adapter against a
//! scripted app server (`src/bin/fake-codex.rs`).
//!
//! Dropped: the missing-approver and invalid-effort cases of `TestOpenValidation` (both
//! are enforced by types), and the idempotency half of `TestCloseKillsProcess` (close
//! consumes the session).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openchef_core::{Effort, PermissionMode};
use openchef_procs::{Identity, alive};
use openchef_runtime::codex::{CodexOptions, CodexRuntime, CodexSession};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Error, Event, EventBody,
    Failure, FailureKind, McpServer, OnSpawn, Question, RequestKind, Runtime as _, Session as _,
    SessionRef, SessionSpec, TurnEvents, TurnInput, TurnResult,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const DRAIN: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Recording {
    decision: Option<Decision>,
    error: Option<String>,
    approvals: Vec<ApprovalRequest>,
    questions: Vec<Question>,
}

#[derive(Clone, Default)]
struct RecordingApprover(Arc<Mutex<Recording>>);

impl Approver for RecordingApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let mut r = self.0.lock().unwrap();
        r.approvals.push(request);
        let out = match &r.error {
            Some(e) => Err(ApproverError(e.clone())),
            None => Ok(r.decision.clone().unwrap_or_else(Decision::allow)),
        };
        Box::pin(async move { out })
    }

    fn answer(
        &self,
        q: Question,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        self.0.lock().unwrap().questions.push(q);
        Box::pin(async { Ok("teal".to_owned()) })
    }
}

struct Harness {
    runtime: CodexRuntime,
    spec: SessionSpec,
    log: PathBuf,
    approver: RecordingApprover,
    _dir: tempfile::TempDir,
}

fn harness(scenario: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("fake-requests.jsonl");
    let approver = RecordingApprover::default();
    let mut env: Vec<(String, String)> = vec![
        ("OPENCHEF_FAKE_CODEX".into(), scenario.into()),
        ("OPENCHEF_FAKE_LOG".into(), log.to_string_lossy().into()),
    ];
    for k in ["TMPDIR", "SYSTEMROOT"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    let spec = SessionSpec {
        dir: dir.path().to_path_buf(),
        model: Some("gpt-test".into()),
        effort: Some(Effort::Max),
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env,
        approver: Arc::new(approver.clone()),
        ask_user: None,
        log_path: Some(dir.path().join("raw.jsonl")),
        on_spawn: None,
    };
    let runtime = CodexRuntime::new(CodexOptions {
        binary: env!("CARGO_BIN_EXE_fake-codex").into(),
        args: Vec::new(),
        ..CodexOptions::default()
    });
    Harness {
        runtime,
        spec,
        log,
        approver,
        _dir: dir,
    }
}

impl Harness {
    async fn open(&self) -> CodexSession {
        self.runtime.open(self.spec.clone()).await.unwrap()
    }

    /// The params of every request of `method` the fake received.
    fn requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|m| m["method"] == method)
            .map(|m| m["params"].clone())
            .collect()
    }
}

async fn drain(rx: &mut TurnEvents) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + DRAIN;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(e)) => events.push(e),
            Ok(None) => return events,
            Err(elapsed) => panic!("turn did not finish ({elapsed}); events: {events:?}"),
        }
    }
}

fn terminal(events: &[Event]) -> &EventBody {
    let last = events.last().expect("no events");
    assert!(
        last.is_terminal() && events[..events.len() - 1].iter().all(|e| !e.is_terminal()),
        "{events:?}"
    );
    &last.body
}

async fn turn(s: &mut CodexSession, input: TurnInput) -> (EventBody, Vec<Event>) {
    let mut rx = s.turn(input, CancellationToken::new()).await.unwrap();
    let events = drain(&mut rx).await;
    (terminal(&events).clone(), events)
}

fn completed(body: &EventBody) -> TurnResult {
    match body {
        EventBody::TurnCompleted(r) => r.clone(),
        other => panic!("not completed: {other:?}"),
    }
}

fn failed(body: &EventBody) -> Failure {
    match body {
        EventBody::TurnFailed(f) => f.clone(),
        other => panic!("not failed: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn open_turn_and_params() {
    let h = harness("ok");
    let mut s = h.open().await;
    assert_eq!(
        s.session_ref(),
        &SessionRef {
            runtime: "codex".into(),
            id: "thr-1".into()
        }
    );
    let (end, events) = turn(&mut s, TurnInput::text("hi")).await;
    let r = completed(&end);
    assert_eq!(r.final_text, "Hello");
    assert_eq!(
        (
            r.usage.input_tokens,
            r.usage.cache_read_tokens,
            r.usage.output_tokens
        ),
        (60, 40, 10),
        "turn delta, cache split out"
    );
    assert_eq!(r.duration, Duration::from_millis(42));
    let text: String = events
        .iter()
        .filter_map(|e| {
            if let EventBody::Text(t) = &e.body {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "Hello");
    let quota = events
        .iter()
        .find_map(|e| {
            if let EventBody::Quota(q) = &e.body {
                Some(q.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        quota.windows.len(),
        2,
        "the sparse update merges into the snapshot"
    );
    assert_eq!(
        (
            quota.windows[0].name.as_str(),
            quota.windows[1].name.as_str()
        ),
        ("five_hour", "weekly")
    );
    assert!((quota.windows[0].utilization - 0.12).abs() < 1e-9);
    let start = &h.requests("thread/start")[0];
    assert_eq!(start["approvalPolicy"], "untrusted");
    assert_eq!(start["sandbox"], "workspace-write");
    assert_eq!(start["model"], "gpt-test");
    assert_eq!(start["cwd"], h.spec.dir.to_string_lossy().as_ref());
    assert_eq!(
        h.requests("turn/start")[0]["effort"],
        "high",
        "max maps to the highest supported normalized level"
    );
    assert!(
        h.spec.log_path.as_ref().unwrap().exists(),
        "raw log not written"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_output() {
    let h = harness("ok");
    let mut s = h.open().await;
    let schema = json!({"type": "object"});
    let (end, _) = turn(
        &mut s,
        TurnInput {
            text: "plan".into(),
            output_schema: Some(schema.clone()),
        },
    )
    .await;
    assert_eq!(completed(&end).structured, Some(json!({"ok": true})));
    assert_eq!(h.requests("turn/start")[0]["outputSchema"], schema);
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_uses_read_only_sandbox() {
    let mut h = harness("ok");
    h.spec.mode = PermissionMode::Plan;
    h.open().await.close().await.unwrap();
    assert_eq!(h.requests("thread/start")[0]["sandbox"], "read-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_is_routed_and_denial_counted() {
    let h = harness("approve");
    h.approver.0.lock().unwrap().decision = Some(Decision::deny("not in the plan"));
    let mut s = h.open().await;
    let r = completed(&turn(&mut s, TurnInput::text("clean")).await.0);
    assert_eq!((r.final_text.as_str(), r.denials), ("decision:decline", 1));
    let approvals = h.approver.0.lock().unwrap().approvals.clone();
    assert_eq!(approvals.len(), 1);
    let a = &approvals[0];
    assert!(
        a.kind == RequestKind::Command
            && a.tool == "Bash"
            && a.command.as_deref() == Some("rm -rf build"),
        "{a:?}"
    );
    assert!(
        a.description == "cleanup" && a.session.id == "thr-1" && a.paths == ["/w"],
        "{a:?}"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_for_session_and_approver_error() {
    let h = harness("approve");
    h.approver.0.lock().unwrap().decision = Some(Decision {
        for_session: true,
        ..Decision::allow()
    });
    let mut s = h.open().await;
    assert_eq!(
        completed(&turn(&mut s, TurnInput::text("x")).await.0).final_text,
        "decision:acceptForSession"
    );
    s.close().await.unwrap();

    let h = harness("approve");
    h.approver.0.lock().unwrap().error = Some("approval service down".into());
    let mut s = h.open().await;
    assert_eq!(
        completed(&turn(&mut s, TurnInput::text("x")).await.0).final_text,
        "decision:decline",
        "an approver error denies"
    );
    s.close().await.unwrap();
}

/// The file-change request carries the paths of the item started just before it.
#[tokio::test(flavor = "multi_thread")]
async fn file_change_approval_carries_item_paths() {
    for _ in 0..10 {
        let h = harness("editapprove");
        let mut s = h.open().await;
        assert_eq!(
            completed(&turn(&mut s, TurnInput::text("x")).await.0).final_text,
            "decision:accept"
        );
        let a = h.approver.0.lock().unwrap().approvals[0].clone();
        assert!(
            a.kind == RequestKind::FileChange && a.paths == ["/w/a.txt"],
            "{a:?}"
        );
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_is_auto_accepted_and_answered() {
    let mut h = harness("ask");
    h.spec.ask_user = Some(McpServer {
        name: "openchef".into(),
        command: "/bin/openchef".into(),
        args: vec!["mcp".into()],
        env: vec![("A".into(), "1".into())],
        tool_timeout: None,
    });
    let mut s = h.open().await;
    assert_eq!(
        completed(&turn(&mut s, TurnInput::text("ask")).await.0).final_text,
        "accept:teal"
    );
    {
        let r = h.approver.0.lock().unwrap();
        assert!(
            r.approvals.is_empty(),
            "OpenChef's own ask_user reached the approver: {:?}",
            r.approvals
        );
        assert!(
            r.questions.len() == 1 && r.questions[0].text == "Which colour?",
            "{:?}",
            r.questions
        );
    }
    let server = &h.requests("thread/start")[0]["config"]["mcp_servers"]["openchef"];
    assert_eq!(
        (
            server["command"].as_str(),
            server["args"].as_array().map(Vec::len),
            server["env"]["A"].as_str()
        ),
        (Some("/bin/openchef"), Some(1), Some("1"))
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_exhaustion() {
    let h = harness("quota");
    let mut s = h.open().await;
    let (end, events) = turn(&mut s, TurnInput::text("x")).await;
    let f = failed(&end);
    assert_eq!(f.kind, FailureKind::QuotaExhausted);
    let until = f.resets_at.unwrap() - time::OffsetDateTime::now_utc();
    assert!(
        until > time::Duration::hours(1),
        "resets_at comes from the exhausted window: {until}"
    );
    assert!(f.message.contains("usageLimitExceeded"), "{}", f.message);
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.body, EventBody::Quota(q) if q.exhausted))
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_during_turn() {
    let h = harness("crash");
    let mut s = h.open().await;
    assert_eq!(
        failed(&turn(&mut s, TurnInput::text("x")).await.0).kind,
        FailureKind::Crashed
    );
    assert!(matches!(
        s.turn(TurnInput::text("again"), CancellationToken::new())
            .await,
        Err(Error::SessionClosed)
    ));
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn context_timeout_interrupts_turn() {
    let h = harness("hang");
    let mut s = h.open().await;
    for _ in 0..3 {
        let cancel = CancellationToken::new();
        let mut rx = s.turn(TurnInput::text("x"), cancel.clone()).await.unwrap();
        cancel.cancel();
        assert_eq!(
            failed(terminal(&drain(&mut rx).await)).kind,
            FailureKind::Timeout
        );
    }
    assert!(
        !h.requests("turn/interrupt").is_empty(),
        "turn/interrupt was not sent"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_turn_while_running() {
    let h = harness("hang");
    let mut s = h.open().await;
    let mut rx = s
        .turn(TurnInput::text("x"), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        s.turn(TurnInput::text("y"), CancellationToken::new()).await,
        Err(Error::TurnInProgress)
    ));
    s.interrupt().await.unwrap();
    assert_eq!(
        failed(terminal(&drain(&mut rx).await)).kind,
        FailureKind::Interrupted
    );
    let _next = s
        .turn(TurnInput::text("z"), CancellationToken::new())
        .await
        .unwrap();
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume() {
    let h = harness("ok");
    let old = SessionRef {
        runtime: "codex".into(),
        id: "thr-old".into(),
    };
    let mut s = h.runtime.resume(old.clone(), h.spec.clone()).await.unwrap();
    assert_eq!(s.session_ref(), &old);
    let p = &h.requests("thread/resume")[0];
    assert!(
        p["threadId"] == "thr-old" && p["excludeTurns"] == true,
        "{p}"
    );
    completed(&turn(&mut s, TurnInput::text("x")).await.0);
    s.close().await.unwrap();
    assert!(
        h.runtime
            .resume(
                SessionRef {
                    runtime: "claude".into(),
                    id: "x".into()
                },
                h.spec.clone()
            )
            .await
            .is_err()
    );
}

/// Regression: a consumer that stops reading must not be able to wedge close.
#[tokio::test(flavor = "multi_thread")]
async fn close_with_unread_events_does_not_block() {
    let h = harness("flood");
    let mut s = h.open().await;
    let _rx = s
        .turn(TurnInput::text("x"), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(15), s.close())
        .await
        .expect("close blocked while events were unread")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn close_kills_process() {
    let mut h = harness("ok");
    let ids: Arc<Mutex<Vec<Identity>>> = Arc::default();
    let hook: OnSpawn = {
        let ids = Arc::clone(&ids);
        Arc::new(move |id| {
            ids.lock().unwrap().push(id);
            Box::pin(async { Ok(()) })
        })
    };
    h.spec.on_spawn = Some(hook);
    let s = h.open().await;
    let id = ids.lock().unwrap()[0];
    s.close().await.unwrap();
    assert!(!alive(&id).unwrap(), "app-server still alive after close");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_validation() {
    let h = harness("ok");
    let bad = CodexRuntime::new(CodexOptions {
        binary: h.spec.dir.join("no-codex"),
        ..CodexOptions::default()
    });
    assert!(
        bad.open(h.spec.clone()).await.is_err(),
        "missing binary must fail"
    );
    let rejecting: OnSpawn = Arc::new(|_| Box::pin(async { Err("not in job".to_owned()) }));
    assert!(
        h.runtime
            .open(SessionSpec {
                on_spawn: Some(rejecting),
                ..h.spec.clone()
            })
            .await
            .is_err()
    );
}

/// Regression (found live): notifications of a previous turn that arrive before the
/// new turn's id is known are not attributed to the new turn.
#[tokio::test(flavor = "multi_thread")]
async fn stale_notifications_are_ignored() {
    for _ in 0..20 {
        let h = harness("stale");
        let mut s = h.open().await;
        let r = completed(&turn(&mut s, TurnInput::text("x")).await.0);
        assert_eq!(
            r.final_text, "Hello",
            "stale completion leaked into the new turn"
        );
        assert_eq!(
            (r.usage.input_tokens, r.usage.output_tokens),
            (60, 10),
            "stale usage leaked"
        );
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn immediate_failure_before_turn_started() {
    for _ in 0..20 {
        let h = harness("fastfail");
        let mut s = h.open().await;
        assert_eq!(
            failed(&turn(&mut s, TurnInput::text("x")).await.0).kind,
            FailureKind::Auth
        );
        s.close().await.unwrap();
    }
}

/// Regression: the stream ending means the session accepts the next turn.
#[tokio::test(flavor = "multi_thread")]
async fn back_to_back_turns() {
    let h = harness("ok");
    let mut s = h.open().await;
    let start = Instant::now();
    for i in 0..30 {
        let mut rx = s
            .turn(TurnInput::text("x"), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("turn {i}: {e}"));
        let _ = drain(&mut rx).await;
    }
    assert!(start.elapsed() < DRAIN);
    s.close().await.unwrap();
}

//! Ported from `internal/runtime/claude/session_test.go`: the Claude adapter against a
//! scripted fake CLI (`src/bin/fake-claude.rs`).
//!
//! Dropped: "Turn after Close returns `ErrSessionClosed`" (close consumes the session, so
//! the call does not compile) and the bad-effort/bad-mode cases of `TestOpenValidation`
//! (both are enums).
// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
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
use openchef_runtime::claude::{ClaudeOptions, ClaudeRuntime, ClaudeSession};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Error, Event, EventBody,
    FailureKind, McpServer, OnSpawn, Question, RequestKind, Runtime as _, Session as _, SessionRef,
    SessionSpec, TurnEvents, TurnInput,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const INTERRUPT_WAIT: Duration = Duration::from_millis(1500);
const CLOSE_WAIT: Duration = Duration::from_secs(1);
const DRAIN: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Recording {
    decision: Option<Decision>,
    error: Option<String>,
    block: bool,
    started_marker: Option<PathBuf>,
    approvals: Vec<ApprovalRequest>,
    cancelled: bool,
}

#[derive(Clone, Default)]
struct RecordingApprover(Arc<Mutex<Recording>>);

impl Approver for RecordingApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let me = self.clone();
        Box::pin(async move {
            let (block, marker, decision, error) = {
                let mut r = me.0.lock().unwrap();
                r.approvals.push(request);
                (
                    r.block,
                    r.started_marker.clone(),
                    r.decision.clone(),
                    r.error.clone(),
                )
            };
            if block {
                if let Some(m) = marker {
                    std::fs::write(m, b"").unwrap(); // the fake polls for it
                }
                cancel.cancelled().await;
                me.0.lock().unwrap().cancelled = true;
                return Err(ApproverError("cancelled".into()));
            }
            match error {
                Some(e) => Err(ApproverError(e)),
                None => Ok(decision.unwrap_or_else(Decision::allow)),
            }
        })
    }

    fn answer(
        &self,
        _: Question,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        Box::pin(async { Ok("teal".into()) })
    }
}

/// Records every process an adapter reports and checks it was alive at the time.
#[derive(Clone, Default)]
struct SpawnRecorder(Arc<Mutex<(Vec<Identity>, Vec<String>)>>);

impl SpawnRecorder {
    fn hook(&self) -> OnSpawn {
        let me = self.clone();
        Arc::new(move |id| {
            let me = me.clone();
            Box::pin(async move {
                let mut r = me.0.lock().unwrap();
                if !alive(&id).unwrap_or(false) {
                    r.1.push(format!("pid {} not alive at on_spawn", id.pid));
                }
                r.0.push(id);
                Ok(())
            })
        })
    }

    fn ids(&self) -> Vec<Identity> {
        self.0.lock().unwrap().0.clone()
    }
}

struct Harness {
    runtime: ClaudeRuntime,
    spec: SessionSpec,
    log: PathBuf,
    approver: RecordingApprover,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

fn harness(scenario: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("transcripts");
    std::fs::create_dir(&state).unwrap();
    let log = dir.path().join("fake.jsonl");
    let approver = RecordingApprover::default();
    let mut env: Vec<(String, String)> = vec![
        ("OPENCHEF_FAKE_CLAUDE".into(), scenario.into()),
        ("OPENCHEF_FAKE_LOG".into(), log.to_string_lossy().into()),
        ("OPENCHEF_FAKE_STATE".into(), state.to_string_lossy().into()),
    ];
    for k in ["TMPDIR", "SYSTEMROOT"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    let spec = SessionSpec {
        dir: dir.path().to_path_buf(),
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
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
    let runtime = ClaudeRuntime::new(ClaudeOptions {
        binary: env!("CARGO_BIN_EXE_fake-claude").into(),
        interrupt_wait: INTERRUPT_WAIT,
        close_wait: CLOSE_WAIT,
        ..ClaudeOptions::default()
    });
    Harness {
        runtime,
        spec,
        log,
        approver,
        state,
        _dir: dir,
    }
}

impl Harness {
    async fn open(&self) -> ClaudeSession {
        self.runtime.open(self.spec.clone()).await.unwrap()
    }

    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn launches(&self) -> Vec<Vec<String>> {
        self.records()
            .iter()
            .filter_map(|r| {
                r["argv"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x.as_str().unwrap().to_owned()).collect())
            })
            .collect()
    }

    fn control_responses(&self) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter_map(|r| (r["in"]["type"] == "control_response").then(|| r["in"].clone()))
            .collect()
    }
}

fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .cloned()
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

/// Waits until the turn streams text (Claude picked up the message).
async fn started(rx: &mut TurnEvents) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + DRAIN;
    loop {
        let e = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("turn never started")
            .expect("stream ended");
        let text = matches!(e.body, EventBody::Text(_));
        events.push(e);
        if text {
            return events;
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

async fn turn(s: &mut ClaudeSession, text: &str) -> Vec<Event> {
    let mut rx = s
        .turn(TurnInput::text(text), CancellationToken::new())
        .await
        .unwrap();
    drain(&mut rx).await
}

fn must_complete(body: &EventBody, text: &str) -> openchef_runtime::TurnResult {
    match body {
        EventBody::TurnCompleted(r) if r.final_text == text => r.clone(),
        other => panic!("want completed {text:?}, got {other:?}"),
    }
}

fn must_fail(body: &EventBody, kind: FailureKind) -> openchef_runtime::Failure {
    match body {
        EventBody::TurnFailed(f) if f.kind == kind => f.clone(),
        other => panic!("want {kind:?}, got {other:?}"),
    }
}

fn kinds(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e.body {
            EventBody::Text(_) => "text",
            EventBody::ToolCall(_) => "tool_call",
            EventBody::ToolResult(_) => "tool_result",
            EventBody::Quota(_) => "quota",
            EventBody::TurnCompleted(_) => "completed",
            EventBody::TurnFailed(_) => "failed",
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn open_turn_events_and_args() {
    let h = harness("1");
    let mut s = h.open().await;
    let id = s.session_ref().id.clone();
    assert_eq!((s.session_ref().runtime.as_str(), id.len()), ("claude", 36));
    let events = turn(&mut s, "ok").await;
    let r = must_complete(terminal(&events), "Hello");
    assert_eq!(
        (
            r.usage.input_tokens,
            r.usage.output_tokens,
            r.usage.cache_read_tokens,
            r.usage.cache_write_tokens
        ),
        (100, 10, 40, 5)
    );
    assert_eq!(r.usage.cost_usd, Some(0.001));
    assert_eq!(
        (r.models.as_slice(), r.duration),
        (
            &["claude-haiku-5-5".to_owned()][..],
            Duration::from_millis(42)
        )
    );
    // Duplicate and subagent tool calls are dropped.
    assert_eq!(
        kinds(&events),
        [
            "quota",
            "text",
            "text",
            "tool_call",
            "tool_result",
            "completed"
        ]
    );
    match (&events[3].body, &events[4].body) {
        (EventBody::ToolCall(c), EventBody::ToolResult(r)) => {
            assert!(
                c.name == "Read" && c.id == "t1" && c.signature.starts_with("Read:"),
                "{c:?}"
            );
            assert!(r.id == "t1" && r.output == "file body", "{r:?}");
        }
        other => panic!("{other:?}"),
    }
    let argv = &h.launches()[0];
    assert_eq!(flag(argv, "--session-id"), Some(id));
    assert!(argv.iter().any(|a| a == "--restricted"));
    // The cached quota signal opens the next turn.
    let events = turn(&mut s, "ok").await;
    assert_eq!(kinds(&events)[0], "quota");
    assert_eq!(h.launches().len(), 1, "two plain turns, one process");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_output_respawns_and_back() {
    let h = harness("1");
    let mut s = h.open().await;
    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
    let mut rx = s
        .turn(
            TurnInput {
                text: "ok".into(),
                output_schema: Some(schema.clone()),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain(&mut rx).await;
    let r = must_complete(terminal(&events), r#"{"ok":true}"#);
    assert_eq!(r.structured, Some(json!({"ok": true})));
    assert!(
        events.iter().all(|e| match &e.body {
            EventBody::ToolCall(c) => c.name != "StructuredOutput",
            EventBody::ToolResult(r) => r.id != "so1",
            _ => true,
        }),
        "StructuredOutput leaked"
    );
    must_complete(terminal(&turn(&mut s, "ok").await), "Hello");
    let l = h.launches();
    assert_eq!(l.len(), 3, "open, schema, back without");
    // No transcript existed before the first result: the schema process reuses
    // --session-id; afterwards --resume (D-013).
    assert!(flag(&l[1], "--session-id").is_some(), "{:?}", l[1]);
    assert_eq!(flag(&l[1], "--json-schema"), Some(schema.to_string()));
    assert!(
        flag(&l[2], "--resume").is_some() && flag(&l[2], "--json-schema").is_none(),
        "{:?}",
        l[2]
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_routed_and_denial_counted() {
    let h = harness("1");
    h.approver.0.lock().unwrap().decision = Some(Decision::deny("no rm"));
    let mut s = h.open().await;
    let r = must_complete(terminal(&turn(&mut s, "perm").await), "decided deny");
    assert_eq!(r.denials, 1);
    let a = h.approver.0.lock().unwrap().approvals.clone();
    assert_eq!(a.len(), 1);
    assert!(
        a[0].kind == RequestKind::Command
            && a[0].command.as_deref() == Some("rm -rf build")
            && a[0].session.runtime == "claude"
    );
    assert!(a[0].paths.iter().any(|p| p == "/work/x.txt"));
    let resp = h.control_responses();
    assert!(
        resp.len() == 1
            && resp[0]["response"]["response"]["behavior"] == "deny"
            && resp[0].to_string().contains("no rm"),
        "{resp:?}"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn approver_error_denies() {
    let h = harness("1");
    h.approver.0.lock().unwrap().error = Some("evaluator down".into());
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "perm").await), "decided deny");
    let resp = h.control_responses();
    assert!(
        resp.len() == 1 && resp[0].to_string().contains("evaluator down"),
        "{resp:?}"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_allowed_without_approver() {
    let mut h = harness("1");
    h.spec.ask_user = Some(McpServer {
        name: "openchef".into(),
        command: "openchef".into(),
        args: vec!["mcp".into()],
        env: Vec::new(),
        tool_timeout: None,
    });
    h.approver.0.lock().unwrap().decision = Some(Decision::deny("must not be asked"));
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "askuser").await), "decided allow");
    assert!(
        h.approver.0.lock().unwrap().approvals.is_empty(),
        "ask_user went to the approver"
    );
    let config = flag(&h.launches()[0], "--mcp-config").unwrap();
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("\"openchef\"")
    );
    s.close().await.unwrap();
    assert!(
        !std::path::Path::new(&config).exists(),
        "mcp config left behind"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_outside_turn_is_denied() {
    let h = harness("permidle");
    let s = h.open().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.control_responses().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let resp = h.control_responses();
    assert!(
        resp.len() == 1 && resp[0]["response"]["response"]["behavior"] == "deny",
        "{resp:?}"
    );
    assert!(
        h.approver.0.lock().unwrap().approvals.is_empty(),
        "approver consulted outside a turn"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_cancelled_by_claude() {
    let h = harness("1");
    {
        let mut r = h.approver.0.lock().unwrap();
        r.block = true;
        r.started_marker = Some(h.state.join("approver-started"));
    }
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "cancelperm").await), "cancelled");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.approver.0.lock().unwrap().cancelled {
        assert!(
            Instant::now() < deadline,
            "control_cancel_request did not cancel the approver"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_keeps_process() {
    let h = harness("1");
    let mut s = h.open().await;
    let mut rx = s
        .turn(TurnInput::text("hang"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = started(&mut rx).await;
    s.interrupt().await.unwrap();
    events.extend(drain(&mut rx).await);
    must_fail(terminal(&events), FailureKind::Interrupted);
    must_complete(terminal(&turn(&mut s, "ok").await), "Hello");
    assert_eq!(h.launches().len(), 1, "a clean interrupt keeps the process");
    s.interrupt().await.unwrap(); // idle: no-op
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stubborn_interrupt_kills_and_resumes() {
    let mut h = harness("1");
    let rec = SpawnRecorder::default();
    h.spec.on_spawn = Some(rec.hook());
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "remember:KESTREL").await), "Hello");
    let mut rx = s
        .turn(TurnInput::text("stubborn"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = started(&mut rx).await;
    let start = Instant::now();
    s.interrupt().await.unwrap();
    assert!(
        start.elapsed() < INTERRUPT_WAIT + Duration::from_secs(2),
        "interrupt took {:?}",
        start.elapsed()
    );
    events.extend(drain(&mut rx).await);
    must_fail(terminal(&events), FailureKind::Interrupted);
    must_complete(terminal(&turn(&mut s, "recall").await), "recalled KESTREL");
    let l = h.launches();
    assert_eq!(l.len(), 2);
    assert_eq!(
        flag(&l[1], "--resume"),
        Some(s.session_ref().id.clone()),
        "replacement process"
    );
    let ids = rec.ids();
    assert!(
        ids.len() == 2 && ids[0].pid != ids[1].pid,
        "on_spawn: {ids:?}"
    );
    assert!(
        rec.0.lock().unwrap().1.is_empty(),
        "{:?}",
        rec.0.lock().unwrap().1
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_of_queued_message_replaces_process() {
    let h = harness("1");
    let mut s = h.open().await;
    let mut rx = s
        .turn(TurnInput::text("queued"), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await; // let the fake take the message
    s.interrupt().await.unwrap();
    // The bound covers a slow CI runner; the point is not waiting for INTERRUPT_WAIT's
    // owed result, which never comes for a still-queued message (D-012).
    must_fail(terminal(&drain(&mut rx).await), FailureKind::Interrupted);
    must_complete(terminal(&turn(&mut s, "ok").await), "Hello");
    assert_eq!(h.launches().len(), 2, "want a replacement process");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_turn_reports_timeout() {
    for _ in 0..5 {
        // our deadline races with Claude's abort result
        let h = harness("1");
        let mut s = h.open().await;
        let cancel = CancellationToken::new();
        let mut rx = s
            .turn(TurnInput::text("hang"), cancel.clone())
            .await
            .unwrap();
        let _ = started(&mut rx).await;
        cancel.cancel();
        must_fail(terminal(&drain(&mut rx).await), FailureKind::Timeout);
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_then_resume() {
    let h = harness("1");
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "remember:OSPREY").await), "Hello");
    let f = must_fail(terminal(&turn(&mut s, "crash").await), FailureKind::Crashed);
    assert!(
        f.message.contains("boom"),
        "stderr not in the failure: {:?}",
        f.message
    );
    must_complete(terminal(&turn(&mut s, "recall").await), "recalled OSPREY");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_on_first_turn_recovers_with_fallback() {
    // The fake writes the transcript before crashing, but no result arrived: --session-id
    // fails ("already in use") and the fallback --resume takes over.
    let h = harness("1");
    let mut s = h.open().await;
    must_fail(terminal(&turn(&mut s, "crash").await), FailureKind::Crashed);
    must_complete(terminal(&turn(&mut s, "ok").await), "Hello");
    let l = h.launches();
    assert_eq!(l.len(), 3, "open + failed session-id + resume");
    assert!(flag(&l[2], "--resume").is_some(), "{:?}", l[2]);
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_result_ignored() {
    for _ in 0..10 {
        let h = harness("1");
        let mut s = h.open().await;
        must_complete(terminal(&turn(&mut s, "stale").await), "Hello");
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_exhausted() {
    let h = harness("1");
    let mut s = h.open().await;
    let events = turn(&mut s, "exhausted").await;
    let f = must_fail(terminal(&events), FailureKind::QuotaExhausted);
    assert!(f.resets_at.is_some());
    assert!(matches!(&events[0].body, EventBody::Quota(q) if q.exhausted));
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_turn_while_running() {
    let h = harness("1");
    let mut s = h.open().await;
    let mut rx = s
        .turn(TurnInput::text("hang"), CancellationToken::new())
        .await
        .unwrap();
    let _ = started(&mut rx).await;
    assert!(matches!(
        s.turn(TurnInput::text("ok"), CancellationToken::new())
            .await,
        Err(Error::TurnInProgress)
    ));
    s.interrupt().await.unwrap();
    let _ = drain(&mut rx).await;
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_across_sessions() {
    let h = harness("1");
    let mut s = h.open().await;
    must_complete(terminal(&turn(&mut s, "remember:HERON").await), "Hello");
    let reference = s.session_ref().clone();
    s.close().await.unwrap();
    let mut s2 = h
        .runtime
        .resume(reference.clone(), h.spec.clone())
        .await
        .unwrap();
    assert_eq!(s2.session_ref(), &reference);
    must_complete(terminal(&turn(&mut s2, "recall").await), "recalled HERON");
    assert_eq!(flag(&h.launches()[1], "--resume"), Some(reference.id));
    s2.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_unknown_session_fails() {
    let h = harness("1");
    let unknown = SessionRef {
        runtime: "claude".into(),
        id: "6f1c1c3e-0000-4000-8000-000000000000".into(),
    };
    let err = h.runtime.resume(unknown, h.spec.clone()).await.unwrap_err();
    assert!(err.to_string().contains("No conversation found"), "{err}");
    assert!(
        h.runtime
            .resume(
                SessionRef {
                    runtime: "codex".into(),
                    id: "x".into()
                },
                h.spec.clone()
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn close_idle_kills_process() {
    let mut h = harness("1");
    let rec = SpawnRecorder::default();
    h.spec.on_spawn = Some(rec.hook());
    let s = h.open().await;
    let id = rec.ids()[0];
    s.close().await.unwrap();
    assert!(!alive(&id).unwrap(), "claude still alive after close");
}

#[tokio::test(flavor = "multi_thread")]
async fn close_in_flight_is_fast() {
    let mut h = harness("1");
    let rec = SpawnRecorder::default();
    h.spec.on_spawn = Some(rec.hook());
    let mut s = h.open().await;
    let id = rec.ids()[0];
    let mut rx = s
        .turn(TurnInput::text("stubborn"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = started(&mut rx).await;
    let start = Instant::now();
    s.close().await.unwrap();
    assert!(
        start.elapsed() < CLOSE_WAIT * 2 + Duration::from_secs(2),
        "close took {:?} (D-010)",
        start.elapsed()
    );
    events.extend(drain(&mut rx).await);
    must_fail(terminal(&events), FailureKind::Interrupted);
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(&id).unwrap() {
        assert!(Instant::now() < deadline, "process survived close");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn close_with_unread_events_does_not_block() {
    let h = harness("1");
    let mut s = h.open().await;
    let _rx = s
        .turn(TurnInput::text("flood"), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // let the flood fill every buffer
    tokio::time::timeout(Duration::from_secs(15), s.close())
        .await
        .expect("close blocked while events were unread")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_validation() {
    let h = harness("1");
    let missing = SessionSpec {
        dir: PathBuf::from("/definitely/not/here"),
        ..h.spec.clone()
    };
    assert!(
        h.runtime.open(missing).await.is_err(),
        "missing working directory"
    );
    let bad = ClaudeRuntime::new(ClaudeOptions {
        binary: h.state.join("no-claude"),
        ..ClaudeOptions::default()
    });
    assert!(
        bad.open(h.spec.clone()).await.is_err(),
        "missing binary must fail"
    );
    assert_eq!(ClaudeRuntime::default().name(), "claude");
}

/// Regression: the stream ending must mean the session accepts the next turn.
#[tokio::test(flavor = "multi_thread")]
async fn back_to_back_turns() {
    let h = harness("1");
    let mut s = h.open().await;
    for i in 0..30 {
        let mut rx = s
            .turn(TurnInput::text("ok"), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("turn {i}: {e}"));
        let _ = drain(&mut rx).await;
    }
    s.close().await.unwrap();
}

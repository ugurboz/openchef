//! Ported from `internal/runtime/agy/session_test.go`: the agy adapter against a
//! scripted ACP server (`src/bin/fake-agy.rs`).
//!
//! Dropped: the no-approver, bad-effort and bad-mode cases of `TestOpenValidation`
//! (enforced by types) and the idempotency half of `TestCloseIdleKillsProcess` (close
//! consumes the session). `TestConcurrentTurnsStartOneProcess` runs the calls one after
//! another: `turn` takes `&mut self`, and the actor serializes them anyway.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
    clippy::many_single_char_names
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openchef_core::{Effort, PermissionMode};
use openchef_procs::{Identity, alive};
use openchef_runtime::agy::{AgyOptions, AgyRuntime, AgySession};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Error, Event, EventBody,
    Failure, FailureKind, McpServer, OnSpawn, Question, RequestKind, Runtime as _, Session as _,
    SessionRef, SessionSpec, TurnEvents, TurnInput, TurnResult,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const INTERRUPT_WAIT: Duration = Duration::from_millis(1500);
const CLOSE_WAIT: Duration = Duration::from_secs(1);
const DRAIN: Duration = Duration::from_secs(45);

#[derive(Default)]
struct Recording {
    decision: Option<Decision>,
    error: Option<String>,
    block: bool,
    cancelled: bool,
    approvals: Vec<ApprovalRequest>,
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
            let (block, decision, error) = {
                let mut r = me.0.lock().unwrap();
                r.approvals.push(request);
                (r.block, r.decision.clone(), r.error.clone())
            };
            if block {
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
        Box::pin(async { Ok(String::new()) })
    }
}

impl RecordingApprover {
    fn list(&self) -> Vec<ApprovalRequest> {
        self.0.lock().unwrap().approvals.clone()
    }
}

struct Harness {
    runtime: AgyRuntime,
    spec: SessionSpec,
    log: PathBuf,
    settings: PathBuf,
    approver: RecordingApprover,
    _dir: tempfile::TempDir,
}

fn options(settings: PathBuf) -> AgyOptions {
    AgyOptions {
        binary: env!("CARGO_BIN_EXE_fake-agy").into(),
        args: Vec::new(),
        settings_dir: Some(settings),
        allow_temp_dir: true,
        interrupt_wait: INTERRUPT_WAIT,
        close_wait: CLOSE_WAIT,
        ..AgyOptions::default()
    }
}

fn harness(scenario: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("sessions");
    std::fs::create_dir(&state).unwrap();
    let log = dir.path().join("fake.jsonl");
    let settings = dir.path().join("agy-settings");
    let approver = RecordingApprover::default();
    let mut env: Vec<(String, String)> = vec![
        ("OPENCHEF_FAKE_AGY".into(), scenario.into()),
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
        model: Some("gemini-big".into()),
        effort: Some(Effort::Low),
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env,
        approver: Arc::new(approver.clone()),
        ask_user: Some(McpServer {
            name: "openchef".into(),
            command: "openchef".into(),
            args: vec!["mcp".into()],
            env: Vec::new(),
            tool_timeout: None,
        }),
        log_path: Some(dir.path().join("raw.jsonl")),
        on_spawn: None,
    };
    Harness {
        runtime: AgyRuntime::new(options(settings.clone())),
        spec,
        log,
        settings,
        approver,
        _dir: dir,
    }
}

impl Harness {
    async fn open(&self) -> AgySession {
        self.runtime.open(self.spec.clone()).await.unwrap()
    }

    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn launches(&self) -> usize {
        self.records()
            .iter()
            .filter(|r| !r["argv"].is_null())
            .count()
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter(|r| r["in"]["method"] == method)
            .map(|r| r["in"]["params"].clone())
            .collect()
    }

    /// Our replies to the fake's permission requests.
    fn answers(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter(|r| r["in"]["method"].is_null() && !r["in"]["result"].is_null())
            .map(|r| {
                let o = &r["in"]["result"]["outcome"];
                if o["outcome"] == "cancelled" {
                    "cancelled".to_owned()
                } else {
                    o["optionId"].as_str().unwrap_or("").to_owned()
                }
            })
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

async fn started(rx: &mut TurnEvents) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let e = tokio::time::timeout(DRAIN, rx.recv())
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

fn terminal(events: &[Event]) -> EventBody {
    let last = events.last().expect("no events");
    assert!(
        last.is_terminal() && events[..events.len() - 1].iter().all(|e| !e.is_terminal()),
        "{events:?}"
    );
    last.body.clone()
}

async fn turn(s: &mut AgySession, input: TurnInput) -> (EventBody, Vec<Event>) {
    let mut rx = s.turn(input, CancellationToken::new()).await.unwrap();
    let events = drain(&mut rx).await;
    (terminal(&events), events)
}

async fn text_turn(s: &mut AgySession, text: &str) -> EventBody {
    turn(s, TurnInput::text(text)).await.0
}

fn must_complete(body: &EventBody, text: &str) -> TurnResult {
    match body {
        EventBody::TurnCompleted(r) if r.final_text == text => r.clone(),
        other => panic!("want completed {text:?}, got {other:?}"),
    }
}

fn must_fail(body: &EventBody, kind: FailureKind) -> Failure {
    match body {
        EventBody::TurnFailed(f) if f.kind == kind => f.clone(),
        other => panic!("want {kind:?}, got {other:?}"),
    }
}

fn spawn_recorder() -> (OnSpawn, Arc<Mutex<Vec<Identity>>>) {
    let ids: Arc<Mutex<Vec<Identity>>> = Arc::default();
    let hook: OnSpawn = {
        let ids = Arc::clone(&ids);
        Arc::new(move |id| {
            ids.lock().unwrap().push(id);
            Box::pin(async { Ok(()) })
        })
    };
    (hook, ids)
}

#[tokio::test(flavor = "multi_thread")]
async fn open_turn_events_and_config() {
    let h = harness("1");
    let mut s = h.open().await;
    assert!(
        s.session_ref().runtime == "agy" && s.session_ref().id.starts_with("sess-"),
        "{:?}",
        s.session_ref()
    );
    let (end, events) = turn(&mut s, TurnInput::text("ok")).await;
    let r = must_complete(&end, "Hello"); // the text after the last tool call, not "Let me look."
    assert_eq!(
        (
            r.usage.input_tokens,
            r.usage.output_tokens,
            r.usage.cache_read_tokens
        ),
        (100, 15, 40)
    );
    assert_eq!(r.models, ["gemini-big"]);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e.body {
            EventBody::Text(_) => "text",
            EventBody::ToolCall(_) => "tool_call",
            EventBody::ToolResult(_) => "tool_result",
            EventBody::Quota(_) => "quota",
            EventBody::TurnCompleted(_) => "completed",
            EventBody::TurnFailed(_) => "failed",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "text",
            "tool_call",
            "tool_result",
            "text",
            "text",
            "completed"
        ]
    );
    match (&events[1].body, &events[2].body) {
        (EventBody::ToolCall(c), EventBody::ToolResult(t)) => {
            assert!(
                c.name == "Bash" && c.id == "c1" && c.signature.contains("cat a.txt"),
                "{c:?}"
            );
            assert!(t.output == "file body" && !t.is_error, "{t:?}");
        }
        other => panic!("{other:?}"),
    }
    let records = h.records();
    let argv: Vec<&str> = records[0]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for f in [
        "--sandbox",
        "--disable-telemetry",
        "--include-default-customizations=false",
        "--cli_dir_name",
    ] {
        assert!(argv.contains(&f), "missing {f} in {argv:?}");
    }
    let set: Vec<String> = h
        .requests("session/set_config_option")
        .iter()
        .map(|p| {
            format!(
                "{}={}",
                p["configId"].as_str().unwrap(),
                p["value"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        set,
        ["model=gemini-big", "thought_level=low", "mode=default"]
    );
    let new = h.requests("session/new");
    assert!(
        new.len() == 1
            && new[0]["mcpServers"][0]["name"] == "openchef"
            && new[0]["cwd"].is_string(),
        "{new:?}"
    );
    let init = h.requests("initialize");
    assert!(
        init.len() == 1 && init[0]["clientCapabilities"]["terminal"] == false,
        "{init:?}"
    );
    let settings = std::fs::read_to_string(h.settings.join("settings.json")).unwrap();
    assert!(settings.contains("proceed-in-sandbox"), "{settings}");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn effort_follows_the_models_levels() {
    let mut h = harness("1");
    h.spec.model = Some("gemini-small".into()); // offers low|medium only
    h.spec.effort = Some(Effort::Max);
    h.open().await.close().await.unwrap();
    let level = h
        .requests("session/set_config_option")
        .into_iter()
        .find(|p| p["configId"] == "thought_level")
        .map(|p| p["value"].clone());
    assert_eq!(
        level,
        Some(json!("medium")),
        "want the highest the model offers"
    );
    let mut h2 = harness("1");
    h2.spec.model = Some("nonexistent".into());
    let err = h2.runtime.open(h2.spec.clone()).await.unwrap_err();
    assert!(err.to_string().contains("unknown model"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn barrier_keeps_every_chunk() {
    for _ in 0..5 {
        let h = harness("1");
        let mut s = h.open().await;
        match text_turn(&mut s, "burst").await {
            EventBody::TurnCompleted(r) => assert_eq!(r.final_text.len(), 500, "chunks lost"),
            other => panic!("{other:?}"),
        }
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_routed() {
    let h = harness("1");
    h.approver.0.lock().unwrap().decision = Some(Decision::deny("no builds"));
    let mut s = h.open().await;
    let r = must_complete(&text_turn(&mut s, "perm").await, "decided deny");
    assert_eq!(r.denials, 1);
    let a = h.approver.list();
    assert_eq!(a.len(), 1);
    assert!(
        a[0].kind == RequestKind::Command
            && a[0].command.as_deref() == Some("go build ./...")
            && &a[0].session == s.session_ref(),
        "{a:?}"
    );
    assert!(
        a[0].description.contains("outside the agy sandbox"),
        "{a:?}"
    );
    h.approver.0.lock().unwrap().decision = Some(Decision::allow());
    must_complete(&text_turn(&mut s, "edit").await, "decided allow");
    let a = h.approver.list();
    assert!(
        a.len() == 2 && a[1].kind == RequestKind::FileChange && a[1].paths == ["/w/a.go"],
        "{a:?}"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn approver_error_denies() {
    let h = harness("1");
    h.approver.0.lock().unwrap().error = Some("evaluator down".into());
    let mut s = h.open().await;
    must_complete(&text_turn(&mut s, "perm").await, "decided deny");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_allowed_without_approver() {
    let h = harness("1");
    h.approver.0.lock().unwrap().decision = Some(Decision::deny("must not be asked"));
    let mut s = h.open().await;
    must_complete(&text_turn(&mut s, "askuser").await, "decided allow");
    assert!(
        h.approver.list().is_empty(),
        "ask_user went to the approver"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_refuses_without_asking() {
    let mut h = harness("1");
    h.spec.mode = PermissionMode::Plan;
    let mut s = h.open().await;
    for k in ["perm", "edit"] {
        must_complete(&text_turn(&mut s, k).await, "decided deny");
    }
    must_complete(&text_turn(&mut s, "askuser").await, "decided allow"); // questions still work
    assert!(
        h.approver.list().is_empty(),
        "plan mode consulted the approver"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_outside_turn_is_denied() {
    let h = harness("permidle");
    let s = h.open().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.answers().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.answers(), ["deny"]);
    assert!(
        h.approver.list().is_empty(),
        "approver consulted outside a turn"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_cancels_pending_approval() {
    let h = harness("1");
    h.approver.0.lock().unwrap().block = true;
    let mut s = h.open().await;
    let mut rx = s
        .turn(TurnInput::text("hangperm"), CancellationToken::new())
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.approver.list().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    s.interrupt().await.unwrap();
    must_fail(&terminal(&drain(&mut rx).await), FailureKind::Interrupted);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.approver.0.lock().unwrap().cancelled {
        assert!(Instant::now() < deadline, "approver token not cancelled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let outcome = h
        .records()
        .iter()
        .filter_map(|r| r["permission_outcome"].as_str().map(str::to_owned))
        .next_back();
    assert_eq!(
        outcome.as_deref(),
        Some("cancelled"),
        "pending request was not answered cancelled"
    );
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
    let f = must_fail(&terminal(&events), FailureKind::Interrupted);
    assert_eq!(
        f.usage.input_tokens, 100,
        "usage of the cancelled turn lost"
    );
    must_complete(&text_turn(&mut s, "ok").await, "Hello");
    assert_eq!(h.launches(), 1);
    let cancels: Vec<Value> = h
        .records()
        .into_iter()
        .filter(|r| r["in"]["method"] == "session/cancel")
        .map(|r| r["in"].clone())
        .collect();
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert!(
        cancels[0]["id"].is_null(),
        "session/cancel must be a notification"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stubborn_cancel_kills_and_reloads() {
    let h = harness("1");
    let mut s = h.open().await;
    must_complete(&text_turn(&mut s, "remember:KESTREL").await, "Hello");
    let mut rx = s
        .turn(TurnInput::text("stubborn"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = started(&mut rx).await;
    let start = Instant::now();
    s.interrupt().await.unwrap();
    assert!(
        start.elapsed() < INTERRUPT_WAIT + CLOSE_WAIT + Duration::from_secs(2),
        "interrupt took {:?}",
        start.elapsed()
    );
    events.extend(drain(&mut rx).await);
    must_fail(&terminal(&events), FailureKind::Interrupted);
    let (end, events) = turn(&mut s, TurnInput::text("recall")).await;
    must_complete(&end, "recalled KESTREL");
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.body, EventBody::Text(t) if t.contains("OLD"))),
        "replayed history leaked into the turn"
    );
    assert_eq!(h.launches(), 2);
    let loads = h.requests("session/load");
    assert!(
        loads.len() == 1 && loads[0]["sessionId"] == s.session_ref().id.as_str(),
        "{loads:?}"
    );
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn context_timeout_reports_timeout() {
    for _ in 0..5 {
        let h = harness("1");
        let mut s = h.open().await;
        let cancel = CancellationToken::new();
        let mut rx = s
            .turn(TurnInput::text("hang"), cancel.clone())
            .await
            .unwrap();
        let _ = started(&mut rx).await;
        cancel.cancel();
        must_fail(&terminal(&drain(&mut rx).await), FailureKind::Timeout);
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_then_reload() {
    let h = harness("1");
    let mut s = h.open().await;
    must_complete(&text_turn(&mut s, "remember:OSPREY").await, "Hello");
    let f = must_fail(&text_turn(&mut s, "crash").await, FailureKind::Crashed);
    assert!(
        f.message.contains("boom"),
        "stderr missing: {:?}",
        f.message
    );
    must_complete(&text_turn(&mut s, "recall").await, "recalled OSPREY");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_and_refusal() {
    let h = harness("1");
    let mut s = h.open().await;
    must_fail(
        &text_turn(&mut s, "quota").await,
        FailureKind::QuotaExhausted,
    );
    must_fail(&text_turn(&mut s, "refuse").await, FailureKind::Unknown);
    must_complete(&text_turn(&mut s, "ok").await, "Hello"); // the session survives both
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_output() {
    let schema = json!({"type": "object", "properties": {"steps": {"type": "array", "items": {"type": "string"}}},
        "required": ["steps"], "additionalProperties": false});
    let h = harness("1");
    let mut s = h.open().await;
    let with = |text: &str| TurnInput {
        text: text.into(),
        output_schema: Some(schema.clone()),
    };
    match turn(&mut s, with("json")).await.0 {
        EventBody::TurnCompleted(r) => assert_eq!(r.structured, Some(json!({"steps": ["a", "b"]}))),
        other => panic!("fenced json: {other:?}"),
    }
    let prompt = h.requests("session/prompt")[0]["prompt"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        prompt.contains("JSON Schema"),
        "prompt lacks the schema instruction: {prompt}"
    );
    // One repair prompt; the usage of both counts.
    match turn(&mut s, with("badjson")).await.0 {
        EventBody::TurnCompleted(r) => {
            assert_eq!(r.structured, Some(json!({"steps": ["fixed"]})));
            assert_eq!(r.usage.input_tokens, 200);
        }
        other => panic!("repaired: {other:?}"),
    }
    must_fail(
        &turn(&mut s, with("neverjson")).await.0,
        FailureKind::Protocol,
    );
    let bogus = TurnInput {
        text: "x".into(),
        output_schema: Some(json!({"type": "bogus"})),
    };
    assert!(
        matches!(
            s.turn(bogus, CancellationToken::new()).await,
            Err(Error::InvalidSchema { .. })
        ),
        "invalid schema accepted"
    );
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
    for _ in 0..7 {
        assert!(matches!(
            s.turn(TurnInput::text("hang"), CancellationToken::new())
                .await,
            Err(Error::TurnInProgress)
        ));
    }
    s.interrupt().await.unwrap();
    must_fail(&terminal(&drain(&mut rx).await), FailureKind::Interrupted);
    assert_eq!(h.launches(), 1);
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_across_sessions() {
    let h = harness("1");
    let mut s = h.open().await;
    must_complete(&text_turn(&mut s, "remember:HERON").await, "Hello");
    let reference = s.session_ref().clone();
    s.close().await.unwrap();
    let mut s2 = h.runtime.resume(reference, h.spec.clone()).await.unwrap();
    must_complete(&text_turn(&mut s2, "recall").await, "recalled HERON");
    s2.close().await.unwrap();
    let err = h
        .runtime
        .resume(
            SessionRef {
                runtime: "agy".into(),
                id: "missing".into(),
            },
            h.spec.clone(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("session not found"), "{err}");
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

#[tokio::test(flavor = "multi_thread")]
async fn close_idle_kills_process() {
    let mut h = harness("1");
    let (hook, ids) = spawn_recorder();
    h.spec.on_spawn = Some(hook);
    let s = h.open().await;
    let id = ids.lock().unwrap()[0];
    s.close().await.unwrap();
    assert!(!alive(&id).unwrap(), "agy alive after close");
}

#[tokio::test(flavor = "multi_thread")]
async fn close_in_flight_is_fast() {
    let mut h = harness("1");
    let (hook, ids) = spawn_recorder();
    h.spec.on_spawn = Some(hook);
    let mut s = h.open().await;
    let id = ids.lock().unwrap()[0];
    let mut rx = s
        .turn(TurnInput::text("stubborn"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = started(&mut rx).await;
    let start = Instant::now();
    s.close().await.unwrap();
    assert!(
        start.elapsed() < CLOSE_WAIT * 3 + Duration::from_secs(3),
        "close took {:?}",
        start.elapsed()
    );
    events.extend(drain(&mut rx).await);
    must_fail(&terminal(&events), FailureKind::Interrupted);
    assert!(!alive(&id).unwrap(), "process survived close");
}

#[tokio::test(flavor = "multi_thread")]
async fn close_with_unread_events_does_not_block() {
    let h = harness("1");
    let mut s = h.open().await;
    let _rx = s
        .turn(TurnInput::text("flood"), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(15), s.close())
        .await
        .expect("close blocked while events were unread")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_validation() {
    let h = harness("1");
    let no_dir = SessionSpec {
        dir: PathBuf::new(),
        ..h.spec.clone()
    };
    assert!(h.runtime.open(no_dir).await.is_err(), "no dir accepted");
    let strict = AgyRuntime::new(AgyOptions {
        allow_temp_dir: false,
        ..options(h.settings.clone())
    });
    let err = strict.open(h.spec.clone()).await.unwrap_err();
    assert!(
        err.to_string().contains("temp tree"),
        "temp workspace: {err}"
    );
    let bad = AgyRuntime::new(AgyOptions {
        binary: h.spec.dir.join("no-agy"),
        ..options(h.settings.clone())
    });
    assert!(
        bad.open(h.spec.clone()).await.is_err(),
        "missing binary accepted"
    );
}

/// Regression: the stream ending means the session accepts the next turn.
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

//! Ported from `internal/runtime/claude/fixtures_test.go`: every recorded Claude
//! transcript in `testdata/claude/` is fed through the session's dispatcher.
#![allow(
    clippy::wildcard_enum_match_arm,
    clippy::match_wild_err_arm,
    clippy::case_sensitive_file_extension_comparisons,
    reason = "test code"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openchef_core::PermissionMode;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ClaudeOptions;
use super::protocol::{ResultMessage, STRUCTURED_OUTPUT_TOOL};
use super::session::Actor;
use crate::rawlog::{Dir, RawLog, read_file};
use crate::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Event, EventBody, FailureKind,
    Question, RequestKind, SessionRef, SessionSpec, TurnEvents,
};

/// Records requests and signals each one, so replay can wait for the asynchronous
/// permission task before feeding the next line.
struct SignalApprover {
    requests: Mutex<Vec<ApprovalRequest>>,
    seen: mpsc::UnboundedSender<()>,
}

impl Approver for SignalApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        self.requests.lock().unwrap().push(request);
        let _sent = self.seen.send(()); // the replay may have stopped listening
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

struct Replay {
    turns: Vec<Vec<Event>>,
    approvals: Vec<ApprovalRequest>,
    results: Vec<ResultMessage>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/claude")
}

async fn collect(rx: &mut TurnEvents, name: &str) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(e)) => events.push(e),
            Ok(None) => return events,
            Err(_) => panic!("{name}: turn did not terminate; events: {events:?}"),
        }
    }
}

/// Feeds a recorded session through the dispatcher, opening a turn at every recorded
/// user message, and returns the normalized events per turn.
async fn replay(name: &str) -> Replay {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let approver = Arc::new(SignalApprover {
        requests: Mutex::new(Vec::new()),
        seen: seen_tx,
    });
    let spec = SessionSpec {
        dir: PathBuf::from("/fixture"),
        model: None,
        effort: None,
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env: Vec::new(),
        approver: approver.clone(),
        ask_user: None,
        log_path: None,
        on_spawn: None,
        confine: None,
    };
    let session = SessionRef {
        runtime: "claude".into(),
        id: "fixture".into(),
    };
    let mut actor = Actor::new(
        Arc::new(ClaudeOptions::default()),
        spec,
        RawLog::discard(),
        session,
        false,
    );
    let generation = actor.attach_detached();
    let mut out = Replay {
        turns: Vec::new(),
        approvals: Vec::new(),
        results: Vec::new(),
    };
    let mut open: Option<TurnEvents> = None;
    for record in read_file(&fixtures_dir().join(name)).unwrap() {
        let Some(msg) = record.msg else { continue };
        let kind = msg["type"].as_str().unwrap_or("").to_owned();
        match record.dir {
            Dir::Send if kind == "user" => {
                assert!(open.is_none(), "{name}: user message while a turn is open");
                open = Some(actor.open_turn(generation));
            }
            Dir::Recv => {
                actor.dispatch(generation, &msg);
                if kind == "control_request" && msg["request"]["subtype"] == "can_use_tool" {
                    tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
                        .await
                        .unwrap_or_else(|_| {
                            panic!("{name}: permission request never reached the approver")
                        });
                }
                if kind == "result" {
                    out.results
                        .push(serde_json::from_value(msg.clone()).unwrap());
                    let mut rx = open
                        .take()
                        .unwrap_or_else(|| panic!("{name}: result without a turn"));
                    out.turns.push(collect(&mut rx, name).await);
                }
            }
            Dir::Send | Dir::Stderr | Dir::Note | Dir::Argv => {}
        }
    }
    assert!(open.is_none(), "{name}: last turn never ended");
    out.approvals = approver.requests.lock().unwrap().clone();
    out
}

fn terminal<'a>(events: &'a [Event], name: &str) -> &'a EventBody {
    let last = events
        .last()
        .unwrap_or_else(|| panic!("{name}: empty turn"));
    assert!(last.is_terminal(), "{name}: no terminal event: {events:?}");
    assert!(
        events[..events.len() - 1].iter().all(|e| !e.is_terminal()),
        "{name}: terminal event before the end"
    );
    &last.body
}

fn streamed(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Text(t) => Some(t.as_str()),
            EventBody::ToolCall(_)
            | EventBody::ToolResult(_)
            | EventBody::Quota(_)
            | EventBody::TurnCompleted(_)
            | EventBody::TurnFailed(_) => None,
        })
        .collect()
}

/// The contract on every recorded session: one terminal event per user message, usage
/// from the result only, final text from the result only, tool results matching tool
/// calls, no `StructuredOutput` plumbing in the stream.
#[tokio::test]
async fn replay_all_fixtures() {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    names.sort();
    assert_ne!(names.len(), 0);
    for name in &names {
        let rp = replay(name).await;
        assert!(
            !rp.turns.is_empty() && rp.turns.len() == rp.results.len(),
            "{name}: {} turns for {} results",
            rp.turns.len(),
            rp.results.len()
        );
        for (i, events) in rp.turns.iter().enumerate() {
            let want = rp.results[i].usage();
            match terminal(events, name) {
                EventBody::TurnCompleted(r) => {
                    assert_eq!(r.usage, want, "{name} turn {i}: usage not the result's");
                    assert_eq!(
                        r.final_text, rp.results[i].result,
                        "{name} turn {i}: final text not the result's"
                    );
                }
                EventBody::TurnFailed(f) => assert_eq!(f.usage, want, "{name} turn {i}: usage"),
                other => panic!("{name}: {other:?}"),
            }
            let mut calls = std::collections::HashSet::new();
            for e in events {
                match &e.body {
                    EventBody::ToolCall(c) => {
                        assert_ne!(
                            c.name, STRUCTURED_OUTPUT_TOOL,
                            "{name} turn {i}: StructuredOutput leaked"
                        );
                        assert!(
                            calls.insert(c.id.clone()),
                            "{name} turn {i}: tool call {} twice",
                            c.id
                        );
                    }
                    EventBody::ToolResult(r) => assert!(
                        calls.contains(&r.id),
                        "{name} turn {i}: result {} without a call",
                        r.id
                    ),
                    EventBody::Text(_)
                    | EventBody::Quota(_)
                    | EventBody::TurnCompleted(_)
                    | EventBody::TurnFailed(_) => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn replay_manual_approvals() {
    for (name, want) in [
        ("stdio-manual.jsonl", 3),
        ("m16-restricted-manual.jsonl", 3),
        ("m16-restricted-acceptedits.jsonl", 2),
    ] {
        let rp = replay(name).await;
        assert_eq!(rp.approvals.len(), want, "{name}");
        for a in &rp.approvals {
            assert!(
                a.session.id == "fixture" && !a.raw.is_null(),
                "{name}: {a:?}"
            );
            if a.tool == "Bash" {
                assert!(
                    a.kind == RequestKind::Command
                        && a.command.as_deref().is_some_and(|c| !c.is_empty()),
                    "{name}: {a:?}"
                );
            }
            if a.tool == "Write" {
                assert!(
                    a.kind == RequestKind::FileChange && !a.paths.is_empty(),
                    "{name}: {a:?}"
                );
            }
        }
    }
    // acceptEdits: writes to tool config still reach the host under --restricted.
    let rp = replay("m16-restricted-acceptedits.jsonl").await;
    let paths: Vec<String> = rp.approvals.iter().flat_map(|a| a.paths.clone()).collect();
    assert!(
        paths.iter().any(|p| p.contains(".claude")),
        "settings write did not reach the approver: {paths:?}"
    );
}

#[tokio::test]
async fn replay_structured_output() {
    for name in ["json-schema.jsonl", "m17-planner.jsonl"] {
        let rp = replay(name).await;
        match terminal(&rp.turns[0], name) {
            EventBody::TurnCompleted(r) => assert!(
                r.structured.as_ref().is_some_and(Value::is_object),
                "{name}: {r:?}"
            ),
            other => panic!("{name}: {other:?}"),
        }
    }
    let models = |rp: &Replay| match terminal(&rp.turns[0], "models") {
        EventBody::TurnCompleted(r) => r.models.clone(),
        other => panic!("{other:?}"),
    };
    // The planner shape keeps the requested model (D-008); Claude's plan mode did not.
    assert_eq!(
        models(&replay("m17-planner.jsonl").await),
        ["claude-haiku-5-5"]
    );
    assert_eq!(
        models(&replay("m16-plan-model.jsonl").await),
        ["claude-sonnet-5-5"]
    );
}

#[tokio::test]
async fn replay_interrupt_then_next_turn() {
    let rp = replay("m16-interrupt.jsonl").await;
    assert_eq!(rp.turns.len(), 2);
    assert!(
        matches!(terminal(&rp.turns[0], "first"), EventBody::TurnFailed(f) if f.kind == FailureKind::Interrupted)
    );
    assert!(
        matches!(terminal(&rp.turns[1], "second"), EventBody::TurnCompleted(r) if r.final_text == "SECOND")
    );
    assert_eq!(streamed(&rp.turns[1]), "SECOND");
}

#[tokio::test]
async fn replay_two_turns_and_budget() {
    let rp = replay("m17-auto.jsonl").await;
    match (terminal(&rp.turns[0], "a"), terminal(&rp.turns[1], "b")) {
        (EventBody::TurnCompleted(a), EventBody::TurnCompleted(b)) => {
            assert_eq!(
                (a.denials, b.denials, b.final_text.as_str()),
                (1, 0, "SECOND")
            );
        }
        other => panic!("{other:?}"),
    }
    for (i, events) in replay("m16-budget.jsonl").await.turns.iter().enumerate() {
        assert!(
            matches!(terminal(events, "budget"), EventBody::TurnFailed(f) if f.kind == FailureKind::Unknown),
            "budget turn {i}"
        );
    }
    let quota: Vec<_> = rp.turns[0]
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Quota(q) => Some(q),
            EventBody::Text(_)
            | EventBody::ToolCall(_)
            | EventBody::ToolResult(_)
            | EventBody::TurnCompleted(_)
            | EventBody::TurnFailed(_) => None,
        })
        .collect();
    assert!(
        !quota.is_empty(),
        "rate_limit_event did not become a quota event"
    );
    assert!(
        quota.iter().all(|q| !q.exhausted && q.windows.len() == 2),
        "{quota:?}"
    );
}

/// Deltas are streamed, but the final text comes from the result alone (§6.1).
#[tokio::test]
async fn replay_streamed_text_is_not_the_final_text() {
    let rp = replay("eof-midturn.jsonl").await;
    let text = streamed(&rp.turns[0]);
    match terminal(&rp.turns[0], "eof") {
        EventBody::TurnCompleted(r) => assert!(!text.is_empty() && text == r.final_text),
        other => panic!("{other:?}"),
    }
}

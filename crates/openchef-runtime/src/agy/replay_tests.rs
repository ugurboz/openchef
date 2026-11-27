//! Ported from `internal/runtime/agy/fixtures_test.go`: recorded `agy acp` sessions in
//! `testdata/agy/` are fed through the session's handlers: updates, permission
//! requests, and prompts with their recorded results.
#![allow(
    clippy::wildcard_enum_match_arm,
    clippy::too_many_lines,
    reason = "test code"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openchef_core::PermissionMode;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::protocol::{PromptResult, SessionResult};
use super::session::{Actor, Prompted};
use super::{AgyOptions, NAME};
use crate::rawlog::{Dir, RawLog, read_file};
use crate::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Event, EventBody, FailureKind,
    McpServer, Question, SessionRef, SessionSpec, TurnEvents,
};

/// Allows everything except commands mentioning "denied" or "outside", like the spike
/// did, and records what it saw.
#[derive(Default)]
struct ReplayApprover {
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl Approver for ReplayApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let command = request.command.clone().unwrap_or_default();
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(
                if command.contains("denied") || command.contains("outside") {
                    Decision::deny("spike policy")
                } else {
                    Decision::allow()
                },
            )
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

#[derive(Default)]
struct Replayed {
    turns: Vec<Vec<Event>>,
    answers: Vec<Value>,
    requests: Vec<ApprovalRequest>,
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/agy")
}

async fn collect(rx: &mut TurnEvents) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("events did not close")
        {
            Some(e) => events.push(e),
            None => return events,
        }
    }
}

async fn replay(mode: PermissionMode, files: &[&str]) -> Replayed {
    let approver = Arc::new(ReplayApprover::default());
    let spec = SessionSpec {
        dir: PathBuf::from("/fixture"),
        model: None,
        effort: None,
        mode,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env: Vec::new(),
        approver: approver.clone(),
        ask_user: Some(McpServer {
            name: "openchef".into(),
            command: "openchef".into(),
            args: Vec::new(),
            env: Vec::new(),
            tool_timeout: None,
        }),
        log_path: None,
        on_spawn: None,
        confine: None,
    };
    let session = SessionRef {
        runtime: NAME.into(),
        id: String::new(),
    };
    let mut actor = Actor::new(
        Arc::new(AgyOptions::default()),
        spec,
        RawLog::discard(),
        session,
    );
    let mut out = Replayed::default();
    for file in files {
        let generation = actor.attach_detached();
        let mut sent: HashMap<String, String> = HashMap::new();
        let mut open: Option<TurnEvents> = None;
        let mut loading = false;
        for record in read_file(&fixtures().join(file)).unwrap() {
            let Some(msg) = record.msg else { continue };
            let method = msg["method"].as_str().unwrap_or("").to_owned();
            let id = msg["id"].to_string();
            match record.dir {
                Dir::Send if !method.is_empty() => {
                    drop(sent.insert(id, method.clone()));
                    match method.as_str() {
                        "session/load" => loading = true,
                        "session/prompt" if open.is_none() => {
                            open = Some(actor.open_turn(generation, None));
                        }
                        _ => {}
                    }
                }
                Dir::Recv if method == "session/update" => {
                    if !loading {
                        actor.on_update(generation, &msg["params"]);
                    }
                }
                Dir::Recv if method == "session/request_permission" => {
                    let (reply, answer) = oneshot::channel();
                    actor.on_permission(
                        generation,
                        &msg["params"],
                        &CancellationToken::new(),
                        reply,
                    );
                    let answer = tokio::time::timeout(Duration::from_secs(5), answer)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    out.answers.push(answer);
                    while let Ok(m) = actor.internal_rx.try_recv() {
                        actor.handle(m); // the denial count
                    }
                }
                Dir::Recv if method.is_empty() && !msg["id"].is_null() => {
                    match sent.get(&id).map(String::as_str) {
                        Some("session/new") => {
                            let res: SessionResult =
                                serde_json::from_value(msg["result"].clone()).unwrap();
                            actor.session.id = res.session_id;
                        }
                        Some("session/load") => loading = false,
                        Some("session/prompt") => {
                            let res: PromptResult = serde_json::from_value(msg["result"].clone())
                                .expect("prompt result");
                            let turn = actor.turn_id().expect("a prompt result without a turn");
                            actor.prompt_done(turn, Prompted::Answered(res));
                            let mut rx = open.take().expect("no open turn");
                            out.turns.push(collect(&mut rx).await);
                        }
                        _ => {}
                    }
                }
                Dir::Send | Dir::Recv | Dir::Stderr | Dir::Note | Dir::Argv => {}
            }
        }
        assert!(open.is_none(), "{file}: last turn never ended");
    }
    out.requests = approver.requests.lock().unwrap().clone();
    out
}

fn terminal(events: &[Event]) -> &EventBody {
    let last = events.last().expect("empty turn");
    assert!(
        last.is_terminal() && events[..events.len() - 1].iter().all(|e| !e.is_terminal()),
        "{events:?}"
    );
    &last.body
}

fn streamed(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| {
            if let EventBody::Text(t) = &e.body {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn final_text(events: &[Event]) -> String {
    match terminal(events) {
        EventBody::TurnCompleted(r) => r.final_text.clone(),
        other => panic!("not completed: {other:?}"),
    }
}

#[tokio::test]
async fn replay_contract_on_all_fixtures() {
    let mut names: Vec<String> = std::fs::read_dir(fixtures())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| Path::new(n).extension().is_some_and(|x| x == "jsonl"))
        .filter(|n| n != "eof-midturn.jsonl") // the turn never ends: stdin closed mid-turn (m1-findings §2)
        .collect();
    names.sort();
    assert_ne!(names.len(), 0);
    for name in &names {
        let rp = replay(PermissionMode::Manual, &[name.as_str()]).await;
        assert_ne!(rp.turns.len(), 0, "{name}: no turns");
        for (i, events) in rp.turns.iter().enumerate() {
            terminal(events);
            let mut calls = std::collections::HashSet::new();
            for e in events {
                match &e.body {
                    EventBody::ToolCall(c) => assert!(
                        calls.insert(c.id.clone()),
                        "{name} turn {i}: {} twice",
                        c.id
                    ),
                    EventBody::ToolResult(r) => assert!(
                        calls.contains(&r.id),
                        "{name} turn {i}: result {} without a call",
                        r.id
                    ),
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn replay_load_drops_history() {
    let rp = replay(
        PermissionMode::Manual,
        &["m18-load.jsonl", "m18-load-resumed.jsonl"],
    )
    .await;
    assert_eq!(rp.turns.len(), 2);
    assert_eq!(final_text(&rp.turns[0]), "READY");
    // The replayed history ("READY", the user's message) must not leak into the turn.
    assert_eq!(
        (final_text(&rp.turns[1]), streamed(&rp.turns[1])),
        ("KESTREL".to_owned(), "KESTREL".to_owned())
    );
    match terminal(&rp.turns[1]) {
        EventBody::TurnCompleted(r) => assert_eq!(
            (r.usage.cache_read_tokens, r.usage.input_tokens),
            (17962, 2271)
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn replay_cancel() {
    let rp = replay(PermissionMode::Manual, &["m18-cancel.jsonl"]).await;
    match terminal(&rp.turns[0]) {
        EventBody::TurnFailed(f) => assert!(
            f.kind == FailureKind::Interrupted && f.usage.output_tokens > 0,
            "{f:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(final_text(&rp.turns[1]), "SECOND");
}

#[tokio::test]
async fn replay_permissions() {
    let rp = replay(PermissionMode::Manual, &["m18-bypass.jsonl"]).await;
    assert_eq!(rp.requests.len(), 2, "{:?}", rp.requests);
    assert_eq!(rp.requests[0].command.as_deref(), Some("echo a > cmd.txt"));
    assert!(
        rp.requests[0]
            .description
            .contains("outside the agy sandbox"),
        "{:?}",
        rp.requests[0]
    );
    assert_eq!(rp.answers.len(), 2);
    assert_eq!(
        (
            rp.answers[0]["outcome"]["optionId"].as_str(),
            rp.answers[1]["outcome"]["optionId"].as_str()
        ),
        (Some("allow"), Some("deny"))
    );
    match terminal(&rp.turns[0]) {
        EventBody::TurnCompleted(r) => assert_eq!(r.denials, 1),
        other => panic!("{other:?}"),
    }
    // Sandboxed failures are tool errors, not turn failures.
    let errors = rp.turns[0]
        .iter()
        .filter(|e| matches!(&e.body, EventBody::ToolResult(r) if r.is_error))
        .count();
    assert!(
        errors >= 2,
        "{errors} failed tool results, want the sandbox failure and the denial"
    );
}

#[tokio::test]
async fn replay_ask_user_is_allowed_without_approver() {
    let rp = replay(PermissionMode::Manual, &["m18-mcp.jsonl"]).await;
    assert!(
        rp.requests.is_empty(),
        "ask_user reached the approver: {:?}",
        rp.requests
    );
    assert!(
        rp.answers.len() == 1 && rp.answers[0]["outcome"]["optionId"] == "allow",
        "{:?}",
        rp.answers
    );
    assert_eq!(final_text(&rp.turns[0]), "teal");
}

#[tokio::test]
async fn replay_plan_mode_refuses_every_request() {
    let rp = replay(PermissionMode::Plan, &["m18-review-sandbox.jsonl"]).await;
    assert!(
        rp.requests.is_empty(),
        "plan mode consulted the approver: {:?}",
        rp.requests
    );
    assert_ne!(rp.answers.len(), 0, "no requests in the fixture");
    for a in &rp.answers {
        assert_eq!(a["outcome"]["optionId"], "deny", "plan mode allowed: {a}");
    }
}

#[tokio::test]
async fn replay_final_text_is_the_last_message() {
    let rp = replay(PermissionMode::Manual, &["m18-auto.jsonl"]).await;
    let text = final_text(&rp.turns[0]);
    assert!(text.starts_with("DONE"), "{text}");
    assert!(
        streamed(&rp.turns[0]).ends_with(&text),
        "final text is not the tail of the stream"
    );
}

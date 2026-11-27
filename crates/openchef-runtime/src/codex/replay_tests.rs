//! Ported from `internal/runtime/codex/fixtures_test.go`: recorded app-server sessions
//! in `testdata/codex/` are fed through the session's notification router.
#![allow(
    clippy::wildcard_enum_match_arm,
    clippy::match_wild_err_arm,
    reason = "test code"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use openchef_core::PermissionMode;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::protocol::{
    CommandApproval, Elicitation, RateLimits, ThreadResponse, TurnStartResponse,
};
use super::session::{ASK_USER_TOOL, Actor};
use crate::linerpc::Notification;
use crate::rawlog::{Dir, Record, read_file};
use crate::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Event, EventBody, FailureKind,
    Question, SessionSpec, TurnEvents,
};

struct Never;

impl Approver for Never {
    fn approve(
        &self,
        _: ApprovalRequest,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        Box::pin(async { Ok(Decision::deny("replay")) })
    }

    fn answer(
        &self,
        _: Question,
        _: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        Box::pin(async { Ok(String::new()) })
    }
}

fn fixture(name: &str) -> Vec<Record> {
    read_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/codex")
            .join(name),
    )
    .unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec {
        dir: PathBuf::from("/fixture"),
        model: None,
        effort: None,
        mode: PermissionMode::Manual,
        deny_rules: Vec::new(),
        ask_rules: Vec::new(),
        web_access: false,
        env: Vec::new(),
        approver: Arc::new(Never),
        ask_user: None,
        log_path: None,
        on_spawn: None,
        confine: None,
    }
}

fn id_key(msg: &Value) -> String {
    msg["id"].to_string()
}

async fn collect(rx: &mut TurnEvents) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(e)) => events.push(e),
            Ok(None) => return events,
            Err(elapsed) => panic!("turn did not terminate ({elapsed}); events: {events:?}"),
        }
    }
}

/// Feeds a recorded session through the router one turn at a time and returns the
/// normalized events of every turn.
async fn replay_turns(records: &[Record]) -> Vec<Vec<Event>> {
    let mut actor = Actor::detached(spec());
    let mut turns = Vec::new();
    let mut open: Option<TurnEvents> = None;
    let mut sent: HashMap<String, String> = HashMap::new();
    for record in records {
        let Some(msg) = &record.msg else { continue };
        let method = msg["method"].as_str().unwrap_or("");
        let has_id = !msg["id"].is_null();
        match record.dir {
            Dir::Send => {
                if !method.is_empty() {
                    drop(sent.insert(id_key(msg), method.to_owned()));
                }
                if method == "turn/start" {
                    open = Some(actor.open_turn(!msg["params"]["outputSchema"].is_null()));
                }
            }
            Dir::Recv if !method.is_empty() && !has_id => {
                actor.on_notification(&Notification {
                    method: method.to_owned(),
                    params: msg["params"].clone(),
                });
                if method == "turn/completed"
                    && let Some(mut rx) = open.take()
                {
                    turns.push(collect(&mut rx).await);
                }
            }
            Dir::Recv if has_id && method.is_empty() => {
                match sent.get(&id_key(msg)).map(String::as_str) {
                    Some("thread/start" | "thread/resume") => {
                        let res: ThreadResponse =
                            serde_json::from_value(msg["result"].clone()).unwrap();
                        actor.session.id = res.thread.id;
                    }
                    Some("turn/start") => {
                        let res: TurnStartResponse =
                            serde_json::from_value(msg["result"].clone()).unwrap();
                        actor.set_turn_id(&res.turn.id);
                    }
                    Some("account/rateLimits/read") => {
                        let res: RateLimits = serde_json::from_value(msg["result"].clone())
                            .expect("rateLimits/read result does not decode");
                        actor.limits = res.rate_limits;
                    }
                    _ => {}
                }
            }
            Dir::Recv | Dir::Stderr | Dir::Note | Dir::Argv => {}
        }
    }
    turns
}

fn terminal(events: &[Event]) -> &EventBody {
    let last = events.last().expect("empty turn");
    assert!(
        last.is_terminal() && events[..events.len() - 1].iter().all(|e| !e.is_terminal()),
        "{events:?}"
    );
    &last.body
}

fn tool_calls(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ToolCall(c) => Some(c.signature.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn replay_main_session() {
    let turns = replay_turns(&fixture("main.jsonl")).await;
    // approve-cmd, decline-cmd, output-schema, user-input, interrupt
    assert_eq!(turns.len(), 5);

    let first = match terminal(&turns[0]) {
        EventBody::TurnCompleted(r) if r.final_text == "DONE" => r.clone(),
        other => panic!("turn 1: {other:?}"),
    };
    let calls = tool_calls(&turns[0]);
    assert!(
        calls.len() == 1 && calls[0].contains("echo spike > out.txt"),
        "{calls:?}"
    );
    assert!(
        first.usage.input_tokens > 0 && first.usage.output_tokens > 0,
        "{:?}",
        first.usage
    );
    assert!(
        turns[0]
            .iter()
            .any(|e| matches!(e.body, EventBody::Text(_))),
        "turn 1 streamed no text"
    );

    // A declined command: the turn still completes; the command is not a success.
    let second = match terminal(&turns[1]) {
        EventBody::TurnCompleted(r) => r.clone(),
        other => panic!("turn 2: {other:?}"),
    };
    assert!(
        turns[1]
            .iter()
            .any(|e| matches!(&e.body, EventBody::ToolResult(r) if r.is_error)),
        "{:?}",
        turns[1]
    );

    match terminal(&turns[2]) {
        EventBody::TurnCompleted(r) => {
            let steps = r
                .structured
                .as_ref()
                .and_then(|s| s["steps"].as_array())
                .map(Vec::len);
            assert_eq!(steps, Some(2), "{:?}", r.structured);
        }
        other => panic!("turn 3: {other:?}"),
    }
    assert!(
        matches!(terminal(&turns[4]), EventBody::TurnFailed(f) if f.kind == FailureKind::Interrupted)
    );

    // Usage is per turn, not cumulative.
    let (a, b) = (first.usage, second.usage);
    assert!(
        b.input_tokens + b.cache_read_tokens
            < a.input_tokens + a.cache_read_tokens + b.input_tokens + b.cache_read_tokens,
        "usage looks cumulative: {a:?} {b:?}"
    );
}

#[tokio::test]
async fn replay_resume_and_mcp() {
    let resume = replay_turns(&fixture("resume.jsonl")).await;
    assert_eq!(resume.len(), 1);
    assert!(
        matches!(terminal(&resume[0]), EventBody::TurnCompleted(r) if r.final_text.contains("HERON"))
    );
    let mcp = replay_turns(&fixture("mcp.jsonl")).await;
    assert_eq!(mcp.len(), 1);
    let calls = tool_calls(&mcp[0]);
    assert!(
        calls.len() == 1 && calls[0].starts_with("mcp__openchef__ask_user:"),
        "{calls:?}"
    );
    assert!(matches!(terminal(&mcp[0]), EventBody::TurnCompleted(r) if r.final_text == "teal"));
}

/// Every recorded server request decodes into the shape the handler reads.
#[test]
fn recorded_server_requests_decode() {
    let mut seen = std::collections::HashSet::new();
    for name in [
        "main.jsonl",
        "mcp.jsonl",
        "resume.jsonl",
        "eof-midturn.jsonl",
    ] {
        for r in fixture(name) {
            let Some(msg) = r.msg.filter(|m| r.dir == Dir::Recv && !m["id"].is_null()) else {
                continue;
            };
            let Some(method) = msg["method"].as_str() else {
                continue;
            };
            let _new = seen.insert(method.to_owned());
            match method {
                "item/commandExecution/requestApproval" => {
                    let p: CommandApproval = serde_json::from_value(msg["params"].clone()).unwrap();
                    assert!(!p.command.is_empty() && !p.thread_id.is_empty(), "{p:?}");
                }
                "mcpServer/elicitation/request" => {
                    let p: Elicitation = serde_json::from_value(msg["params"].clone()).unwrap();
                    assert!(
                        p.meta.approval_kind == "mcp_tool_call"
                            && p.meta.tool_name == ASK_USER_TOOL
                            && p.server_name == "openchef",
                        "{p:?}"
                    );
                }
                _ => {}
            }
        }
    }
    for want in [
        "item/commandExecution/requestApproval",
        "mcpServer/elicitation/request",
    ] {
        assert!(seen.contains(want), "fixtures contain no {want}");
    }
}

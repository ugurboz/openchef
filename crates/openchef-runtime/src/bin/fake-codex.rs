//! A scripted `codex app-server` for the adapter tests (port of the Go
//! `fake_test.go`). `OPENCHEF_FAKE_CODEX` picks the scenario; `OPENCHEF_FAKE_LOG`
//! receives every line the fake reads, for assertions.
// A test fixture, not product code: it ignores write failures (the adapter under test
// may have gone) and favours one readable script over small functions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test fixture"
)]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead as _, Write as _};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

struct Fake {
    scenario: String,
    out: Mutex<std::io::Stdout>,
    replies: Mutex<HashMap<String, Sender<Value>>>,
    next_request: Mutex<u32>,
    turns: Mutex<u32>,
    thread: Mutex<String>,
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

impl Fake {
    fn thread(&self) -> String {
        self.thread.lock().unwrap().clone()
    }

    fn send(&self, v: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Sends a server-initiated request and waits for the client's result.
    fn request(&self, method: &str, params: Value) -> Value {
        let id = {
            let mut n = self.next_request.lock().unwrap();
            *n += 1;
            900 + *n
        };
        let (tx, rx) = channel();
        self.replies.lock().unwrap().insert(id.to_string(), tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or(Value::Null)
    }

    fn item(&self, turn: &str, kind: &str, mut item: Value) {
        item["type"] = json!(kind);
        self.notify(
            "item/completed",
            json!({"threadId": self.thread(), "turnId": turn, "item": item, "completedAtMs": 1}),
        );
    }

    fn complete(&self, turn: &str, final_text: &str) {
        self.notify("thread/tokenUsage/updated", json!({"threadId": self.thread(), "turnId": turn, "tokenUsage": {
            "last": {"inputTokens": 100, "cachedInputTokens": 40, "outputTokens": 10, "reasoningOutputTokens": 0, "totalTokens": 110},
            "total": {"inputTokens": 1100, "cachedInputTokens": 440, "outputTokens": 110, "reasoningOutputTokens": 0, "totalTokens": 1210}}}));
        self.item(
            turn,
            "agentMessage",
            json!({"id": "msg", "text": final_text, "phase": "final_answer"}),
        );
        self.notify("turn/completed", json!({"threadId": self.thread(), "turn": {"id": turn, "status": "completed", "durationMs": 42}}));
    }

    fn handle(&self, m: &Value) {
        let id = &m["id"];
        match m["method"].as_str().unwrap_or("") {
            "initialize" => self.reply(id, json!({"userAgent": "fake"})),
            "model/list" => self.reply(id, json!({"data": [{"id": "gpt-test", "model": "gpt-test", "isDefault": true,
                "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "medium"},
                    {"reasoningEffort": "high"}, {"reasoningEffort": "xhigh"}]}]})),
            "account/rateLimits/read" => {
                let reset = now() + 3600;
                self.reply(id, json!({"rateLimits": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": reset},
                    "secondary": {"usedPercent": 50, "windowDurationMins": 10080, "resetsAt": reset + 86400}}}));
            }
            "thread/start" => self.reply(id, json!({"thread": {"id": self.thread()}, "model": "gpt-test"})),
            "thread/resume" => {
                let thread = m["params"]["threadId"].as_str().unwrap_or("").to_owned();
                self.thread.lock().unwrap().clone_from(&thread);
                self.reply(id, json!({"thread": {"id": thread}}));
            }
            "turn/interrupt" => {
                self.reply(id, json!({}));
                let turn = m["params"]["turnId"].clone();
                self.notify("turn/completed", json!({"threadId": self.thread(), "turn": {"id": turn, "status": "interrupted"}}));
            }
            "turn/start" => {
                let turn = {
                    let mut n = self.turns.lock().unwrap();
                    *n += 1;
                    format!("turn-{n}")
                };
                if self.scenario == "stale" {
                    // Leftovers of a previous turn, sent before the new turn's id is known
                    // (seen live right after thread/resume).
                    self.notify("thread/tokenUsage/updated", json!({"threadId": self.thread(), "turnId": "old-turn", "tokenUsage": {
                        "last": {"inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 1},
                        "total": {"inputTokens": 5000, "cachedInputTokens": 0, "outputTokens": 500}}}));
                    self.notify("turn/completed", json!({"threadId": self.thread(), "turn": {"id": "old-turn", "status": "failed", "error": {"message": "stale"}}}));
                }
                self.reply(id, json!({"turn": {"id": turn, "status": "inProgress"}}));
                if self.scenario == "fastfail" {
                    self.notify("turn/completed", json!({"threadId": self.thread(), "turn": {"id": turn, "status": "failed",
                        "error": {"message": "denied", "codexErrorInfo": "unauthorized"}}}));
                    return;
                }
                self.run_turn(&turn, &m["params"]);
            }
            other => self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown {other}")}})),
        }
    }

    fn run_turn(&self, turn: &str, params: &Value) {
        let thread = self.thread();
        self.notify(
            "turn/started",
            json!({"threadId": thread, "turn": {"id": turn}}),
        );
        match self.scenario.as_str() {
            "ok" | "stale" => {
                for d in ["Hel", "lo"] {
                    self.notify(
                        "item/agentMessage/delta",
                        json!({"threadId": thread, "turnId": turn, "itemId": "msg", "delta": d}),
                    );
                }
                self.notify("account/rateLimits/updated", json!({"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}}}));
                let final_text = if params["outputSchema"].is_null() {
                    "Hello"
                } else {
                    r#"{"ok":true}"#
                };
                self.complete(turn, final_text);
            }
            "approve" => {
                self.notify("item/started", json!({"threadId": thread, "turnId": turn, "item": {
                    "id": "cmd1", "type": "commandExecution", "command": "rm -rf build", "cwd": "/w", "status": "inProgress", "commandActions": []}}));
                let res = self.request("item/commandExecution/requestApproval", json!({
                    "threadId": thread, "turnId": turn, "itemId": "cmd1", "startedAtMs": 1, "command": "rm -rf build", "cwd": "/w", "reason": "cleanup"}));
                let decision = res["decision"].as_str().unwrap_or("").to_owned();
                let status = if decision == "accept" || decision == "acceptForSession" {
                    "completed"
                } else {
                    "declined"
                };
                self.item(turn, "commandExecution", json!({"id": "cmd1", "command": "rm -rf build", "cwd": "/w", "status": status, "commandActions": []}));
                self.complete(turn, &format!("decision:{decision}"));
            }
            "editapprove" => {
                self.notify("item/started", json!({"threadId": thread, "turnId": turn, "item": {
                    "id": "fc1", "type": "fileChange", "status": "inProgress", "changes": [{"path": "/w/a.txt", "kind": "add"}]}}));
                let res = self.request(
                    "item/fileChange/requestApproval",
                    json!({"threadId": thread, "turnId": turn, "itemId": "fc1", "reason": "write"}),
                );
                self.complete(
                    turn,
                    &format!("decision:{}", res["decision"].as_str().unwrap_or("")),
                );
            }
            "ask" => {
                let el = self.request("mcpServer/elicitation/request", json!({"threadId": thread, "serverName": "openchef", "message": "Allow?",
                    "mode": "form", "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_name": "ask_user"}}));
                let ui = self.request(
                    "item/tool/requestUserInput",
                    json!({"threadId": thread, "turnId": turn, "itemId": "q", "isBlocking": true,
                    "questions": [{"id": "q1", "header": "Colour", "question": "Which colour?"}]}),
                );
                let got = ui["answers"]["q1"]["answers"][0]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                self.complete(
                    turn,
                    &format!("{}:{got}", el["action"].as_str().unwrap_or("")),
                );
            }
            "quota" => {
                let reset = now() + 7200;
                self.notify("account/rateLimits/updated", json!({"rateLimits": {"primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": reset}}}));
                self.notify("turn/completed", json!({"threadId": thread, "turn": {"id": turn, "status": "failed",
                    "error": {"message": "You've hit your usage limit", "codexErrorInfo": "usageLimitExceeded"}}}));
            }
            "crash" => std::process::exit(3),
            "flood" => {
                for i in 0..2000 {
                    self.notify("item/agentMessage/delta", json!({"threadId": thread, "turnId": turn, "itemId": "msg", "delta": i.to_string()}));
                }
            }
            // "hang": wait for turn/interrupt (handled in `handle`).
            _ => {}
        }
    }
}

fn main() {
    let scenario = std::env::var("OPENCHEF_FAKE_CODEX").unwrap_or_default();
    let mut log = std::env::var("OPENCHEF_FAKE_LOG")
        .ok()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    let fake = Arc::new(Fake {
        scenario,
        out: Mutex::new(std::io::stdout()),
        replies: Mutex::new(HashMap::new()),
        next_request: Mutex::new(0),
        turns: Mutex::new(0),
        thread: Mutex::new("thr-1".to_owned()),
    });
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
        if m["method"].is_null() {
            // A response to one of the fake's requests.
            if let Some(tx) = fake.replies.lock().unwrap().remove(&m["id"].to_string()) {
                let _ = tx.send(m["result"].clone());
            }
            continue;
        }
        if m["id"].is_null() {
            continue; // a notification (initialized)
        }
        let fake = Arc::clone(&fake);
        drop(thread::spawn(move || fake.handle(&m)));
    }
}

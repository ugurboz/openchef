//! A scripted `agy acp` for the adapter tests (port of the Go `fake_test.go`),
//! mirroring `testdata/agy` and m1-findings §11: `session/load` replays history before
//! it answers, `session/cancel` ends the prompt with `cancelled`, thinking levels depend
//! on the model, permission options carry kinds. The prompt's first word selects the
//! behaviour (see `prompt`).
//!
//! `OPENCHEF_FAKE_AGY` enables it ("1", or "permidle" for a permission request outside
//! any turn); `OPENCHEF_FAKE_LOG` receives argv and every stdin line;
//! `OPENCHEF_FAKE_STATE` holds one file per session (its "transcript").
// A test fixture, not product code: it ignores write failures (the adapter under test
// may have gone) and favours one readable script over small functions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test fixture"
)]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Default)]
struct State {
    session: String,
    model: String,
    next: u32,
    replies: HashMap<String, Sender<Value>>,
    /// Set by session/cancel for the prompt in progress; stays set once raised, since
    /// the prompt thread may reach `wait_cancel` only after the cancel arrived.
    cancelled: bool,
    last_kind: String,
}

struct Fake {
    scenario: String,
    state_dir: PathBuf,
    out: Mutex<std::io::Stdout>,
    log: Option<Mutex<std::fs::File>>,
    st: Mutex<State>,
    cancel: Condvar,
}

impl Fake {
    fn record(&self, v: &Value) {
        if let Some(log) = &self.log {
            let _ = writeln!(log.lock().unwrap(), "{v}");
        }
    }

    fn send(&self, v: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn fail(&self, id: &Value, msg: &str) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": msg}}));
    }

    fn update(&self, u: Value) {
        let sid = self.st.lock().unwrap().session.clone();
        self.send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}));
    }

    fn chunk(&self, text: &str) {
        self.update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}));
    }

    fn config_options(&self) -> Value {
        let model = self.st.lock().unwrap().model.clone();
        let levels: &[&str] = if model == "gemini-small" {
            &["low", "medium"]
        } else {
            &["low", "medium", "high", "xhigh"]
        };
        let opt = |id: &str, cur: &str, values: &[&str]| {
            let vs: Vec<Value> = values
                .iter()
                .map(|v| json!({"name": v, "value": v}))
                .collect();
            json!({"id": id, "category": id, "type": "select", "currentValue": cur, "options": vs})
        };
        json!([
            opt("model", &model, &["gemini-big", "gemini-small"]),
            opt("thought_level", "high", levels),
            opt(
                "mode",
                "default",
                &["default", "accept-edits", "auto", "plan"]
            ),
        ])
    }

    fn transcript(&self, id: &str) -> PathBuf {
        self.state_dir.join(id)
    }

    fn handle(self: &Arc<Self>, id: &Value, method: &str, params: &Value) {
        match method {
            "initialize" => self.reply(id, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}})),
            "session/new" => {
                let sid = format!("sess-{}", std::process::id());
                self.st.lock().unwrap().session.clone_from(&sid);
                if let Err(e) = std::fs::write(self.transcript(&sid), b"") {
                    return self.fail(id, &e.to_string());
                }
                self.reply(id, json!({"sessionId": sid, "configOptions": self.config_options()}));
                if self.scenario == "permidle" {
                    let fake = Arc::clone(self);
                    drop(thread::spawn(move || fake.permission("run_command", "execute", json!({"CommandLine": "ls"}), None)));
                }
            }
            "session/load" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_owned();
                if !self.transcript(&sid).exists() {
                    return self.fail(id, &format!("session not found: {sid}"));
                }
                self.st.lock().unwrap().session = sid;
                // Like agy: the history is replayed before the response.
                self.update(json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "OLD QUESTION"}}));
                self.chunk("OLD ANSWER");
                self.reply(id, json!({"configOptions": self.config_options()}));
            }
            "session/set_config_option" => {
                if params["configId"] == "model" {
                    let value = params["value"].as_str().unwrap_or("").to_owned();
                    if value != "gemini-big" && value != "gemini-small" {
                        return self.fail(id, &format!("unknown model {value}"));
                    }
                    self.st.lock().unwrap().model = value;
                }
                self.reply(id, json!({"configOptions": self.config_options()}));
            }
            other => self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown {other}")}})),
        }
    }

    /// Asks the client and returns its answer.
    fn permission(&self, name: &str, kind: &str, input: Value, locations: Option<Value>) -> Value {
        let (tx, rx) = channel();
        let (id, sid) = {
            let mut st = self.st.lock().unwrap();
            st.next += 1;
            let id = 900 + st.next;
            st.replies.insert(id.to_string(), tx);
            (id, st.session.clone())
        };
        let mut tc = json!({"toolCallId": format!("perm-{id}"), "name": name, "kind": kind, "title": name, "rawInput": input, "status": "pending"});
        if let Some(l) = locations {
            tc["locations"] = l;
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {"sessionId": sid, "toolCall": tc,
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}}));
        rx.recv_timeout(Duration::from_secs(20))
            .unwrap_or(Value::Null)
    }

    fn done(&self, id: &Value, stop: &str) {
        self.reply(id, json!({"stopReason": stop, "usage": {"inputTokens": 100, "outputTokens": 10, "cachedReadTokens": 40,
            "cachedWriteTokens": 0, "thoughtTokens": 5, "totalTokens": 155}}));
    }

    fn wait_cancel(&self) {
        let mut st = self.st.lock().unwrap();
        while !st.cancelled {
            st = self.cancel.wait(st).unwrap();
        }
    }

    fn prompt(&self, id: &Value, params: &Value) {
        let text = params["prompt"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let (kind, arg) = match text.find([':', '\n', ' ']) {
            Some(i) => (
                text[..i].to_owned(),
                text[i + 1..].split('\n').next().unwrap_or("").to_owned(),
            ),
            None => (text.clone(), String::new()),
        };
        let repair = text.starts_with("Your previous answer");
        let (last, sid) = {
            let mut st = self.st.lock().unwrap();
            st.cancelled = false;
            if !repair {
                st.last_kind.clone_from(&kind);
            }
            (st.last_kind.clone(), st.session.clone())
        };
        if repair {
            self.chunk(if last == "neverjson" {
                "still prose"
            } else {
                r#"{"steps":["fixed"]}"#
            });
            return self.done(id, "end_turn");
        }
        match kind.as_str() {
            "ok" | "remember" => {
                if kind == "remember"
                    && let Err(e) = std::fs::write(self.transcript(&sid), &arg)
                {
                    return self.fail(id, &e.to_string());
                }
                self.chunk("Let me look. ");
                let input = json!({"CommandLine": "cat a.txt", "toolSummary": "Read a.txt", "WaitMsBeforeAsync": 5000});
                self.update(json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "name": "run_command", "kind": "execute", "title": "cat a.txt", "status": "in_progress", "rawInput": input}));
                self.update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "name": "run_command", "kind": "execute", "status": "pending", "rawInput": input}));
                self.update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "name": "run_command", "kind": "execute", "status": "completed", "rawInput": input,
                    "rawOutput": {"exit_code": 0, "output": "file body"}}));
                self.update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed"})); // repeated
                self.update(
                    json!({"sessionUpdate": "usage_update", "used": 155, "size": 1_048_576}),
                );
                self.chunk("Hel");
                self.chunk("lo");
                self.done(id, "end_turn");
            }
            "recall" => {
                let raw = std::fs::read_to_string(self.transcript(&sid)).unwrap_or_default();
                self.chunk(&format!("recalled {raw}"));
                self.done(id, "end_turn");
            }
            "burst" => {
                // Many updates immediately followed by the response (the barrier).
                for i in 0..500 {
                    self.chunk(&(i % 10).to_string());
                }
                self.done(id, "end_turn");
            }
            "perm" | "edit" | "askuser" => {
                let (name, tk, input, locations) = match kind.as_str() {
                    "edit" => (
                        "write_file",
                        "edit",
                        json!({"Path": "/w/a.go", "Content": "x"}),
                        Some(json!([{"path": "/w/a.go"}])),
                    ),
                    "askuser" => (
                        "mcp_openchef_ask_user",
                        "other",
                        json!({"question": "colour?"}),
                        None,
                    ),
                    _ => (
                        "run_command",
                        "execute",
                        json!({"CommandLine": "go build ./...", "BypassSandbox": true, "toolSummary": "Build"}),
                        None,
                    ),
                };
                let answer = outcome(&self.permission(name, tk, input, locations));
                self.chunk(&format!("decided {answer}"));
                self.done(id, "end_turn");
            }
            "hangperm" => {
                let answer = outcome(&self.permission(
                    "run_command",
                    "execute",
                    json!({"CommandLine": "sleep 100", "BypassSandbox": true}),
                    None,
                ));
                self.record(&json!({"permission_outcome": answer}));
                self.done(id, "cancelled");
            }
            "hang" => {
                self.chunk("working");
                self.wait_cancel();
                self.done(id, "cancelled");
            }
            "stubborn" => {
                self.chunk("working");
                loop {
                    thread::sleep(Duration::from_secs(3600)); // ignores session/cancel: the adapter must kill
                }
            }
            "crash" => {
                eprintln!("boom");
                std::process::exit(3);
            }
            "quota" => self.fail(id, "RESOURCE_EXHAUSTED: You have exhausted your quota"),
            "json" => {
                self.chunk("```json\n{\"steps\":[\"a\",\"b\"]}\n```");
                self.done(id, "end_turn");
            }
            "badjson" | "neverjson" => {
                self.chunk("Sure, the plan is to do step a.");
                self.done(id, "end_turn");
            }
            "flood" => {
                for _ in 0..3000 {
                    self.chunk("x");
                }
                self.wait_cancel();
                self.done(id, "cancelled");
            }
            "refuse" => self.done(id, "refusal"),
            _ => {
                self.chunk(&format!("unknown {text}"));
                self.done(id, "end_turn");
            }
        }
    }
}

fn outcome(r: &Value) -> String {
    if r["outcome"]["outcome"] == "cancelled" {
        return "cancelled".to_owned();
    }
    r["outcome"]["optionId"].as_str().unwrap_or("").to_owned()
}

fn main() {
    let log = std::env::var("OPENCHEF_FAKE_LOG")
        .ok()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok())
        .map(Mutex::new);
    let fake = Arc::new(Fake {
        scenario: std::env::var("OPENCHEF_FAKE_AGY").unwrap_or_default(),
        state_dir: PathBuf::from(std::env::var("OPENCHEF_FAKE_STATE").unwrap_or_default()),
        out: Mutex::new(std::io::stdout()),
        log,
        st: Mutex::new(State {
            model: "gemini-big".to_owned(),
            ..State::default()
        }),
        cancel: Condvar::new(),
    });
    let argv: Vec<String> = std::env::args().skip(1).collect();
    fake.record(&json!({"argv": argv}));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        fake.record(&json!({"in": m}));
        let method = m["method"].as_str().unwrap_or("").to_owned();
        let has_id = !m["id"].is_null();
        if method.is_empty() && has_id {
            // An answer to the fake's request.
            if let Some(tx) = fake.st.lock().unwrap().replies.remove(&m["id"].to_string()) {
                let _ = tx.send(m["result"].clone());
            }
        } else if method == "session/cancel" {
            fake.st.lock().unwrap().cancelled = true;
            fake.cancel.notify_all();
        } else if has_id && method == "session/prompt" {
            let fake = Arc::clone(&fake);
            drop(thread::spawn(move || fake.prompt(&m["id"], &m["params"])));
        } else if has_id {
            fake.handle(&m["id"], &method, &m["params"]);
        }
    }
}

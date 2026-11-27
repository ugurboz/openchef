//! A scripted `claude -p` in stream-json mode for the adapter tests (port of the Go
//! `fake_test.go`). It mirrors the behaviour recorded in `testdata/claude`:
//!
//! - `--resume` fails without a transcript ("No conversation found"); `--session-id`
//!   fails when one exists. The transcript is created when the first message runs.
//! - one result per user message; `result_index` counts per process from 0.
//! - interrupt answers `{"still_queued":[]}` and ends the running turn with
//!   `error_during_execution` / `aborted_streaming`.
//!
//! The user message selects the turn (see `fake_turn`). Environment:
//! `OPENCHEF_FAKE_CLAUDE` ("1", or "permidle" for a permission request outside any
//! turn), `OPENCHEF_FAKE_LOG` (argv and every stdin line), `OPENCHEF_FAKE_STATE`
//! (transcript directory).
// A test fixture, not product code: it ignores write failures (the adapter under test
// may have gone) and favours one readable script over small functions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test fixture"
)]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

const APPROVER_STARTED: &str = "approver-started";

#[derive(Default)]
struct State {
    index: u64,
    running: String,
    pending: u32,
    interrupt: Option<Sender<()>>,
    replies: HashMap<String, Sender<Value>>,
}

struct Fake {
    scenario: String,
    schema: bool,
    transcript: PathBuf,
    state_dir: PathBuf,
    out: Mutex<std::io::Stdout>,
    log: Option<Mutex<std::fs::File>>,
    state: Mutex<State>,
    idle: Condvar,
}

impl Fake {
    fn record(&self, v: &Value) {
        if let Some(log) = &self.log {
            let mut f = log.lock().unwrap();
            let _ = writeln!(f, "{v}");
        }
    }

    fn send(&self, v: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn control_ok(&self, id: &str, resp: Value) {
        self.send(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": resp}}));
    }

    fn on_interrupt(&self, id: &str) {
        let st = self.state.lock().unwrap();
        let queued = st.running == "queued" || (st.running.is_empty() && st.pending > 0);
        self.control_ok(
            id,
            json!({"still_queued": if queued { json!(["msg-1"]) } else { json!([]) }}),
        );
        if !queued
            && !st.running.is_empty()
            && let Some(tx) = &st.interrupt
        {
            let _ = tx.send(());
        }
    }

    /// Sends `can_use_tool` and waits for the host's decision.
    fn permission(
        &self,
        id: &str,
        tool: &str,
        input: Value,
        sent: Option<Sender<()>>,
    ) -> Option<Value> {
        let (tx, rx) = channel();
        self.state.lock().unwrap().replies.insert(id.to_owned(), tx);
        self.send(&json!({"type": "control_request", "request_id": id, "request": {
            "subtype": "can_use_tool", "tool_name": tool, "input": input, "tool_use_id": format!("tu-{id}"),
            "description": "fake request", "blocked_path": "/work/x.txt"}}));
        if let Some(s) = sent {
            let _ = s.send(());
        }
        rx.recv_timeout(Duration::from_secs(20)).ok()
    }

    fn result(&self, subtype: &str, is_error: bool, text: &str, extra: Value) {
        let idx = {
            let mut st = self.state.lock().unwrap();
            st.index += 1;
            st.index - 1
        };
        let mut r = json!({"type": "result", "subtype": subtype, "is_error": is_error, "result": text,
            "result_index": idx, "duration_ms": 42, "total_cost_usd": 0.001, "permission_denials": [],
            "modelUsage": {"claude-haiku-5-5": {}},
            "usage": {"input_tokens": 100, "output_tokens": 10, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 5}});
        if let Value::Object(extra) = extra {
            for (k, v) in extra {
                r[k] = v;
            }
        }
        self.send(&r);
    }

    fn rate_limit(&self, status: &str, util: f64) {
        let reset = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        self.send(&json!({"type": "rate_limit_event", "rate_limit_info": {"status": status, "overageStatus": "rejected",
            "unifiedWindows": {"five_hour": {"status": status, "utilization": util, "resetsAt": reset}}}}));
    }

    fn delta(&self, text: &str) {
        self.send(
            &json!({"type": "stream_event", "parent_tool_use_id": null, "event": {
            "type": "content_block_delta", "delta": {"type": "text_delta", "text": text}}}),
        );
    }

    fn aborted(&self) {
        self.result(
            "error_during_execution",
            true,
            "",
            json!({"terminal_reason": "aborted_streaming"}),
        );
    }

    fn fake_turn(&self, text: &str, interrupt: &Receiver<()>) {
        let (kind, arg) = text.split_once(':').unwrap_or((text, ""));
        match kind {
            "ok" | "remember" => {
                if kind == "remember" {
                    std::fs::write(&self.transcript, arg).unwrap();
                }
                self.rate_limit("allowed", 0.25);
                self.delta("Hel");
                self.delta("lo");
                let tool_use = json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/work/a"}});
                for _ in 0..2 {
                    // repeated, as with partial messages
                    self.send(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [tool_use]}}));
                }
                self.send(
                    &json!({"type": "user", "parent_tool_use_id": null, "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "file body"}]}}),
                );
                // A subagent's message must be ignored.
                self.send(&json!({"type": "assistant", "parent_tool_use_id": "t1", "message": {"content": [
                    {"type": "tool_use", "id": "sub1", "name": "Bash", "input": {}}]}}));
                if self.schema {
                    self.send(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
                        {"type": "tool_use", "id": "so1", "name": "StructuredOutput", "input": {"ok": true}}]}}));
                    self.send(
                        &json!({"type": "user", "parent_tool_use_id": null, "message": {"content": [
                        {"type": "tool_result", "tool_use_id": "so1", "content": "ok"}]}}),
                    );
                    self.result(
                        "success",
                        false,
                        r#"{"ok":true}"#,
                        json!({"structured_output": {"ok": true}}),
                    );
                } else {
                    self.result("success", false, "Hello", json!({}));
                }
            }
            "recall" => {
                let raw = std::fs::read_to_string(&self.transcript).unwrap_or_default();
                self.result("success", false, &format!("recalled {raw}"), json!({}));
            }
            "perm" | "askuser" => {
                let (tool, input) = if kind == "askuser" {
                    ("mcp__openchef__ask_user", json!({"question": "colour?"}))
                } else {
                    ("Bash", json!({"command": "rm -rf build"}))
                };
                let resp = self
                    .permission("perm-1", tool, input, None)
                    .unwrap_or(Value::Null);
                let behavior = resp["behavior"].as_str().unwrap_or("").to_owned();
                let denials = if behavior == "allow" {
                    json!([])
                } else {
                    json!([{"tool_name": tool}])
                };
                self.result(
                    "success",
                    false,
                    &format!("decided {behavior}"),
                    json!({"permission_denials": denials}),
                );
            }
            "cancelperm" => {
                // The request must be on the wire before the cancel, and the cancel must
                // wait until the test's approver is really waiting.
                let (sent_tx, sent_rx) = channel();
                let fake = self;
                thread::scope(|s| {
                    let _ = s.spawn(move || {
                        fake.permission(
                            "perm-2",
                            "Bash",
                            json!({"command": "sleep 100"}),
                            Some(sent_tx),
                        )
                    });
                    let _ = sent_rx.recv();
                    let marker = self.state_dir.join(APPROVER_STARTED);
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while Instant::now() < deadline && !marker.exists() {
                        thread::sleep(Duration::from_millis(10));
                    }
                    self.send(&json!({"type": "control_cancel_request", "request_id": "perm-2"}));
                    thread::sleep(Duration::from_millis(200));
                    self.result("success", false, "cancelled", json!({}));
                    // Release the waiting permission thread.
                    if let Some(tx) = self.state.lock().unwrap().replies.remove("perm-2") {
                        let _ = tx.send(Value::Null);
                    }
                });
            }
            "hang" => {
                self.delta("working");
                let _ = interrupt.recv();
                self.aborted();
            }
            "stubborn" => {
                self.delta("working");
                let _ = interrupt.recv();
                loop {
                    thread::sleep(Duration::from_secs(3600)); // never answers: the adapter must kill
                }
            }
            "queued" => loop {
                thread::sleep(Duration::from_secs(3600)); // the interrupt reports the message as still queued
            },
            "crash" => {
                eprintln!("boom");
                std::process::exit(3);
            }
            "stale" => {
                self.send(&json!({"type": "result", "subtype": "success", "is_error": false, "result": "STALE", "result_index": 99}));
                self.result("success", false, "Hello", json!({}));
            }
            "exhausted" => {
                self.rate_limit("rejected", 1.0);
                self.result(
                    "success",
                    true,
                    "Claude AI usage limit reached",
                    json!({"api_error_status": 429}),
                );
            }
            "flood" => {
                for _ in 0..3000 {
                    self.delta("x");
                }
                let _ = interrupt.recv();
                self.aborted();
            }
            _ => self.result(
                "success",
                false,
                &format!("unknown command {text}"),
                json!({}),
            ),
        }
    }
}

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> ExitCode {
    let scenario = std::env::var("OPENCHEF_FAKE_CLAUDE").unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = std::env::var("OPENCHEF_FAKE_LOG")
        .ok()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok())
        .map(Mutex::new);
    let state_dir = PathBuf::from(std::env::var("OPENCHEF_FAKE_STATE").unwrap_or_default());
    let (id, resume) = match arg_after(&args, "--resume") {
        Some(id) => (id, true),
        None => (arg_after(&args, "--session-id").unwrap_or_default(), false),
    };
    let fake = Arc::new(Fake {
        scenario,
        schema: args.iter().any(|a| a == "--json-schema"),
        transcript: state_dir.join(&id),
        state_dir,
        out: Mutex::new(std::io::stdout()),
        log,
        state: Mutex::new(State::default()),
        idle: Condvar::new(),
    });
    fake.record(&json!({"pid": std::process::id(), "argv": args}));
    let exists = Path::new(&fake.transcript).exists();
    if resume && !exists {
        eprintln!("No conversation found with session ID: {id}");
        return ExitCode::from(1);
    }
    if !resume && exists {
        eprintln!("Error: Session ID {id} is already in use.");
        return ExitCode::from(1);
    }

    let (queue_tx, queue_rx) = channel::<String>();
    let worker = {
        let fake = Arc::clone(&fake);
        thread::spawn(move || {
            for text in queue_rx {
                let (itx, irx) = channel();
                {
                    let mut st = fake.state.lock().unwrap();
                    text.split(':')
                        .next()
                        .unwrap_or("")
                        .clone_into(&mut st.running);
                    st.pending -= 1;
                    st.interrupt = Some(itx);
                }
                if !text.starts_with("queued") {
                    // An empty transcript: the session now exists.
                    let _ = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&fake.transcript);
                }
                fake.fake_turn(&text, &irx);
                fake.state.lock().unwrap().running.clear();
                fake.idle.notify_all();
            }
        })
    };

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let msg: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        fake.record(&json!({"pid": std::process::id(), "in": msg}));
        match msg["type"].as_str().unwrap_or("") {
            "control_request" => match msg["request"]["subtype"].as_str().unwrap_or("") {
                "initialize" => {
                    fake.control_ok(msg["request_id"].as_str().unwrap_or(""), json!({"commands": []}));
                    if fake.scenario == "permidle" {
                        let fake = Arc::clone(&fake);
                        drop(thread::spawn(move || fake.permission("idle-1", "Bash", json!({"command": "ls"}), None)));
                    }
                }
                "interrupt" => fake.on_interrupt(msg["request_id"].as_str().unwrap_or("")),
                _ => fake.send(&json!({"type": "control_response", "response": {"subtype": "error", "request_id": msg["request_id"], "error": "unknown"}})),
            },
            "control_response" => {
                let id = msg["response"]["request_id"].as_str().unwrap_or("").to_owned();
                if let Some(tx) = fake.state.lock().unwrap().replies.remove(&id) {
                    let _ = tx.send(msg["response"]["response"].clone());
                }
            }
            "user" => {
                let text = msg["message"]["content"][0]["text"].as_str().unwrap_or("").to_owned();
                fake.state.lock().unwrap().pending += 1;
                let _ = queue_tx.send(text);
            }
            _ => {}
        }
    }
    // EOF: like Claude, finish what is running, then exit (queued messages are dropped).
    drop(queue_tx);
    drop(worker);
    let mut st = fake.state.lock().unwrap();
    while !st.running.is_empty() {
        st = fake.idle.wait(st).unwrap();
    }
    ExitCode::SUCCESS
}

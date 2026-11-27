//! Ported from Go `internal/mcp/askuser_test.go`: the `ask_user` MCP server on pipes,
//! driven by a JSON-RPC client the way agent CLIs drive it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openchef_api::mcp::{Asker, MAX_QUESTION, SERVER_NAME, TOOL_NAME, serve};
use openchef_runtime::BoxFuture;
use openchef_runtime::linerpc::{self, CODE_METHOD_NOT_FOUND, LineSink, Peer};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Answers every question with `answer` (or fails with `err`) and records what it was
/// asked; `block` waits until the call is cancelled.
#[derive(Default)]
struct FakeAsker {
    answer: Option<String>,
    err: Option<String>,
    block: bool,
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

impl Asker for FakeAsker {
    fn ask(
        &self,
        question: String,
        options: Vec<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Option<String>, String>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((question, options));
        let (answer, err, block) = (self.answer.clone(), self.err.clone(), self.block);
        Box::pin(async move {
            if block {
                cancel.cancelled().await;
                return Err("cancelled".to_owned());
            }
            err.map_or(Ok(answer), Err)
        })
    }
}

/// The client's side of the server's input; `close` hangs up like an exiting agent.
struct PipeSink(Mutex<Option<io::PipeWriter>>);

impl PipeSink {
    fn close(&self) {
        drop(self.0.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
}

impl LineSink for PipeSink {
    fn send_line(&self, mut line: Vec<u8>) -> BoxFuture<'static, io::Result<()>> {
        line.push(b'\n');
        let r = match self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            Some(w) => w.write_all(&line),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        };
        Box::pin(async move { r })
    }
}

/// The server on pipes, and a client for it. Dropping the client's writer ends the
/// server's input.
struct Running {
    client: Peer,
    to_server: Arc<PipeSink>,
    served: tokio::task::JoinHandle<Result<(), openchef_api::Error>>,
}

fn start(asker: Arc<FakeAsker>) -> Running {
    let (server_in, client_out) = io::pipe().unwrap();
    let (client_in, server_out) = io::pipe().unwrap();
    let served = tokio::spawn(serve(server_in, server_out, asker, "test"));
    let to_server = Arc::new(PipeSink(Mutex::new(Some(client_out))));
    let (client, _notes) = Peer::new(
        client_in,
        Arc::clone(&to_server) as Arc<dyn LineSink>,
        linerpc::Options::default(),
    );
    Running {
        client,
        to_server,
        served,
    }
}

impl Running {
    async fn call(&self, method: &str, params: Value) -> Result<Value, linerpc::Error> {
        let cancel = CancellationToken::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            self.client.call_value(method, params, &cancel),
        )
        .await
        .expect("call timed out")
    }

    async fn ask(&self, args: Value) -> Value {
        self.call("tools/call", json!({"name": TOOL_NAME, "arguments": args}))
            .await
            .unwrap()
    }

    /// Closes the server's input and waits for it to return.
    async fn finish(self) {
        self.to_server.close();
        let r = tokio::time::timeout(Duration::from_secs(5), self.served)
            .await
            .expect("serve did not return after EOF")
            .unwrap();
        assert!(r.is_ok(), "serve: {r:?}");
    }
}

fn text(r: &Value) -> &str {
    r["content"][0]["text"].as_str().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_and_tool_list() {
    let s = start(Arc::default());
    let init = s
        .call(
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {},
                   "clientInfo": {"name": "claude-code"}}),
        )
        .await
        .unwrap();
    assert_eq!(init["protocolVersion"], "2025-03-26");
    assert_eq!(init["serverInfo"]["name"], SERVER_NAME);
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    s.client
        .notify("notifications/initialized", Value::Null)
        .await
        .unwrap();
    let unknown = s
        .call("initialize", json!({"protocolVersion": "1999-01-01"}))
        .await
        .unwrap();
    assert_eq!(unknown["protocolVersion"], "2025-06-18", "our newest");
    let list = s.call("tools/list", Value::Null).await.unwrap();
    // Only ask_user: every tool a server lists is offered to the model.
    let tools = list["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], TOOL_NAME);
    s.call("ping", Value::Null).await.unwrap();
    match s.call("resources/list", Value::Null).await {
        Err(linerpc::Error::Rpc { error, .. }) => assert_eq!(error.code, CODE_METHOD_NOT_FOUND),
        other => panic!("unknown method: {other:?}"),
    }
    assert!(
        matches!(
            s.call("tools/call", json!({"name": "approve"})).await,
            Err(linerpc::Error::Rpc { .. })
        ),
        "unknown tool"
    );
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_answers() {
    let asker = Arc::new(FakeAsker {
        answer: Some("9090".into()),
        ..FakeAsker::default()
    });
    let s = start(Arc::clone(&asker));
    let r = s
        .ask(json!({"question": "  Which port?  ", "options": ["8080", "9090"]}))
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["content"].as_array().map(Vec::len), Some(1));
    assert_eq!(text(&r), "9090");
    assert_eq!(
        asker.asked.lock().unwrap()[0],
        (
            "Which port?".to_owned(),
            vec!["8080".to_owned(), "9090".to_owned()]
        )
    );
    assert_eq!(
        s.ask(json!({"question": ""})).await["isError"],
        true,
        "empty question"
    );
    let huge = "x".repeat(MAX_QUESTION + 1);
    assert_eq!(
        s.ask(json!({"question": huge})).await["isError"],
        true,
        "huge question"
    );
    assert_eq!(
        asker.asked.lock().unwrap().len(),
        1,
        "refused questions are not asked"
    );
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_user_unanswered() {
    for asker in [
        FakeAsker::default(),
        FakeAsker {
            err: Some("daemon not running".into()),
            ..FakeAsker::default()
        },
    ] {
        let s = start(Arc::new(asker));
        let r = s.ask(json!({"question": "?"})).await;
        assert_eq!(r["isError"], false, "{r}");
        assert!(text(&r).contains("Make the most reasonable choice"), "{r}");
        s.finish().await;
    }
}

/// When the agent goes away mid-question, the server stops without blocking and the
/// question is cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn serve_ends_with_input() {
    let (server_in, mut client_out) = io::pipe().unwrap();
    let asker = Arc::new(FakeAsker {
        block: true,
        ..FakeAsker::default()
    });
    let served = tokio::spawn(serve(
        server_in,
        io::sink(),
        Arc::clone(&asker) as Arc<dyn Asker>,
        "test",
    ));
    client_out
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"?"}}}"#,
        )
        .unwrap();
    client_out.write_all(b"\n").unwrap();
    // Wait until the question is being asked, then hang up.
    tokio::time::timeout(Duration::from_secs(5), async {
        while asker.asked.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the question never arrived");
    drop(client_out);
    let r = tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("serve did not return after EOF")
        .unwrap();
    assert!(r.is_ok(), "{r:?}");
}

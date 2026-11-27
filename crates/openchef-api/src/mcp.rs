//! OpenChef's stdio MCP server (D-024), ported from Go `internal/mcp`. It offers
//! exactly one tool, `ask_user`, through which agents put questions to the user (§9.2;
//! M1 findings: every tool a server lists is offered to the model, so nothing else is
//! exposed). Answers come from the daemon's approval queue.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

use openchef_runtime::BoxFuture;
use openchef_runtime::linerpc::{self, CODE_METHOD_NOT_FOUND, Handler, LineSink, Peer, RpcError};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{CODE_INVALID_PARAMS, Error};

/// The server's name: Claude sees the tool as `mcp__openchef__ask_user`.
pub const SERVER_NAME: &str = "openchef";
/// The one tool.
pub const TOOL_NAME: &str = "ask_user";

/// The tool's result when nobody answers.
pub const UNANSWERED: &str = "No one is available to answer right now. Make the most \
    reasonable choice, say which one you made in your final answer, and continue.";

/// The longest question accepted, in characters.
pub const MAX_QUESTION: usize = 4000;

/// The MCP revisions this server speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Puts a question to the user. `Ok(None)` means nobody answered; `Err` that the user
/// could not be reached. Must stop when `cancel` fires.
pub trait Asker: Send + Sync + 'static {
    /// Asks and waits for the answer.
    fn ask(
        &self,
        question: String,
        options: Vec<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Option<String>, String>>;
}

/// Runs the server on `reader`/`writer` until the input ends. A question in flight
/// when the agent goes away is cancelled, which withdraws it from the queue.
///
/// # Errors
///
/// [`Error::Mcp`] when the input could not be read.
pub async fn serve(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    asker: Arc<dyn Asker>,
    version: &str,
) -> Result<(), Error> {
    let version = version.to_owned();
    let handler: Handler = Arc::new(move |method, params, cancel| {
        handle(&method, params, cancel, Arc::clone(&asker), &version)
    });
    let sink = Arc::new(WriterSink(Mutex::new(Box::new(writer))));
    let (peer, mut notifications) = Peer::new(
        reader,
        sink,
        linerpc::Options {
            handler: Some(handler),
            ..linerpc::Options::default()
        },
    );
    // `initialized` and `cancelled` need nothing: a cancelled call ends with the input.
    while notifications.recv().await.is_some() {}
    peer.closed().cancelled().await;
    match peer.stop_error() {
        None => Ok(()),
        Some(e) => Err(Error::Mcp(e)),
    }
}

/// Writes each line to the writer, in order. The lock guards one small write and is
/// never held across `.await` (D-030).
struct WriterSink(Mutex<Box<dyn Write + Send>>);

impl LineSink for WriterSink {
    fn send_line(&self, mut line: Vec<u8>) -> BoxFuture<'static, io::Result<()>> {
        line.push(b'\n');
        let written = {
            let mut w = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            w.write_all(&line).and_then(|()| w.flush())
        };
        Box::pin(async move { written })
    }
}

fn handle(
    method: &str,
    params: Value,
    cancel: CancellationToken,
    asker: Arc<dyn Asker>,
    version: &str,
) -> BoxFuture<'static, Result<Value, RpcError>> {
    let ready = |v: Result<Value, RpcError>| -> BoxFuture<'static, Result<Value, RpcError>> {
        Box::pin(async move { v })
    };
    match method {
        "initialize" => ready(Ok(initialize(&params, version))),
        "ping" => ready(Ok(json!({}))),
        "tools/list" => ready(Ok(json!({ "tools": [tool_spec()] }))),
        "tools/call" => match call_args(params) {
            Ok(Call::Ask { question, options }) => Box::pin(async move {
                let answered = asker.ask(question, options, cancel.clone()).await;
                if cancel.is_cancelled() {
                    return Err(RpcError::server("cancelled"));
                }
                Ok(match answered {
                    Ok(Some(answer)) if !answer.is_empty() => tool_result(&answer, false),
                    Ok(_) => tool_result(UNANSWERED, false),
                    // The agent gets a usable answer; failing the tool call helps no one.
                    Err(e) => tool_result(
                        &format!("OpenChef could not reach the user ({e}). {UNANSWERED}"),
                        false,
                    ),
                })
            }),
            Ok(Call::Refused(text)) => ready(Ok(tool_result(&text, true))),
            Err(e) => ready(Err(e)),
        },
        other => ready(Err(RpcError {
            code: CODE_METHOD_NOT_FOUND,
            message: format!("method not found: {other}"),
            data: None,
        })),
    }
}

/// The handshake: the client's protocol revision when we speak it, else our newest.
fn initialize(params: &Value, version: &str) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let v = PROTOCOL_VERSIONS
        .iter()
        .find(|v| Some(**v) == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": v,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": version },
    })
}

fn tool_spec() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Ask the user a question when you cannot proceed without their decision. \
            Use it sparingly: only for choices that change the result. The call waits until \
            they answer.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question, self-contained." },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Suggested answers, if the choice is between a few.",
                },
            },
            "required": ["question"],
            "additionalProperties": false,
        },
    })
}

/// What a `tools/call` asks for.
enum Call {
    /// A question to put to the user.
    Ask {
        question: String,
        options: Vec<String>,
    },
    /// A call the tool refuses, with the reason for the agent.
    Refused(String),
}

#[derive(Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: AskArguments,
}

#[derive(Default, Deserialize)]
struct AskArguments {
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

fn call_args(params: Value) -> Result<Call, RpcError> {
    let invalid = |message: String| RpcError {
        code: CODE_INVALID_PARAMS,
        message,
        data: None,
    };
    let p: CallParams =
        serde_json::from_value(params).map_err(|e| invalid(format!("invalid params: {e}")))?;
    if p.name != TOOL_NAME {
        return Err(invalid(format!("unknown tool: {}", p.name)));
    }
    let question = p.arguments.question.trim();
    if question.is_empty() || question.chars().count() > MAX_QUESTION {
        return Ok(Call::Refused(format!(
            "{TOOL_NAME} needs a question of 1 to {MAX_QUESTION} characters."
        )));
    }
    Ok(Call::Ask {
        question: question.to_owned(),
        options: p.arguments.options,
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

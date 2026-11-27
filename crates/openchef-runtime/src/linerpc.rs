//! A JSON-RPC 2.0 peer over newline-delimited JSON, the framing of `codex app-server`
//! and `agy acp`. It matches responses to calls, delivers notifications in order
//! without ever dropping one (D-001), and answers requests the agent sends to us
//! (approvals, questions) through a [`Handler`].
//!
//! A reader thread owns the CLI's stdout; writes go through a [`LineSink`] (the
//! process's [`openchef_procs::StdinWriter`]), which serializes them.

use std::collections::HashMap;
use std::io::{self, BufReader, Read};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::BoxFuture;
use crate::rawlog::{Dir, RawLog};

/// Largest single message (64 MiB): room for diffs and file contents, a stop for a
/// runaway stream.
pub const DEFAULT_MAX_LINE: usize = 64 << 20;

/// Method not found.
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Internal error.
pub const CODE_INTERNAL: i64 = -32603;
/// Generic server error.
pub const CODE_SERVER: i64 = -32000;

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("json-rpc error {code}: {message}")]
pub struct RpcError {
    /// The error code.
    pub code: i64,
    /// The message.
    pub message: String,
    /// Extra data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// A server error with a message.
    #[must_use]
    pub fn server(message: impl Into<String>) -> Self {
        Self {
            code: CODE_SERVER,
            message: message.into(),
            data: None,
        }
    }
}

/// Why a call or notification failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The peer's input ended; pending and future calls fail with this.
    #[error("{method}: connection closed")]
    Closed {
        /// The method.
        method: String,
    },
    /// The remote side answered with an error.
    #[error("{method}: {error}")]
    Rpc {
        /// The method.
        method: String,
        /// The error object.
        error: RpcError,
    },
    /// The call was cancelled before its response arrived.
    #[error("{method}: cancelled")]
    Cancelled {
        /// The method.
        method: String,
    },
    /// The result did not have the expected shape.
    #[error("{method}: decode result: {message}")]
    Decode {
        /// The method.
        method: String,
        /// What serde said.
        message: String,
    },
    /// Writing to the CLI failed.
    #[error("{method}: write: {source}")]
    Write {
        /// The method.
        method: String,
        /// The I/O error.
        #[source]
        source: io::Error,
    },
}

/// Where outgoing lines go. Implementations write one line (a newline is appended)
/// and keep lines in the order `send_line` is awaited.
pub trait LineSink: Send + Sync + 'static {
    /// Writes `line` followed by a newline.
    fn send_line(&self, line: Vec<u8>) -> BoxFuture<'static, io::Result<()>>;
}

impl LineSink for openchef_procs::StdinWriter {
    fn send_line(&self, mut line: Vec<u8>) -> BoxFuture<'static, io::Result<()>> {
        line.push(b'\n');
        let writer = self.clone();
        Box::pin(async move { writer.write(line).await.map_err(io::Error::other) })
    }
}

/// Answers a request the remote side initiated. `cancel` fires when the peer closes.
pub type Handler = Arc<
    dyn Fn(String, Value, CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>>
        + Send
        + Sync,
>;

/// Called for lines that are not valid JSON-RPC; the peer keeps running and the
/// adapter decides whether that is fatal.
pub type OnProtocolError = Arc<dyn Fn(&[u8], &str) + Send + Sync>;

/// Peer options.
#[derive(Clone, Default)]
pub struct Options {
    /// Protocol log.
    pub log: Option<RawLog>,
    /// Answers remote requests; without one they get "method not found".
    pub handler: Option<Handler>,
    /// Overrides [`DEFAULT_MAX_LINE`].
    pub max_line: Option<usize>,
    /// Reports malformed lines.
    pub on_protocol_error: Option<OnProtocolError>,
}

/// A message without an id.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// The method.
    pub method: String,
    /// The parameters (`null` when absent).
    pub params: Value,
}

enum Inbound {
    Note(Notification),
    Fence(oneshot::Sender<()>),
}

/// Notifications in arrival order. Ends after the peer closed and every received
/// notification was delivered.
pub struct Notifications {
    rx: mpsc::UnboundedReceiver<Inbound>,
}

impl Notifications {
    /// The next notification. Barrier markers are released here, invisibly.
    pub async fn recv(&mut self) -> Option<Notification> {
        loop {
            match self.rx.recv().await? {
                Inbound::Note(n) => return Some(n),
                // The barrier's caller may have given up waiting; nothing to release then.
                Inbound::Fence(done) => drop(done.send(())),
            }
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
struct Message {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

type Response = Result<Value, RpcError>;

struct Inner {
    sink: Arc<dyn LineSink>,
    log: RawLog,
    next_id: AtomicI64,
    // Calls waiting for their response: leaf state, locked only to insert or take one
    // entry (D-030).
    pending: Mutex<HashMap<String, oneshot::Sender<Response>>>,
    // `None` once the input ended.
    inbound: Mutex<Option<mpsc::UnboundedSender<Inbound>>>,
    closed: CancellationToken,
    stop_error: Mutex<Option<String>>,
    drained: watch::Receiver<bool>,
}

/// One side of a JSON-RPC connection. Cheap to clone.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("closed", &self.inner.closed.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Peer {
    /// Starts reading `reader` on its own thread and writing through `sink`. Must be
    /// called inside a Tokio runtime (request handlers run on it).
    pub fn new(
        reader: impl Read + Send + 'static,
        sink: Arc<dyn LineSink>,
        options: Options,
    ) -> (Self, Notifications) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (drained_tx, drained_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            sink,
            log: options.log.clone().unwrap_or_else(RawLog::discard),
            next_id: AtomicI64::new(0),
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(Some(tx)),
            closed: CancellationToken::new(),
            stop_error: Mutex::new(None),
            drained: drained_rx,
        });
        let peer = Self { inner };
        let reading = peer.clone();
        let handle = Handle::current();
        let spawned = thread::Builder::new()
            .name("openchef-linerpc-reader".to_owned())
            .spawn(move || reading.read_loop(reader, &options, &handle, drained_tx));
        if let Err(e) = spawned {
            peer.inner
                .log
                .note(Dir::Note, &format!("linerpc: cannot start reader: {e}"));
            peer.inner.closed.cancel();
        }
        (peer, Notifications { rx })
    }

    /// Fires when the input has ended (EOF, read error or an oversize message).
    #[must_use]
    pub fn closed(&self) -> CancellationToken {
        self.inner.closed.clone()
    }

    /// Why the peer stopped: `None` for a clean EOF or while running.
    #[must_use]
    pub fn stop_error(&self) -> Option<String> {
        self.inner
            .stop_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sends a request and waits for its result.
    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        cancel: &CancellationToken,
    ) -> Result<R, Error> {
        let raw = self.call_value(method, params, cancel).await?;
        serde_json::from_value(raw).map_err(|e| Error::Decode {
            method: method.to_owned(),
            message: e.to_string(),
        })
    }

    /// [`Peer::call`] returning the raw result.
    pub async fn call_value(
        &self,
        method: &str,
        params: impl Serialize,
        cancel: &CancellationToken,
    ) -> Result<Value, Error> {
        let closed = || Error::Closed {
            method: method.to_owned(),
        };
        if self.inner.closed.is_cancelled() {
            return Err(closed());
        }
        let n = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = n.to_string();
        let (tx, rx) = oneshot::channel();
        let _previous = self.pending().insert(id.clone(), tx);
        let message = Message {
            id: Some(Value::from(n)),
            method: method.to_owned(),
            params: to_value(params),
            ..Message::default()
        };
        if let Err(source) = self.send(message).await {
            drop(self.pending().remove(&id));
            return Err(Error::Write {
                method: method.to_owned(),
                source,
            });
        }
        tokio::select! {
            response = rx => match response {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(Error::Rpc { method: method.to_owned(), error }),
                Err(_) => Err(closed()),
            },
            () = cancel.cancelled() => {
                drop(self.pending().remove(&id));
                Err(Error::Cancelled { method: method.to_owned() })
            }
        }
    }

    /// Sends a notification.
    pub async fn notify(&self, method: &str, params: impl Serialize) -> Result<(), Error> {
        let message = Message {
            method: method.to_owned(),
            params: to_value(params),
            ..Message::default()
        };
        self.send(message).await.map_err(|source| Error::Write {
            method: method.to_owned(),
            source,
        })
    }

    /// Returns once the consumer has taken every notification read before this call.
    /// Protocols that end a turn with a response rather than a notification (ACP's
    /// `session/prompt`, D-017) use it so no update that preceded the response is lost
    /// or credited to the next turn.
    pub async fn barrier(&self, cancel: &CancellationToken) -> Result<(), Error> {
        let fence = {
            let inbound = self
                .inner
                .inbound
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            inbound.as_ref().map(|tx| {
                let (done, wait) = oneshot::channel();
                // A send error means the consumer is gone: nothing can be credited wrongly.
                drop(tx.send(Inbound::Fence(done)));
                wait
            })
        };
        let cancelled = || Error::Cancelled {
            method: "barrier".to_owned(),
        };
        if let Some(wait) = fence {
            return tokio::select! {
                _ = wait => Ok(()),
                () = cancel.cancelled() => Err(cancelled()),
            };
        }
        let mut drained = self.inner.drained.clone();
        tokio::select! {
            _ = drained.wait_for(|d| *d) => Ok(()),
            () = cancel.cancelled() => Err(cancelled()),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Response>>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn send(&self, mut message: Message) -> io::Result<()> {
        "2.0".clone_into(&mut message.jsonrpc);
        let raw = serde_json::to_vec(&message).map_err(io::Error::other)?;
        self.inner.log.line(Dir::Send, &raw);
        self.inner.sink.send_line(raw).await
    }

    fn read_loop(
        &self,
        reader: impl Read,
        options: &Options,
        handle: &Handle,
        drained: watch::Sender<bool>,
    ) {
        let max = options.max_line.unwrap_or(DEFAULT_MAX_LINE);
        let mut reader = BufReader::with_capacity(64 << 10, reader);
        let stop = loop {
            match read_line(&mut reader, max) {
                Ok(Some(line)) => {
                    if !line.is_empty() {
                        self.dispatch(&line, options, handle);
                    }
                }
                Ok(None) => break None,
                Err(e) => break Some(e.to_string()),
            }
        };
        self.finish(stop, handle, drained);
    }

    fn dispatch(&self, line: &[u8], options: &Options, handle: &Handle) {
        self.inner.log.line(Dir::Recv, line);
        let report = |reason: &str| {
            if let Some(f) = &options.on_protocol_error {
                f(line, reason);
            }
        };
        let message: Message = match serde_json::from_slice(line) {
            Ok(m) => m,
            Err(e) => return report(&format!("invalid JSON: {e}")),
        };
        match (message.method.is_empty(), message.id) {
            (false, Some(id)) => {
                let peer = self.clone();
                let handler = options.handler.clone();
                let params = message.params.unwrap_or(Value::Null);
                let method = message.method;
                drop(handle.spawn(async move { peer.answer(handler, id, method, params).await }));
            }
            (false, None) => {
                let note = Notification {
                    method: message.method,
                    params: message.params.unwrap_or(Value::Null),
                };
                if let Some(tx) = self
                    .inner
                    .inbound
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    // The consumer may have dropped the stream; then nobody wants it.
                    drop(tx.send(Inbound::Note(note)));
                }
            }
            (true, Some(id)) => {
                let key = canonical_id(&id);
                let Some(tx) = self.pending().remove(&key) else {
                    return report(&format!("response to unknown id {id}"));
                };
                let response = match message.error {
                    Some(e) => Err(e),
                    None => Ok(message.result.unwrap_or(Value::Null)),
                };
                // The caller may have given up (cancelled) in between.
                drop(tx.send(response));
            }
            (true, None) => report("message has neither method nor id"),
        }
    }

    async fn answer(&self, handler: Option<Handler>, id: Value, method: String, params: Value) {
        let result = match handler {
            None => Err(RpcError {
                code: CODE_METHOD_NOT_FOUND,
                message: format!("no handler for {method}"),
                data: None,
            }),
            Some(h) => h(method, params, self.inner.closed.clone()).await,
        };
        let (result, error) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let reply = Message {
            id: Some(id.clone()),
            result: result.or(if error.is_none() {
                Some(Value::Null)
            } else {
                None
            }),
            error,
            ..Message::default()
        };
        if let Err(e) = self.send(reply).await {
            self.inner
                .log
                .note(Dir::Note, &format!("linerpc: reply to {id} failed: {e}"));
        }
    }

    fn finish(&self, stop: Option<String>, handle: &Handle, drained: watch::Sender<bool>) {
        *self
            .inner
            .stop_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stop;
        self.inner.closed.cancel();
        // Dropping the senders fails every pending call with `Closed`.
        self.pending().clear();
        let last = self
            .inner
            .inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match last {
            Some(tx) => {
                let (done, wait) = oneshot::channel();
                drop(tx.send(Inbound::Fence(done)));
                drop(tx);
                // Released when the consumer reads past the last notification, or fails
                // when it dropped the stream: drained either way.
                drop(handle.spawn(async move {
                    drop(wait.await);
                    drained.send_replace(true);
                }));
            }
            None => {
                drained.send_replace(true);
            }
        }
    }
}

fn to_value(params: impl Serialize) -> Option<Value> {
    match serde_json::to_value(params) {
        Ok(Value::Null) | Err(_) => None,
        Ok(v) => Some(v),
    }
}

/// Makes ids comparable regardless of formatting (`7` vs `7.0` vs `"7"` stays a string).
fn canonical_id(id: &Value) -> String {
    match id {
        Value::Number(n) => n
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0)
                    .map(|f| format!("{f:.0}"))
            })
            .unwrap_or_else(|| n.to_string()),
        Value::String(s) => format!("s:{s}"),
        other @ (Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_)) => {
            other.to_string()
        }
    }
}

/// Reads one line without its `\n`/`\r\n`; `None` at EOF; an error past `max` bytes.
fn read_line(reader: &mut impl std::io::BufRead, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..i], Some(i + 1)),
            None => (available, None),
        };
        if line.len() + chunk.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message exceeds {max} bytes"),
            ));
        }
        line.extend_from_slice(chunk);
        let used = done.unwrap_or(available.len());
        reader.consume(used);
        if done.is_some() {
            if line.last() == Some(&b'\r') {
                let _cr = line.pop();
            }
            return Ok(Some(line));
        }
    }
}

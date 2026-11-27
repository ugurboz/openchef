//! The Codex session actor (D-030). One Tokio task owns the thread's state: the running
//! turn, rate limits and file-change paths. Notifications, server requests, turn/start
//! responses and deadline signals reach it as messages; protocol calls that wait run on
//! their own tasks, so the actor never blocks.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use openchef_core::{Effort, PermissionMode};
use openchef_procs::{Process, Spec, Stderr};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::protocol::{
    CommandApproval, DeltaNotification, Elicitation, ErrorNotification, FileChangeApproval,
    ItemNotification, ModelList, RateLimitSnapshot, RateLimits, ThreadResponse, TokenBreakdown,
    TokenUsageNotification, TurnCompletedNotification, TurnError, TurnStartResponse,
    TurnStartedNotification, UserInput, v2_decision,
};
use super::{CodexOptions, NAME};
use crate::linerpc::{
    self, CODE_METHOD_NOT_FOUND, Handler, Notification, Notifications, Peer, RpcError,
};
use crate::rawlog::{Dir, RawLog};
use crate::{
    ApprovalRequest, Decision, Error, EventBody, Failure, FailureKind, Question, RequestKind,
    SessionRef, SessionSpec, TurnEvents, TurnInput, TurnResult, TurnSender, nearest_effort,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const ASK_USER_TOOL: &str = "ask_user";
const CODE_INVALID_PARAMS: i64 = -32602;

enum Cmd {
    Turn {
        input: TurnInput,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<TurnEvents, Error>>,
    },
    Interrupt {
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

pub(crate) enum Internal {
    Started {
        turn: u64,
        result: Result<String, Error>,
    },
    Cancelled {
        turn: u64,
    },
    CancelSettled {
        turn: u64,
    },
    Denied {
        turn: u64,
    },
    Request {
        method: String,
        params: Value,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
}

/// One turn (D-002 ordering through [`TurnSender`]).
pub(crate) struct TurnState {
    id: u64,
    codex_id: Option<String>,
    tx: TurnSender,
    started: Instant,
    want_schema: bool,
    final_text: String,
    usage_base: Option<TokenBreakdown>,
    usage_last: TokenBreakdown,
    denials: u32,
    last_error: Option<TurnError>,
    /// Why OpenChef stopped the turn; wins over Codex's generic "interrupted", which
    /// races with it.
    stop: Option<FailureKind>,
    /// Turn-scoped notifications that arrived while the turn id was unknown. Replayed
    /// once it is known; those of other (stale) turns are then dropped.
    early: Vec<Notification>,
    done: CancellationToken,
}

impl TurnState {
    fn usage(&self) -> crate::Usage {
        self.usage_base.map_or_else(crate::Usage::default, |base| {
            self.usage_last.minus(base).usage()
        })
    }
}

/// A turn whose `turn/start` call is still in flight.
struct Starting {
    turn: u64,
    rx: TurnEvents,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<TurnEvents, Error>>,
}

pub(crate) struct Actor {
    spec: SessionSpec,
    log: RawLog,
    peer: Peer,
    process: Option<Process>,
    pub(crate) session: SessionRef,
    efforts: Vec<Effort>,
    pub(crate) limits: RateLimitSnapshot,
    pub(crate) turn: Option<TurnState>,
    starting: Option<Starting>,
    items: HashMap<String, Vec<String>>,
    closed: bool,
    next_turn: u64,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
}

/// The handle the engine holds.
pub struct CodexSession {
    session: SessionRef,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSession")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

fn start_error(message: impl Into<String>) -> Error {
    Error::Start {
        runtime: NAME,
        message: message.into(),
    }
}

fn protocol_error(reason: impl Into<String>) -> Error {
    Error::Protocol {
        runtime: NAME,
        reason: reason.into(),
        line: String::new(),
    }
}

fn call_error(e: linerpc::Error) -> Error {
    match e {
        linerpc::Error::Closed { .. } => Error::SessionClosed,
        other @ (linerpc::Error::Rpc { .. }
        | linerpc::Error::Cancelled { .. }
        | linerpc::Error::Decode { .. }
        | linerpc::Error::Write { .. }) => start_error(other.to_string()),
    }
}

async fn call<R: DeserializeOwned>(
    peer: &Peer,
    method: &str,
    params: Value,
    cancel: &CancellationToken,
) -> Result<R, Error> {
    match tokio::time::timeout(CALL_TIMEOUT, peer.call(method, params, cancel)).await {
        Ok(result) => result.map_err(call_error),
        Err(_) => Err(start_error(format!(
            "{method}: no response within {CALL_TIMEOUT:?}"
        ))),
    }
}

/// How the session starts: a new thread or a resumed one.
pub(crate) enum Thread {
    New,
    Resume(String),
}

/// Builds `thread/start` and `thread/resume` parameters. Every mode except plan uses
/// `workspace-write` with approvals routed to OpenChef (`untrusted`): the approver
/// applies the mode and the deny rules, so they hold even in bypass (§9.3). Inside
/// OpenChef's sandbox Codex's own cannot apply (sandboxes do not nest, D-032), so it
/// runs `danger-full-access` and OpenChef's sandbox is the boundary.
pub(crate) fn thread_params(spec: &SessionSpec, extra: &[(&str, Value)]) -> Value {
    let sandbox = if spec.confine.is_some() {
        "danger-full-access"
    } else if spec.mode == PermissionMode::Plan {
        "read-only"
    } else {
        "workspace-write"
    };
    let mut p = json!({
        "cwd": spec.dir.to_string_lossy(),
        "approvalPolicy": "untrusted",
        "sandbox": sandbox,
        "ephemeral": false,
    });
    if let Some(model) = &spec.model {
        p["model"] = json!(model);
    }
    if let Some(m) = &spec.ask_user {
        let mut server = json!({ "command": m.command.to_string_lossy(), "args": m.args });
        if let Some(t) = m.tool_timeout {
            server["tool_timeout_sec"] = json!(t.as_secs_f64());
        }
        if !m.env.is_empty() {
            let env: Map<String, Value> =
                m.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            server["env"] = Value::Object(env);
        }
        p["config"] = json!({ "mcp_servers": { m.name.clone(): server } });
    }
    for (k, v) in extra {
        p[*k] = v.clone();
    }
    p
}

impl CodexSession {
    pub(crate) async fn begin(
        options: &CodexOptions,
        spec: SessionSpec,
        thread: Thread,
    ) -> Result<Self, Error> {
        let log = match &spec.log_path {
            Some(path) => RawLog::open(path)?,
            None => RawLog::discard(),
        };
        let process_spec = Spec {
            program: options.binary.clone(),
            args: options.args.iter().map(Into::into).collect(),
            dir: Some(spec.dir.clone()),
            env: spec.env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
            stderr: Stderr::Piped,
        };
        let process_spec = spec.confined(process_spec).map_err(start_error)?;
        let mut process = tokio::task::spawn_blocking(move || Process::start(process_spec))
            .await
            .map_err(|e| start_error(format!("start task: {e}")))??;
        if let Err(e) = spec.spawned(process.identity()).await {
            drop(process.close().await);
            return Err(e);
        }
        copy_stderr(process.take_stderr(), &log);
        let Some(stdout) = process.take_stdout() else {
            drop(process.close().await);
            return Err(start_error("app-server has no stdout"));
        };
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let peer_cell: Arc<OnceLock<Peer>> = Arc::new(OnceLock::new());
        let handler = request_handler(internal_tx.clone(), Arc::clone(&peer_cell));
        let options_rpc = linerpc::Options {
            log: Some(log.clone()),
            handler: Some(handler),
            max_line: None,
            on_protocol_error: Some({
                let log = log.clone();
                Arc::new(move |_line: &[u8], err: &str| {
                    log.note(Dir::Note, &format!("codex: protocol error: {err}"));
                })
            }),
        };
        let (peer, notifications) = Peer::new(stdout, Arc::new(process.stdin()), options_rpc);
        let _set = peer_cell.set(peer.clone());
        let mut actor = Actor {
            spec,
            log,
            peer,
            process: Some(process),
            session: SessionRef {
                runtime: NAME.to_owned(),
                id: String::new(),
            },
            efforts: Vec::new(),
            limits: RateLimitSnapshot::default(),
            turn: None,
            starting: None,
            items: HashMap::new(),
            closed: false,
            next_turn: 0,
            internal_tx,
            internal_rx,
        };
        if let Err(e) = actor.handshake(&options.client_version, thread).await {
            drop(actor.shutdown().await);
            return Err(e);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let session = actor.session.clone();
        drop(tokio::spawn(actor.run(rx, notifications)));
        Ok(Self { session, tx })
    }

    pub(crate) fn session_ref(&self) -> &SessionRef {
        &self.session
    }

    pub(crate) async fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Cmd::Turn {
                input,
                cancel,
                reply,
            })
            .map_err(|_| Error::SessionClosed)?;
        result.await.map_err(|_| Error::SessionClosed)?
    }

    pub(crate) async fn interrupt(&mut self) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Cmd::Interrupt { reply })
            .map_err(|_| Error::SessionClosed)?;
        result.await.map_err(|_| Error::SessionClosed)?
    }

    pub(crate) async fn close(self) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        if self.tx.send(Cmd::Close { reply }).is_err() {
            return Ok(());
        }
        result.await.unwrap_or(Ok(()))
    }
}

fn copy_stderr(pipe: Option<std::process::ChildStderr>, log: &RawLog) {
    let Some(pipe) = pipe else { return };
    let writer = log.clone();
    let spawned = thread::Builder::new()
        .name("openchef-codex-stderr".to_owned())
        .spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                writer.note(Dir::Stderr, &line);
            }
        });
    if let Err(e) = spawned {
        // Without the thread the CLI's stderr is lost, not the session.
        log.note(Dir::Note, &format!("codex: cannot read stderr: {e}"));
    }
}

/// Forwards server requests to the actor. It first waits until the actor has taken
/// every notification read before the request, so state they carry (file-change
/// paths) is known when the request is handled.
fn request_handler(tx: mpsc::UnboundedSender<Internal>, peer: Arc<OnceLock<Peer>>) -> Handler {
    Arc::new(move |method, params, cancel| {
        let tx = tx.clone();
        let peer = peer.get().cloned();
        Box::pin(async move {
            if let Some(peer) = peer {
                // An error means the peer closed; the request then fails below.
                let _ordered = peer.barrier(&cancel).await;
            }
            let (reply, result) = oneshot::channel();
            tx.send(Internal::Request {
                method,
                params,
                cancel,
                reply,
            })
            .map_err(|_| RpcError::server("OpenChef session is closed"))?;
            result
                .await
                .unwrap_or_else(|_| Err(RpcError::server("OpenChef session is closed")))
        })
    })
}

impl Actor {
    /// Builds an actor over a peer without a process (replay tests).
    #[cfg(test)]
    pub(crate) fn detached(spec: SessionSpec) -> Self {
        struct Nowhere;
        impl linerpc::LineSink for Nowhere {
            fn send_line(&self, _: Vec<u8>) -> crate::BoxFuture<'static, std::io::Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        let (peer, _notifications) = Peer::new(
            std::io::empty(),
            Arc::new(Nowhere),
            linerpc::Options::default(),
        );
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            spec,
            log: RawLog::discard(),
            peer,
            process: None,
            session: SessionRef {
                runtime: NAME.to_owned(),
                id: String::new(),
            },
            efforts: Vec::new(),
            limits: RateLimitSnapshot::default(),
            turn: None,
            starting: None,
            items: HashMap::new(),
            closed: false,
            next_turn: 0,
            internal_tx,
            internal_rx,
        }
    }

    async fn handshake(&mut self, client_version: &str, thread: Thread) -> Result<(), Error> {
        let none = CancellationToken::new();
        let info = json!({ "clientInfo": { "name": "openchef", "title": "OpenChef", "version": client_version } });
        call::<Value>(&self.peer, "initialize", info, &none).await?;
        self.peer
            .notify("initialized", Value::Null)
            .await
            .map_err(call_error)?;
        let models: ModelList = call(&self.peer, "model/list", json!({}), &none).await?;
        self.efforts = models.supported_efforts(self.spec.model.as_deref());
        match call::<RateLimits>(&self.peer, "account/rateLimits/read", Value::Null, &none).await {
            Ok(r) => self.limits = r.rate_limits,
            // Rate limits are informative; an account without them must not block work.
            Err(e) => self.log.note(
                Dir::Note,
                &format!("codex: account/rateLimits/read failed: {e}"),
            ),
        }
        let id = match thread {
            Thread::New => {
                let res: ThreadResponse = call(
                    &self.peer,
                    "thread/start",
                    thread_params(&self.spec, &[]),
                    &none,
                )
                .await?;
                if res.thread.id.is_empty() {
                    return Err(protocol_error("thread/start returned no thread id"));
                }
                res.thread.id
            }
            Thread::Resume(id) => {
                let extra = [("threadId", json!(id)), ("excludeTurns", json!(true))];
                let _res: ThreadResponse = call(
                    &self.peer,
                    "thread/resume",
                    thread_params(&self.spec, &extra),
                    &none,
                )
                .await?;
                id
            }
        };
        self.session.id = id;
        Ok(())
    }

    async fn run(
        mut self,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        mut notifications: Notifications,
    ) {
        let mut stream_open = true;
        loop {
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Turn { input, cancel, reply }) => self.start_turn(&input, cancel, reply),
                    Some(Cmd::Interrupt { reply }) => self.interrupt(reply),
                    Some(Cmd::Close { reply }) => {
                        drop(reply.send(self.shutdown().await));
                        return;
                    }
                    None => {
                        drop(self.shutdown().await);
                        return;
                    }
                },
                Some(msg) = self.internal_rx.recv() => self.handle(msg),
                n = notifications.recv(), if stream_open => {
                    if let Some(n) = n {
                        self.on_notification(&n);
                    } else {
                        stream_open = false;
                        self.gone();
                    }
                }
            }
        }
    }

    pub(crate) fn handle(&mut self, msg: Internal) {
        match msg {
            Internal::Started { turn, result } => self.started(turn, result),
            Internal::Cancelled { turn } => self.cancelled(turn),
            Internal::CancelSettled { turn } => {
                if let Some(t) = self.turn.as_ref().filter(|t| t.id == turn) {
                    let usage = t.usage();
                    let mut f = Failure::new(FailureKind::Timeout, "turn deadline reached");
                    f.usage = usage;
                    self.end_turn(EventBody::TurnFailed(f));
                }
            }
            Internal::Denied { turn } => {
                if let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) {
                    t.denials += 1;
                }
            }
            Internal::Request {
                method,
                params,
                cancel,
                reply,
            } => self.on_request(&method, &params, cancel, reply),
        }
    }

    // --- turns ---------------------------------------------------------------------

    fn start_turn(
        &mut self,
        input: &TurnInput,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<TurnEvents, Error>>,
    ) {
        if self.closed {
            drop(reply.send(Err(Error::SessionClosed)));
            return;
        }
        if self.turn.is_some() {
            drop(reply.send(Err(Error::TurnInProgress)));
            return;
        }
        self.next_turn += 1;
        let id = self.next_turn;
        let (tx, rx) = TurnSender::new();
        self.turn = Some(TurnState {
            id,
            codex_id: None,
            tx,
            started: Instant::now(),
            want_schema: input.output_schema.is_some(),
            final_text: String::new(),
            usage_base: None,
            usage_last: TokenBreakdown::default(),
            denials: 0,
            last_error: None,
            stop: None,
            early: Vec::new(),
            done: CancellationToken::new(),
        });
        let mut params = json!({
            "threadId": self.session.id,
            "input": [{ "type": "text", "text": input.text }],
        });
        if let Some(effort) = nearest_effort(self.spec.effort, &self.efforts) {
            params["effort"] = json!(effort.as_str());
        }
        if let Some(schema) = &input.output_schema {
            params["outputSchema"] = schema.clone();
        }
        self.starting = Some(Starting {
            turn: id,
            rx,
            cancel: cancel.clone(),
            reply,
        });
        let (peer, internal) = (self.peer.clone(), self.internal_tx.clone());
        drop(tokio::spawn(async move {
            let result = call::<TurnStartResponse>(&peer, "turn/start", params, &cancel)
                .await
                .and_then(|r| {
                    if r.turn.id.is_empty() {
                        Err(protocol_error("turn/start returned no turn id"))
                    } else {
                        Ok(r.turn.id)
                    }
                });
            drop(internal.send(Internal::Started { turn: id, result }));
        }));
    }

    fn started(&mut self, turn: u64, result: Result<String, Error>) {
        let Some(starting) = self.starting.take_if(|s| s.turn == turn) else {
            return;
        };
        match result {
            Ok(codex_id) => {
                self.adopt_turn_id(turn, &codex_id);
                if let Some(t) = self.turn.as_ref().filter(|t| t.id == turn) {
                    let (cancel, done, internal) = (
                        starting.cancel.clone(),
                        t.done.clone(),
                        self.internal_tx.clone(),
                    );
                    let closed = self.peer.closed();
                    drop(tokio::spawn(async move {
                        tokio::select! {
                            () = cancel.cancelled() => drop(internal.send(Internal::Cancelled { turn })),
                            () = done.cancelled() => {}
                            () = closed.cancelled() => {}
                        }
                    }));
                }
                drop(starting.reply.send(Ok(starting.rx)));
            }
            Err(e) => {
                if let Some(mut t) = self.turn.take_if(|t| t.id == turn) {
                    t.done.cancel();
                    let _ended = t.tx.finish(EventBody::TurnFailed(Failure::new(
                        FailureKind::Protocol,
                        e.to_string(),
                    )));
                }
                drop(starting.reply.send(Err(e)));
            }
        }
    }

    /// Records the turn id (first source wins: the turn/start response or turn/started)
    /// and replays notifications held back while it was unknown.
    fn adopt_turn_id(&mut self, turn: u64, codex_id: &str) {
        let Some(t) = self
            .turn
            .as_mut()
            .filter(|t| t.id == turn && t.codex_id.is_none())
        else {
            return;
        };
        t.codex_id = Some(codex_id.to_owned());
        let early = std::mem::take(&mut t.early);
        for n in &early {
            self.on_notification(n);
        }
    }

    /// The caller's deadline: mark the turn, interrupt Codex, then end the turn once the
    /// interrupt is answered (or failed), so the next turn does not overlap it.
    fn cancelled(&mut self, turn: u64) {
        let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) else {
            return;
        };
        t.stop = Some(FailureKind::Timeout); // before interrupting: Codex may confirm at once
        let params = json!({ "threadId": self.session.id, "turnId": t.codex_id.clone().unwrap_or_default() });
        let (peer, internal, log) = (
            self.peer.clone(),
            self.internal_tx.clone(),
            self.log.clone(),
        );
        drop(tokio::spawn(async move {
            if let Err(e) =
                call::<Value>(&peer, "turn/interrupt", params, &CancellationToken::new()).await
            {
                log.note(
                    Dir::Note,
                    &format!("codex: interrupt after the deadline failed: {e}"),
                );
            }
            drop(internal.send(Internal::CancelSettled { turn }));
        }));
    }

    fn interrupt(&self, reply: oneshot::Sender<Result<(), Error>>) {
        let Some(t) = &self.turn else {
            drop(reply.send(Ok(())));
            return;
        };
        let params = json!({ "threadId": self.session.id, "turnId": t.codex_id.clone().unwrap_or_default() });
        let peer = self.peer.clone();
        drop(tokio::spawn(async move {
            let result = call::<Value>(&peer, "turn/interrupt", params, &CancellationToken::new())
                .await
                .map(drop);
            drop(reply.send(result));
        }));
    }

    /// Clears the turn before sending its terminal event, so a consumer that starts the
    /// next turn as soon as the stream ends never sees `TurnInProgress`.
    pub(crate) fn end_turn(&mut self, body: EventBody) {
        if let Some(mut t) = self.turn.take() {
            t.done.cancel();
            let _ended = t.tx.finish(body);
        }
    }

    /// The app server is gone.
    fn gone(&mut self) {
        self.closed = true;
        let ended = self.turn.as_ref().map(|t| t.id);
        if let Some(t) = &self.turn {
            let mut message = "codex app-server exited during the turn".to_owned();
            if let Some(e) = self.peer.stop_error() {
                message = format!("{message}: {e}");
            }
            let mut f = Failure::new(FailureKind::Crashed, message);
            f.usage = t.usage();
            self.end_turn(EventBody::TurnFailed(f));
        }
        // The server can exit right after answering turn/start, before that answer
        // reaches the actor. The turn was sent, so it ends as crashed, like any turn
        // the server dies in; only a turn that never started is refused.
        if let Some(s) = self.starting.take() {
            let reply = if ended == Some(s.turn) {
                Ok(s.rx)
            } else {
                Err(Error::SessionClosed)
            };
            drop(s.reply.send(reply));
        }
    }

    async fn shutdown(&mut self) -> Result<(), Error> {
        self.closed = true;
        if let Some(t) = &self.turn {
            let mut f = Failure::new(FailureKind::Interrupted, "session closed");
            f.usage = t.usage();
            self.end_turn(EventBody::TurnFailed(f));
        }
        if let Some(s) = self.starting.take() {
            drop(s.reply.send(Err(Error::SessionClosed)));
        }
        let mut result = Ok(());
        if let Some(mut process) = self.process.take() {
            match tokio::time::timeout(CLOSE_TIMEOUT, process.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => result = Err(e.into()),
                Err(_) => result = Err(start_error("app-server did not stop")),
            }
        }
        let closed = self.peer.closed();
        // The reader ends with the process; a stuck pipe must not wedge close.
        let _reader_done = tokio::time::timeout(CLOSE_TIMEOUT, closed.cancelled()).await;
        if let Some(e) = self.log.error() {
            result = result.and(Err(start_error(format!("raw log: {e}"))));
        }
        result
    }

    // --- notifications ---------------------------------------------------------------

    /// The active turn if a notification scoped to `thread`/`turn` belongs to it. While
    /// the turn id is unknown, turn-scoped notifications are held back rather than
    /// guessed at: after a resume Codex can still send some for the previous turn.
    fn current(&mut self, n: &Notification, thread: &str, turn: &str) -> Option<&mut TurnState> {
        if thread != self.session.id {
            return None;
        }
        let t = self.turn.as_mut()?;
        match &t.codex_id {
            None => {
                if !turn.is_empty() {
                    t.early.push(n.clone());
                }
                None
            }
            Some(id) if !turn.is_empty() && turn != id => None,
            Some(_) => Some(t),
        }
    }

    pub(crate) fn on_notification(&mut self, n: &Notification) {
        match n.method.as_str() {
            "account/rateLimits/updated" => {
                let Some(p) = parse::<RateLimits>(&n.params) else {
                    return;
                };
                self.limits.merge(p.rate_limits);
                if let (Some(t), Some(signal)) = (self.turn.as_mut(), self.limits.signal()) {
                    t.tx.emit(EventBody::Quota(signal));
                }
            }
            "item/agentMessage/delta" => {
                let Some(p) = parse::<DeltaNotification>(&n.params) else {
                    return;
                };
                if !p.delta.is_empty()
                    && let Some(t) = self.current(n, &p.thread_id, &p.turn_id)
                {
                    t.tx.emit(EventBody::Text(p.delta));
                }
            }
            "item/started" => {
                let Some(p) = parse::<ItemNotification>(&n.params) else {
                    return;
                };
                if p.item.kind == "fileChange" {
                    drop(self.items.insert(p.item.id.clone(), p.item.paths()));
                }
                if let Some(t) = self.current(n, &p.thread_id, &p.turn_id)
                    && let Some(call) = p.item.tool_call()
                {
                    t.tx.emit(EventBody::ToolCall(call));
                }
            }
            "item/completed" => {
                let Some(p) = parse::<ItemNotification>(&n.params) else {
                    return;
                };
                let Some(t) = self.current(n, &p.thread_id, &p.turn_id) else {
                    return;
                };
                if p.item.kind == "agentMessage" {
                    if p.item.phase.is_empty() || p.item.phase == "final_answer" {
                        t.final_text = p.item.text;
                    }
                } else if let Some(result) = p.item.tool_result() {
                    t.tx.emit(EventBody::ToolResult(result));
                }
            }
            "thread/tokenUsage/updated" => {
                let Some(p) = parse::<TokenUsageNotification>(&n.params) else {
                    return;
                };
                if let Some(t) = self.current(n, &p.thread_id, &p.turn_id) {
                    let u = p.token_usage;
                    t.usage_base.get_or_insert(u.total.minus(u.last));
                    t.usage_last = u.total;
                }
            }
            "error" => {
                let Some(p) = parse::<ErrorNotification>(&n.params) else {
                    return;
                };
                if !p.will_retry
                    && let Some(t) = self.current(n, &p.thread_id, &p.turn_id)
                {
                    t.last_error = Some(p.error);
                }
            }
            "turn/started" => {
                let Some(p) = parse::<TurnStartedNotification>(&n.params) else {
                    return;
                };
                if !p.turn.id.is_empty()
                    && p.thread_id == self.session.id
                    && let Some(turn) = self.turn.as_ref().map(|t| t.id)
                {
                    self.adopt_turn_id(turn, &p.turn.id);
                }
            }
            "turn/completed" => {
                let Some(p) = parse::<TurnCompletedNotification>(&n.params) else {
                    return;
                };
                let (thread, turn) = (p.thread_id.clone(), p.turn.id.clone());
                if self.current(n, &thread, &turn).is_some() {
                    let body = self.completion(&p);
                    self.end_turn(body);
                }
            }
            _ => {}
        }
    }

    fn completion(&self, p: &TurnCompletedNotification) -> EventBody {
        let Some(t) = &self.turn else {
            return EventBody::TurnFailed(Failure::new(FailureKind::Protocol, "no turn"));
        };
        let usage = t.usage();
        let duration = p
            .turn
            .duration_ms
            .map_or_else(|| t.started.elapsed(), Duration::from_millis);
        match p.turn.status.as_str() {
            "completed" => {
                let structured = if t.want_schema {
                    serde_json::from_str(&t.final_text).ok()
                } else {
                    None
                };
                EventBody::TurnCompleted(TurnResult {
                    final_text: t.final_text.clone(),
                    structured,
                    usage,
                    denials: t.denials,
                    models: Vec::new(),
                    duration,
                })
            }
            "interrupted" => {
                let mut f = Failure::new(
                    t.stop.unwrap_or(FailureKind::Interrupted),
                    "turn interrupted",
                );
                f.usage = usage;
                EventBody::TurnFailed(f)
            }
            status => {
                let mut f = match p.turn.error.as_ref().or(t.last_error.as_ref()) {
                    Some(e) => e.failure(),
                    None => Failure::new(
                        FailureKind::Unknown,
                        format!("turn ended with status {status}"),
                    ),
                };
                f.usage = usage;
                if matches!(
                    f.kind,
                    FailureKind::QuotaExhausted | FailureKind::RateLimited
                ) {
                    f.resets_at = self.limits.earliest_reset();
                }
                EventBody::TurnFailed(f)
            }
        }
    }

    // --- server requests -------------------------------------------------------------

    #[allow(clippy::too_many_lines, reason = "one arm per protocol method")]
    fn on_request(
        &self,
        method: &str,
        params: &Value,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    ) {
        match method {
            "item/commandExecution/requestApproval" => {
                let Some(p) = parse::<CommandApproval>(params) else {
                    return invalid(reply, method);
                };
                let request = ApprovalRequest {
                    session: self.session.clone(),
                    kind: RequestKind::Command,
                    tool: "Bash".to_owned(),
                    command: Some(p.command),
                    paths: if p.cwd.is_empty() {
                        Vec::new()
                    } else {
                        vec![p.cwd]
                    },
                    description: p.reason.unwrap_or_default(),
                    input: Value::Null,
                    raw: params.clone(),
                };
                self.decide(
                    request,
                    cancel,
                    reply,
                    |d| json!({ "decision": v2_decision(d.allow, d.for_session) }),
                );
            }
            "item/fileChange/requestApproval" => {
                let Some(p) = parse::<FileChangeApproval>(params) else {
                    return invalid(reply, method);
                };
                let mut paths = self.items.get(&p.item_id).cloned().unwrap_or_default();
                paths.extend(p.grant_root.filter(|r| !r.is_empty()));
                let request = ApprovalRequest {
                    session: self.session.clone(),
                    kind: RequestKind::FileChange,
                    tool: "Edit".to_owned(),
                    command: None,
                    paths,
                    description: p.reason.unwrap_or_default(),
                    input: Value::Null,
                    raw: params.clone(),
                };
                self.decide(
                    request,
                    cancel,
                    reply,
                    |d| json!({ "decision": v2_decision(d.allow, d.for_session) }),
                );
            }
            "mcpServer/elicitation/request" => {
                let Some(p) = parse::<Elicitation>(params) else {
                    return invalid(reply, method);
                };
                if p.meta.approval_kind != "mcp_tool_call" {
                    // Free-form elicitations (forms) are not supported; declining lets
                    // the agent continue without the answer.
                    drop(reply.send(Ok(json!({ "action": "decline" }))));
                    return;
                }
                let own = self
                    .spec
                    .ask_user
                    .as_ref()
                    .is_some_and(|m| m.name == p.server_name);
                if own && p.meta.tool_name == ASK_USER_TOOL {
                    drop(reply.send(Ok(json!({ "action": "accept", "content": {} }))));
                    return;
                }
                let request = ApprovalRequest {
                    session: self.session.clone(),
                    kind: RequestKind::Tool,
                    tool: format!("mcp__{}__{}", p.server_name, p.meta.tool_name),
                    command: None,
                    paths: Vec::new(),
                    description: p.message,
                    input: p.meta.tool_params,
                    raw: params.clone(),
                };
                self.decide(request, cancel, reply, |d| {
                    if d.allow {
                        json!({ "action": "accept", "content": {} })
                    } else {
                        json!({ "action": "decline" })
                    }
                });
            }
            "item/tool/requestUserInput" => {
                let Some(p) = parse::<UserInput>(params) else {
                    return invalid(reply, method);
                };
                self.answer_questions(p, cancel, reply);
            }
            "execCommandApproval" | "applyPatchApproval" => {
                // Legacy (v1) approvals, treated like their v2 counterparts.
                let (kind, tool) = if method == "applyPatchApproval" {
                    (RequestKind::FileChange, "Edit")
                } else {
                    (RequestKind::Command, "Bash")
                };
                let request = ApprovalRequest {
                    session: self.session.clone(),
                    kind,
                    tool: tool.to_owned(),
                    command: None,
                    paths: Vec::new(),
                    description: String::new(),
                    input: Value::Null,
                    raw: params.clone(),
                };
                self.decide(request, cancel, reply, |d| {
                    let decision = match (d.allow, d.for_session) {
                        (true, true) => "approved_for_session",
                        (true, false) => "approved",
                        (false, _) => "denied",
                    };
                    json!({ "decision": decision })
                });
            }
            "account/chatgptAuthTokens/refresh" => {
                drop(reply.send(Err(RpcError::server(
                    "OpenChef does not manage Codex credentials; run `codex login`",
                ))));
            }
            other => {
                drop(reply.send(Err(RpcError {
                    code: CODE_METHOD_NOT_FOUND,
                    message: format!("OpenChef does not handle {other}"),
                    data: None,
                })));
            }
        }
    }

    /// Asks the approver on its own task. Failing to decide denies: an undecided request
    /// must never run. A denial is counted on the turn it happened in.
    fn decide(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
        answer: fn(&Decision) -> Value,
    ) {
        let approver = Arc::clone(&self.spec.approver);
        let (internal, turn) = (self.internal_tx.clone(), self.turn.as_ref().map(|t| t.id));
        drop(tokio::spawn(async move {
            let decision = approver.approve(request, cancel).await.unwrap_or_else(|e| {
                Decision::deny(format!("OpenChef could not decide this request: {e}"))
            });
            if !decision.allow
                && let Some(turn) = turn
            {
                drop(internal.send(Internal::Denied { turn }));
            }
            drop(reply.send(Ok(answer(&decision))));
        }));
    }

    fn answer_questions(
        &self,
        input: UserInput,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    ) {
        let approver = Arc::clone(&self.spec.approver);
        let session = self.session.clone();
        drop(tokio::spawn(async move {
            let mut answers = Map::new();
            for q in input.questions {
                let options = q
                    .options
                    .unwrap_or_default()
                    .into_iter()
                    .map(|o| o.label)
                    .collect();
                let question = Question {
                    session: session.clone(),
                    text: q.question,
                    options,
                };
                match approver.answer(question, cancel.clone()).await {
                    Ok(a) => drop(answers.insert(q.id, json!({ "answers": [a] }))),
                    Err(e) => {
                        drop(reply.send(Err(RpcError::server(format!("answer question: {e}")))));
                        return;
                    }
                }
            }
            drop(reply.send(Ok(json!({ "answers": answers }))));
        }));
    }

    // --- replay support --------------------------------------------------------------

    #[cfg(test)]
    pub(crate) fn open_turn(&mut self, want_schema: bool) -> TurnEvents {
        self.next_turn += 1;
        let (tx, rx) = TurnSender::new();
        self.turn = Some(TurnState {
            id: self.next_turn,
            codex_id: None,
            tx,
            started: Instant::now(),
            want_schema,
            final_text: String::new(),
            usage_base: None,
            usage_last: TokenBreakdown::default(),
            denials: 0,
            last_error: None,
            stop: None,
            early: Vec::new(),
            done: CancellationToken::new(),
        });
        rx
    }

    #[cfg(test)]
    pub(crate) fn set_turn_id(&mut self, codex_id: &str) {
        if let Some(turn) = self.turn.as_ref().map(|t| t.id) {
            self.adopt_turn_id(turn, codex_id);
        }
    }
}

fn parse<T: DeserializeOwned>(params: &Value) -> Option<T> {
    serde_json::from_value(params.clone()).ok()
}

fn invalid(reply: oneshot::Sender<Result<Value, RpcError>>, method: &str) {
    drop(reply.send(Err(RpcError {
        code: CODE_INVALID_PARAMS,
        message: format!("invalid {method} params"),
        data: None,
    })));
}

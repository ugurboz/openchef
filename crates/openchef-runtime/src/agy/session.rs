//! The agy session actor (D-030). One Tokio task owns the session: its process
//! generations, the running turn and its pending permission requests. Notifications,
//! permission requests, prompt results and timers reach it as messages; every protocol
//! call that waits runs on its own task.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use openchef_core::PermissionMode;
use openchef_procs::{Process, Spec, Stderr};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::protocol::{
    ConfigOption, PermissionParams, PromptResult, SessionResult, UpdateParams, add_usage,
    cancelled, classify_error, find_option, mcp_servers, selected, signature, stop_failure,
    thought_level, tool_name,
};
use super::{AgyOptions, NAME};
use crate::linerpc::{self, CODE_METHOD_NOT_FOUND, Handler, Notifications, Peer, RpcError};
use crate::rawlog::{Dir, RawLog};
use crate::schema::{self, Schema};
use crate::{
    Decision, Error, EventBody, Failure, FailureKind, SessionRef, SessionSpec, ToolCall,
    TurnEvents, TurnInput, TurnResult, TurnSender, Usage,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(90); // session/new starts a language server
const BARRIER_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_WAIT: Duration = Duration::from_secs(2);
const STDERR_TAIL: usize = 2 << 10;
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

/// How a `session/prompt` call ended.
pub(crate) enum Prompted {
    Answered(PromptResult),
    /// The process went away (`closed`), or agy answered with an error.
    Failed {
        message: String,
        closed: bool,
    },
}

pub(crate) enum Internal {
    Update {
        generation: u64,
        params: Value,
    },
    Routed {
        generation: u64,
    },
    Permission {
        generation: u64,
        params: Value,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
    Denied {
        turn: u64,
    },
    PromptDone {
        turn: u64,
        prompted: Prompted,
    },
    TurnCancelled {
        turn: u64,
    },
    InterruptExpired {
        turn: u64,
    },
    KillExpired {
        turn: u64,
    },
}

/// One `agy acp` process. A session replaces it after a crash or a kill and continues
/// with `session/load`.
pub(crate) struct Generation {
    peer: Peer,
    process: Option<Arc<Process>>,
    stderr: Arc<Mutex<String>>,
    /// `session/load` is replaying history: the forwarder drops updates (D-016).
    loading: Arc<AtomicBool>,
    killed: bool,
    routed: bool,
}

pub(crate) struct TurnState {
    id: u64,
    generation: u64,
    tx: TurnSender,
    started: Instant,
    stop: Option<FailureKind>,
    /// Agent text since the last tool call: the final answer.
    text: String,
    tools: HashSet<String>,
    results: HashSet<String>,
    denials: u32,
    usage: Usage,
    schema: Option<Schema>,
    repaired: bool,
    interrupting: bool,
    /// Cancelled on interrupt and at the end: pending approvals answer "cancelled".
    perms: CancellationToken,
    done: CancellationToken,
    waiters: Vec<oneshot::Sender<Result<(), Error>>>,
}

pub(crate) struct Actor {
    options: Arc<AgyOptions>,
    spec: SessionSpec,
    log: RawLog,
    pub(crate) session: SessionRef,
    generations: HashMap<u64, Generation>,
    current: Option<u64>,
    next_generation: u64,
    respawn: bool,
    closed: bool,
    model: Option<String>,
    pub(crate) turn: Option<TurnState>,
    next_turn: u64,
    life: CancellationToken,
    internal_tx: mpsc::UnboundedSender<Internal>,
    pub(crate) internal_rx: mpsc::UnboundedReceiver<Internal>,
}

/// The handle the engine holds.
pub struct AgySession {
    session: SessionRef,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl std::fmt::Debug for AgySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgySession")
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

fn lock(m: &Mutex<String>) -> std::sync::MutexGuard<'_, String> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AgySession {
    pub(crate) async fn begin(
        options: Arc<AgyOptions>,
        spec: SessionSpec,
        id: Option<String>,
    ) -> Result<Self, Error> {
        let log = match &spec.log_path {
            Some(path) => RawLog::open(path)?,
            None => RawLog::discard(),
        };
        let session = SessionRef {
            runtime: NAME.to_owned(),
            id: id.unwrap_or_default(),
        };
        let mut actor = Actor::new(options, spec, log, session);
        if let Err(e) = actor.start().await {
            drop(actor.shutdown().await);
            return Err(e);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let session = actor.session.clone();
        drop(tokio::spawn(actor.run(rx)));
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

    /// Returns once the turn has ended.
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

/// Forwards one generation's notifications in order; updates replayed by
/// `session/load` are dropped here, before any turn can see them.
fn forward(
    generation: u64,
    mut notifications: Notifications,
    loading: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<Internal>,
) {
    drop(tokio::spawn(async move {
        while let Some(n) = notifications.recv().await {
            if n.method == "session/update" && !loading.load(Ordering::SeqCst) {
                drop(tx.send(Internal::Update {
                    generation,
                    params: n.params,
                }));
            }
        }
        drop(tx.send(Internal::Routed { generation }));
    }));
}

fn permission_handler(generation: u64, tx: mpsc::UnboundedSender<Internal>) -> Handler {
    Arc::new(move |method, params, cancel| {
        let tx = tx.clone();
        Box::pin(async move {
            if method != "session/request_permission" {
                return Err(RpcError {
                    code: CODE_METHOD_NOT_FOUND,
                    message: format!("OpenChef does not provide {method}"),
                    data: None,
                });
            }
            let (reply, result) = oneshot::channel();
            tx.send(Internal::Permission {
                generation,
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

fn copy_stderr(pipe: Option<std::process::ChildStderr>, log: &RawLog, tail: Arc<Mutex<String>>) {
    let Some(pipe) = pipe else { return };
    let writer = log.clone();
    let spawned = thread::Builder::new()
        .name("openchef-agy-stderr".to_owned())
        .spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                writer.note(Dir::Stderr, &line);
                let mut t = lock(&tail);
                t.push_str(&line);
                t.push('\n');
                if t.len() > STDERR_TAIL {
                    let cut = (t.len() - STDERR_TAIL..t.len())
                        .find(|i| t.is_char_boundary(*i))
                        .unwrap_or(t.len());
                    t.drain(..cut);
                }
            }
        });
    if let Err(e) = spawned {
        log.note(Dir::Note, &format!("agy: cannot read stderr: {e}"));
    }
}

async fn call<R: serde::de::DeserializeOwned>(
    peer: &Peer,
    method: &str,
    params: Value,
) -> Result<R, Error> {
    let none = CancellationToken::new();
    match tokio::time::timeout(CALL_TIMEOUT, peer.call(method, params, &none)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(start_error(e.to_string())),
        Err(_) => Err(start_error(format!(
            "{method}: no response within {CALL_TIMEOUT:?}"
        ))),
    }
}

impl Actor {
    pub(crate) fn new(
        options: Arc<AgyOptions>,
        spec: SessionSpec,
        log: RawLog,
        session: SessionRef,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            options,
            spec,
            log,
            session,
            generations: HashMap::new(),
            current: None,
            next_generation: 0,
            respawn: false,
            closed: false,
            model: None,
            turn: None,
            next_turn: 0,
            life: CancellationToken::new(),
            internal_tx,
            internal_rx,
        }
    }

    fn ask_user_name(&self) -> Option<&str> {
        self.spec.ask_user.as_ref().map(|m| m.name.as_str())
    }

    // --- processes -----------------------------------------------------------------

    /// Launches a process, then creates the session (or loads it) and configures it.
    async fn start(&mut self) -> Result<(), Error> {
        let mut args: Vec<String> = self.options.args.clone();
        args.extend(
            [
                "--sandbox",
                "--disable-telemetry",
                "--include-default-customizations=false",
                "--cli_dir_name",
            ]
            .map(str::to_owned),
        );
        args.push(self.options.cli_dir_name.clone());
        let spec = Spec {
            program: self.options.binary.clone(),
            args: args.into_iter().map(Into::into).collect(),
            dir: Some(self.spec.dir.clone()),
            env: self
                .spec
                .env
                .iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            stderr: Stderr::Piped,
        };
        let mut process = tokio::task::spawn_blocking(move || Process::start(spec))
            .await
            .map_err(|e| start_error(format!("start task: {e}")))??;
        if let Err(e) = self.spec.spawned(process.identity()).await {
            drop(process.close().await);
            return Err(e);
        }
        let tail = Arc::new(Mutex::new(String::new()));
        copy_stderr(process.take_stderr(), &self.log, Arc::clone(&tail));
        let Some(stdout) = process.take_stdout() else {
            drop(process.close().await);
            return Err(start_error("agy has no stdout"));
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        let log = self.log.clone();
        let options = linerpc::Options {
            log: Some(self.log.clone()),
            handler: Some(permission_handler(generation, self.internal_tx.clone())),
            max_line: None,
            on_protocol_error: Some(Arc::new(move |_line: &[u8], err: &str| {
                log.note(Dir::Note, &format!("agy: protocol error: {err}"));
            })),
        };
        let (peer, notifications) = Peer::new(stdout, Arc::new(process.stdin()), options);
        let loading = Arc::new(AtomicBool::new(false));
        forward(
            generation,
            notifications,
            Arc::clone(&loading),
            self.internal_tx.clone(),
        );
        drop(self.generations.insert(
            generation,
            Generation {
                peer: peer.clone(),
                process: Some(Arc::new(process)),
                stderr: Arc::clone(&tail),
                loading,
                killed: false,
                routed: false,
            },
        ));
        self.current = Some(generation);
        self.respawn = false;
        if let Err(e) = self.handshake(generation, &peer).await {
            self.kill(generation);
            let tail = lock(&tail).trim().to_owned();
            return Err(if tail.is_empty() {
                e
            } else {
                start_error(format!("{e} (stderr: {tail})"))
            });
        }
        Ok(())
    }

    async fn handshake(&mut self, generation: u64, peer: &Peer) -> Result<(), Error> {
        // OpenChef offers no client file system or terminal: agy uses its own tools,
        // which run in its sandbox and ask OpenChef before leaving it.
        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
            "clientInfo": { "name": "openchef", "title": "OpenChef", "version": self.options.client_version },
        });
        call::<Value>(peer, "initialize", init)
            .await
            .map_err(|e| start_error(format!("initialize: {e}")))?;
        let servers = mcp_servers(self.spec.ask_user.as_ref());
        let cwd = self.spec.dir.to_string_lossy().into_owned();
        let opts = if self.session.id.is_empty() {
            let res: SessionResult = call(
                peer,
                "session/new",
                json!({ "cwd": cwd, "mcpServers": servers }),
            )
            .await
            .map_err(|e| start_error(format!("session/new: {e}")))?;
            if res.session_id.is_empty() {
                return Err(Error::Protocol {
                    runtime: NAME,
                    reason: "session/new returned no session id".into(),
                    line: String::new(),
                });
            }
            self.session.id = res.session_id;
            res.config_options
        } else {
            self.load(generation, peer, &cwd, servers).await?
        };
        self.configure(peer, opts).await
    }

    /// Resumes the session; the history agy replays meanwhile is dropped (D-016).
    async fn load(
        &self,
        generation: u64,
        peer: &Peer,
        cwd: &str,
        servers: Value,
    ) -> Result<Vec<ConfigOption>, Error> {
        let Some(loading) = self
            .generations
            .get(&generation)
            .map(|g| Arc::clone(&g.loading))
        else {
            return Err(Error::SessionClosed);
        };
        loading.store(true, Ordering::SeqCst);
        let params = json!({ "sessionId": self.session.id, "cwd": cwd, "mcpServers": servers });
        let result: Result<SessionResult, Error> = call(peer, "session/load", params).await;
        let barrier =
            tokio::time::timeout(CALL_TIMEOUT, peer.barrier(&CancellationToken::new())).await;
        loading.store(false, Ordering::SeqCst);
        let res = result.map_err(|e| start_error(format!("session/load: {e}")))?;
        match barrier {
            Ok(Ok(())) => Ok(res.config_options),
            Ok(Err(e)) => Err(start_error(format!("session/load: {e}"))),
            Err(_) => Err(start_error("session/load: updates did not drain")),
        }
    }

    /// Sets model, thinking level and ACP mode `default` (D-015). The levels on offer
    /// depend on the model, so they are read after the model is set.
    async fn configure(&mut self, peer: &Peer, mut opts: Vec<ConfigOption>) -> Result<(), Error> {
        let id = self.session.id.clone();
        let set = |key: &'static str, value: String| {
            let peer = peer.clone();
            let params = json!({ "sessionId": id, "configId": key, "value": value });
            async move {
                let res: SessionResult = call(&peer, "session/set_config_option", params)
                    .await
                    .map_err(|e| start_error(format!("set {key}: {e}")))?;
                Ok::<_, Error>(res.config_options)
            }
        };
        if let Some(model) = self.spec.model.clone() {
            let got = set("model", model).await?;
            if !got.is_empty() {
                opts = got;
            }
        }
        if let Some(level) = thought_level(self.spec.effort, &opts) {
            let got = set("thought_level", level).await?;
            if !got.is_empty() {
                opts = got;
            }
        }
        let got = set("mode", "default".to_owned()).await?;
        if !got.is_empty() {
            opts = got;
        }
        self.model = find_option(&opts, "model")
            .map(|m| m.current_value.clone())
            .filter(|m| !m.is_empty());
        Ok(())
    }

    /// Stops a generation at once; the next turn starts a new one.
    fn kill(&mut self, generation: u64) {
        if self.current == Some(generation) {
            self.respawn = true;
        }
        let Some(g) = self.generations.get_mut(&generation) else {
            return;
        };
        g.killed = true;
        if let Some(p) = g.process.clone() {
            let log = self.log.clone();
            drop(tokio::spawn(async move {
                if let Err(e) = p.terminate(Duration::ZERO, &CancellationToken::new()).await {
                    log.note(Dir::Note, &format!("agy: kill: {e}"));
                }
            }));
        }
    }

    // --- the actor loop ------------------------------------------------------------

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Turn { input, cancel, reply }) => {
                        let result = self.start_turn(&input, cancel).await;
                        drop(reply.send(result));
                    }
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
            }
        }
    }

    pub(crate) fn handle(&mut self, msg: Internal) {
        match msg {
            Internal::Update { generation, params } => self.on_update(generation, &params),
            Internal::Routed { generation } => {
                if let Some(g) = self.generations.get_mut(&generation) {
                    g.routed = true;
                }
                if self.current == Some(generation) && !self.closed {
                    self.respawn = true; // an unexpected exit is recovered on the next turn
                }
            }
            Internal::Permission {
                generation,
                params,
                cancel,
                reply,
            } => self.on_permission(generation, &params, &cancel, reply),
            Internal::Denied { turn } => {
                if let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) {
                    t.denials += 1;
                }
            }
            Internal::PromptDone { turn, prompted } => self.prompt_done(turn, prompted),
            Internal::TurnCancelled { turn } => {
                if let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) {
                    t.stop.get_or_insert(FailureKind::Timeout);
                    self.begin_interrupt(self.options.interrupt_wait);
                }
            }
            Internal::InterruptExpired { turn } => {
                if let Some(generation) = self
                    .turn
                    .as_ref()
                    .filter(|t| t.id == turn)
                    .map(|t| t.generation)
                {
                    // The pending prompt then fails with "closed" and ends the turn; if
                    // even that does not happen, KillExpired does.
                    self.kill(generation);
                    self.after(self.options.close_wait, Internal::KillExpired { turn });
                }
            }
            Internal::KillExpired { turn } => {
                if let Some(t) = self.turn.as_ref().filter(|t| t.id == turn) {
                    let kind = t.stop.unwrap_or(FailureKind::Interrupted);
                    self.end_turn(EventBody::TurnFailed(Failure::new(
                        kind,
                        "turn did not stop after cancel; process killed",
                    )));
                }
            }
        }
    }

    fn after(&self, wait: Duration, msg: Internal) {
        let tx = self.internal_tx.clone();
        drop(tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            drop(tx.send(msg));
        }));
    }

    /// Waits until `done` holds or `deadline` passes, handling messages meanwhile.
    async fn wait_until(
        &mut self,
        deadline: tokio::time::Instant,
        done: impl Fn(&Self) -> bool,
    ) -> bool {
        loop {
            if done(self) {
                return true;
            }
            tokio::select! {
                msg = self.internal_rx.recv() => match msg {
                    Some(msg) => self.handle(msg),
                    None => return done(self),
                },
                () = tokio::time::sleep_until(deadline) => return done(self),
            }
        }
    }

    // --- turns ---------------------------------------------------------------------

    async fn start_turn(
        &mut self,
        input: &TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        let schema = input
            .output_schema
            .as_ref()
            .map(Schema::compile)
            .transpose()?;
        if self.closed {
            return Err(Error::SessionClosed);
        }
        if self.turn.is_some() {
            return Err(Error::TurnInProgress);
        }
        let current = self.current.and_then(|g| self.generations.get(&g));
        if self.respawn || current.is_none_or(|g| g.routed || g.peer.closed().is_cancelled()) {
            if let Some(old) = self.current {
                self.kill(old);
            }
            self.start().await?;
        }
        let Some(generation) = self.current else {
            return Err(Error::SessionClosed);
        };
        let mut text = input.text.clone();
        if let Some(raw) = &input.output_schema {
            text.push_str(&schema::instruction(raw));
        }
        let rx = self.open_turn(generation, schema);
        let Some(t) = &self.turn else {
            return Err(Error::SessionClosed);
        };
        let (id, done) = (t.id, t.done.clone());
        self.send_prompt(id, generation, &text);
        let internal = self.internal_tx.clone();
        drop(tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => drop(internal.send(Internal::TurnCancelled { turn: id })),
                () = done.cancelled() => {}
            }
        }));
        Ok(rx)
    }

    pub(crate) fn open_turn(&mut self, generation: u64, schema: Option<Schema>) -> TurnEvents {
        self.next_turn += 1;
        let (tx, rx) = TurnSender::new();
        self.turn = Some(TurnState {
            id: self.next_turn,
            generation,
            tx,
            started: Instant::now(),
            stop: None,
            text: String::new(),
            tools: HashSet::new(),
            results: HashSet::new(),
            denials: 0,
            usage: Usage::default(),
            schema,
            repaired: false,
            interrupting: false,
            perms: self.life.child_token(),
            done: CancellationToken::new(),
            waiters: Vec::new(),
        });
        rx
    }

    /// Sends one `session/prompt` and reports once every update that preceded the
    /// response has been forwarded (the barrier, D-017).
    fn send_prompt(&self, turn: u64, generation: u64, text: &str) {
        let Some(g) = self.generations.get(&generation) else {
            return;
        };
        let (peer, process, internal, life, log) = (
            g.peer.clone(),
            g.process.clone(),
            self.internal_tx.clone(),
            self.life.clone(),
            self.log.clone(),
        );
        let params =
            json!({ "sessionId": self.session.id, "prompt": [{ "type": "text", "text": text }] });
        drop(tokio::spawn(async move {
            let result = peer
                .call::<PromptResult>("session/prompt", params, &life)
                .await;
            if let Ok(Err(e)) = tokio::time::timeout(BARRIER_TIMEOUT, peer.barrier(&life)).await
                && result.is_ok()
            {
                log.note(Dir::Note, &format!("agy: barrier: {e}"));
            }
            let prompted = match result {
                Ok(r) => Prompted::Answered(r),
                Err(e) => {
                    let closed = matches!(e, linerpc::Error::Closed { .. });
                    if closed && let Some(p) = process {
                        // stdout can end before stderr is fully copied; once the process
                        // is reaped, the copy is complete.
                        let _reaped = tokio::time::timeout(EXIT_WAIT, p.wait()).await;
                    }
                    Prompted::Failed {
                        message: format!("session/prompt: {e}"),
                        closed,
                    }
                }
            };
            drop(internal.send(Internal::PromptDone { turn, prompted }));
        }));
    }

    pub(crate) fn prompt_done(&mut self, turn: u64, prompted: Prompted) {
        let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) else {
            return;
        };
        let generation = t.generation;
        let stop = t.stop;
        let result = match prompted {
            Prompted::Answered(r) => r,
            Prompted::Failed { message, closed } => {
                let killed = self.generations.get(&generation).is_some_and(|g| g.killed);
                let (kind, message) = match stop {
                    Some(stop) if killed => (stop, message),
                    _ if closed => {
                        let tail = self
                            .generations
                            .get(&generation)
                            .map_or_else(String::new, |g| lock(&g.stderr).trim().to_owned());
                        (
                            FailureKind::Crashed,
                            if tail.is_empty() {
                                message
                            } else {
                                format!("{message}: {tail}")
                            },
                        )
                    }
                    _ => (classify_error(&message), message),
                };
                let mut f = Failure::new(kind, message);
                f.usage = t.usage;
                return self.end_turn(EventBody::TurnFailed(f));
            }
        };
        t.usage = add_usage(&t.usage, &result.usage());
        if let Some(kind) = stop_failure(&result.stop_reason, stop) {
            let mut f = Failure::new(kind, format!("agy stopped: {}", result.stop_reason));
            f.usage = t.usage;
            return self.end_turn(EventBody::TurnFailed(f));
        }
        let final_text = t.text.trim().to_owned();
        let structured = match &t.schema {
            None => None,
            Some(schema) => match validate(schema, &final_text) {
                Ok(doc) => Some(doc),
                Err(problem) if !t.repaired => {
                    // agy has no native structured output (m1-findings §4): one repair.
                    t.repaired = true;
                    t.text.clear();
                    return self.send_prompt(turn, generation, &schema::repair(&problem));
                }
                Err(problem) => {
                    let mut f = Failure::new(
                        FailureKind::Protocol,
                        format!(
                            "answer does not satisfy the output schema after one repair: {problem}"
                        ),
                    );
                    f.usage = t.usage;
                    return self.end_turn(EventBody::TurnFailed(f));
                }
            },
        };
        let result = TurnResult {
            final_text,
            structured,
            usage: t.usage,
            denials: t.denials,
            models: self.model.iter().cloned().collect(),
            duration: t.started.elapsed(),
        };
        self.end_turn(EventBody::TurnCompleted(result));
    }

    /// Clears the turn before sending its terminal event, so a consumer that starts the
    /// next turn as soon as the stream ends never sees `TurnInProgress`.
    pub(crate) fn end_turn(&mut self, body: EventBody) {
        let Some(mut t) = self.turn.take() else {
            return;
        };
        t.perms.cancel();
        t.done.cancel();
        let _ended = t.tx.finish(body);
        for w in t.waiters {
            drop(w.send(Ok(())));
        }
    }

    fn interrupt(&mut self, reply: oneshot::Sender<Result<(), Error>>) {
        let Some(t) = self.turn.as_mut() else {
            drop(reply.send(Ok(())));
            return;
        };
        t.stop.get_or_insert(FailureKind::Interrupted);
        t.waiters.push(reply);
        self.begin_interrupt(self.options.interrupt_wait);
    }

    /// Sends `session/cancel`, answers pending approvals "cancelled" and waits `wait` for
    /// the prompt to return; then the process is killed and the next turn reloads.
    fn begin_interrupt(&mut self, wait: Duration) {
        let Some(t) = self.turn.as_mut() else { return };
        if t.interrupting {
            return;
        }
        t.interrupting = true;
        t.perms.cancel();
        let (turn, generation) = (t.id, t.generation);
        if let Some(g) = self.generations.get(&generation) {
            let (peer, log, id) = (g.peer.clone(), self.log.clone(), self.session.id.clone());
            drop(tokio::spawn(async move {
                if let Err(e) = peer
                    .notify("session/cancel", json!({ "sessionId": id }))
                    .await
                    && !matches!(e, linerpc::Error::Closed { .. })
                {
                    log.note(Dir::Note, &format!("agy: cancel: {e}"));
                }
            }));
        }
        self.after(wait, Internal::InterruptExpired { turn });
    }

    async fn shutdown(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let close_wait = self.options.close_wait;
        let mut result = Ok(());
        if let Some(t) = self.turn.as_mut() {
            t.stop.get_or_insert(FailureKind::Interrupted);
            let (turn, generation) = (t.id, t.generation);
            self.begin_interrupt(close_wait);
            let deadline = tokio::time::Instant::now() + close_wait * 2 + Duration::from_secs(1);
            let _ended = self
                .wait_until(deadline, |a| a.turn.as_ref().is_none_or(|t| t.id != turn))
                .await;
            self.kill(generation);
        } else if let Some(g) = self.current.and_then(|g| self.generations.get_mut(&g)) {
            g.killed = true;
            if let Some(p) = g.process.clone() {
                // agy exits on EOF within ~50 ms (m1-findings §2).
                if let Err(e) = p
                    .terminate(openchef_procs::DEFAULT_GRACE, &CancellationToken::new())
                    .await
                {
                    result = Err(e.into());
                }
            }
        }
        if let Some(current) = self.current {
            let deadline = tokio::time::Instant::now() + close_wait;
            if !self
                .wait_until(deadline, |a| {
                    a.generations.get(&current).is_none_or(|g| g.routed)
                })
                .await
            {
                result = result.and(Err(start_error("notifications did not drain")));
            }
        }
        self.end_turn(EventBody::TurnFailed(Failure::new(
            FailureKind::Interrupted,
            "session closed",
        )));
        // Nothing of this session outlives close: wait until every process is reaped.
        let processes: Vec<Arc<Process>> = self
            .generations
            .values()
            .filter_map(|g| g.process.clone())
            .collect();
        for p in processes {
            if tokio::time::timeout(close_wait, p.wait()).await.is_err()
                && let Err(e) = p.terminate(Duration::ZERO, &CancellationToken::new()).await
            {
                result = result.and(Err(e.into()));
            }
        }
        self.life.cancel();
        if let Some(e) = self.log.error() {
            result = result.and(Err(start_error(format!("raw log: {e}"))));
        }
        result
    }

    // --- updates -------------------------------------------------------------------

    fn active(&mut self, generation: u64) -> Option<&mut TurnState> {
        self.turn.as_mut().filter(|t| t.generation == generation)
    }

    pub(crate) fn on_update(&mut self, generation: u64, raw: &Value) {
        let p: UpdateParams = match serde_json::from_value(raw.clone()) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .log
                    .note(Dir::Note, &format!("agy: undecodable update: {e}"));
            }
        };
        if !p.session_id.is_empty() && p.session_id != self.session.id {
            return; // another session
        }
        let ask_user = self.ask_user_name().map(str::to_owned);
        let Some(t) = self.active(generation) else {
            return;
        }; // outside any turn
        let u = p.update;
        match u.kind.as_str() {
            "agent_message_chunk" => {
                let text = u.chunk_text();
                if !text.is_empty() {
                    t.text.push_str(text);
                    t.tx.emit(EventBody::Text(text.to_owned()));
                }
            }
            "tool_call" | "tool_call_update" => {
                // A tool call is reported once; agent text before it is not the answer.
                if !u.tool_call_id.is_empty() && t.tools.insert(u.tool_call_id.clone()) {
                    t.text.clear();
                    let name = tool_name(&u.name, &u.tool_kind, ask_user.as_deref());
                    let signature = signature(&name, &u.raw_input);
                    t.tx.emit(EventBody::ToolCall(ToolCall {
                        id: u.tool_call_id.clone(),
                        name,
                        input: u.raw_input.clone(),
                        signature,
                    }));
                }
                if u.kind == "tool_call_update"
                    && (u.status == "completed" || u.status == "failed")
                    && t.results.insert(u.tool_call_id.clone())
                {
                    t.tx.emit(EventBody::ToolResult(u.result()));
                }
            }
            _ => {}
        }
    }

    // --- permissions ---------------------------------------------------------------

    pub(crate) fn on_permission(
        &mut self,
        generation: u64,
        raw: &Value,
        peer_closed: &CancellationToken,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    ) {
        let p: PermissionParams = match serde_json::from_value(raw.clone()) {
            Ok(p) => p,
            Err(e) => {
                drop(reply.send(Err(RpcError {
                    code: CODE_INVALID_PARAMS,
                    message: format!("invalid params: {e}"),
                    data: None,
                })));
                return;
            }
        };
        let request = p.approval_request(self.session.clone(), raw, self.ask_user_name());
        let own_question = self
            .ask_user_name()
            .is_some_and(|n| request.tool == format!("mcp__{n}__ask_user"));
        let plan = self.spec.mode == PermissionMode::Plan;
        let approver = Arc::clone(&self.spec.approver);
        let log = self.log.clone();
        let Some(t) = self.active(generation) else {
            drop(reply.send(Ok(answer(
                &p,
                &Decision::deny("no OpenChef turn is active"),
                &log,
            ))));
            return;
        };
        if own_question {
            drop(reply.send(Ok(answer(&p, &Decision::allow(), &log)))); // OpenChef's own question tool
            return;
        }
        if plan {
            // D-015: plan is read-only; only sandboxed reads run, every request is refused.
            t.denials += 1;
            let d = Decision::deny(
                "plan mode is read-only: OpenChef refuses edits and commands outside the sandbox",
            );
            drop(reply.send(Ok(answer(&p, &d, &log))));
            return;
        }
        let (turn, perms, internal) = (t.id, t.perms.clone(), self.internal_tx.clone());
        let ask = CancellationToken::new();
        let watcher = {
            let (ask, perms, closed) = (ask.clone(), perms.clone(), peer_closed.clone());
            async move {
                tokio::select! {
                    () = perms.cancelled() => ask.cancel(),
                    () = closed.cancelled() => ask.cancel(),
                    () = ask.cancelled() => {}
                }
            }
        };
        drop(tokio::spawn(async move {
            let watching = tokio::spawn(watcher);
            let decided = approver.approve(request, ask.clone()).await;
            let interrupted = ask.is_cancelled() && perms.is_cancelled();
            ask.cancel();
            let _watched = watching.await; // ends as soon as `ask` is cancelled
            if interrupted {
                drop(reply.send(Ok(cancelled())));
                return;
            }
            let d = decided.unwrap_or_else(|e| {
                Decision::deny(format!("OpenChef could not decide this request: {e}"))
            });
            if !d.allow {
                drop(internal.send(Internal::Denied { turn }));
            }
            drop(reply.send(Ok(answer(&p, &d, &log))));
        }));
    }

    // --- replay support ------------------------------------------------------------

    /// Adds a generation without a process (replay tests).
    #[cfg(test)]
    pub(crate) fn attach_detached(&mut self) -> u64 {
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
        let generation = self.next_generation;
        self.next_generation += 1;
        drop(self.generations.insert(
            generation,
            Generation {
                peer,
                process: None,
                stderr: Arc::new(Mutex::new(String::new())),
                loading: Arc::new(AtomicBool::new(false)),
                killed: false,
                routed: false,
            },
        ));
        self.current = Some(generation);
        generation
    }

    #[cfg(test)]
    pub(crate) fn turn_id(&self) -> Option<u64> {
        self.turn.as_ref().map(|t| t.id)
    }
}

/// The reply to a permission request for decision `d` (D-016: options by kind).
fn answer(p: &PermissionParams, d: &Decision, log: &RawLog) -> Value {
    let kind = if d.allow { "allow_once" } else { "reject_once" };
    if let Some(id) = p.option_id(kind) {
        return selected(id);
    }
    log.note(
        Dir::Note,
        &format!("agy: no {kind} option; cancelling the request"),
    );
    cancelled()
}

fn validate(schema: &Schema, text: &str) -> Result<Value, String> {
    let doc = schema::extract(text).ok_or_else(|| "answer contains no JSON value".to_owned())?;
    schema.validate(&doc)?;
    Ok(doc)
}

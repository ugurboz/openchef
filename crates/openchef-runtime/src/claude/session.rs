//! The Claude session actor (D-030). One Tokio task owns the session: its process
//! generations, the running turn, pending control requests and permission requests.
//! Reader and stderr threads, permission decisions and turn deadlines reach it as
//! messages. Operations that wait (initialize, interrupt, close) keep handling those
//! messages while they wait, so the actor never stalls on itself.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use openchef_procs::{Process, Spec, Stderr, StdinWriter};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::args::{Launch, ask_user_tool, build_args, mcp_config_json, mode_and_tools};
use super::protocol::{
    CanUseTool, ControlResponse, Envelope, ResultMessage, STRUCTURED_OUTPUT_TOOL, blocks,
    control_request, error_response, permission_response, quota_signal, signature, text_delta,
    tool_result_text, user_message,
};
use super::{ClaudeOptions, NAME};
use crate::rawlog::{Dir, RawLog};
use crate::{
    Decision, Error, EventBody, Failure, FailureKind, QuotaSignal, SessionRef, SessionSpec,
    ToolCall, ToolResult, TurnEvents, TurnInput, TurnSender,
};

const MAX_LINE: usize = 64 << 20;
const STDERR_TAIL: usize = 2 << 10;
const EXIT_WAIT: Duration = Duration::from_secs(2);

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
    Line {
        generation: u64,
        line: Value,
    },
    Gone {
        generation: u64,
    },
    Decided {
        generation: u64,
        request_id: String,
        input: Value,
        decision: Decision,
    },
    TurnCancelled {
        turn: u64,
    },
}

/// One Claude process. A session replaces it for a new output schema (D-011), after
/// killing a turn that would not stop (D-009, D-012) and after an unexpected exit; the
/// replacement continues the conversation with `--resume`.
pub(crate) struct Generation {
    process: Option<Arc<Process>>,
    stdin: Option<StdinWriter>,
    stderr: Arc<Mutex<String>>,
    written: u32,
    results: u32,
    done: bool,
    killed: bool,
    perms: HashMap<String, CancellationToken>,
}

impl Generation {
    pub(crate) fn detached() -> Self {
        Self {
            process: None,
            stdin: None,
            stderr: Arc::new(Mutex::new(String::new())),
            written: 0,
            results: 0,
            done: false,
            killed: false,
            perms: HashMap::new(),
        }
    }
}

pub(crate) struct TurnState {
    id: u64,
    generation: u64,
    /// `result_index` of the result that ends this turn (D-009).
    ordinal: u32,
    tx: TurnSender,
    stop: Option<FailureKind>,
    tools: HashSet<String>,
    skip: HashSet<String>,
    done: CancellationToken,
}

pub(crate) struct Actor {
    options: Arc<ClaudeOptions>,
    spec: SessionSpec,
    log: RawLog,
    session: SessionRef,
    mcp_config: Option<PathBuf>,
    /// Holds the MCP config; removed on shutdown, before close returns.
    tmp: Option<tempfile::TempDir>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    generations: HashMap<u64, Generation>,
    current: Option<u64>,
    next_generation: u64,
    schema: Option<Value>,
    respawn: bool,
    /// Claude has a transcript for the session id (after the first result, or when
    /// resuming); before that `--resume` fails, so a replacement reuses `--session-id`.
    started: bool,
    closed: bool,
    quota: Option<QuotaSignal>,
    pub(crate) turn: Option<TurnState>,
    next_turn: u64,
    control_seq: u64,
    controls: HashMap<String, Option<ControlResponse>>,
    life: CancellationToken,
}

/// The handle the engine holds.
pub struct ClaudeSession {
    session: SessionRef,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl std::fmt::Debug for ClaudeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeSession")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl ClaudeSession {
    pub(crate) async fn begin(
        options: Arc<ClaudeOptions>,
        spec: SessionSpec,
        id: String,
        resume: bool,
    ) -> Result<Self, Error> {
        let log = match &spec.log_path {
            Some(path) => RawLog::open(path)?,
            None => RawLog::discard(),
        };
        let mut actor = Actor::new(
            options,
            spec,
            log,
            SessionRef {
                runtime: NAME.to_owned(),
                id,
            },
            resume,
        );
        if let Some(server) = actor.spec.ask_user.clone() {
            let tmp = tempfile::Builder::new()
                .prefix("openchef-claude-")
                .tempdir()
                .map_err(|e| start_error(format!("temp dir: {e}")))?;
            let path = tmp.path().join("mcp.json");
            std::fs::write(&path, mcp_config_json(&server))
                .map_err(|e| start_error(format!("write mcp config: {e}")))?;
            actor.mcp_config = Some(path);
            actor.tmp = Some(tmp);
        }
        actor.spawn(None, false).await?;
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
            return Ok(()); // the actor is already gone
        }
        result.await.unwrap_or(Ok(()))
    }
}

fn start_error(message: String) -> Error {
    Error::Start {
        runtime: NAME,
        message,
    }
}

impl Actor {
    pub(crate) fn new(
        options: Arc<ClaudeOptions>,
        spec: SessionSpec,
        log: RawLog,
        session: SessionRef,
        started: bool,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            options,
            spec,
            log,
            session,
            mcp_config: None,
            tmp: None,
            internal_tx,
            internal_rx,
            generations: HashMap::new(),
            current: None,
            next_generation: 0,
            schema: None,
            respawn: false,
            started,
            closed: false,
            quota: None,
            turn: None,
            next_turn: 0,
            control_seq: 0,
            controls: HashMap::new(),
            life: CancellationToken::new(),
        }
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Turn { input, cancel, reply }) => {
                        let result = self.start_turn(input, cancel).await;
                        drop(reply.send(result));
                    }
                    Some(Cmd::Interrupt { reply }) => {
                        let result = self.interrupt_current(FailureKind::Interrupted, self.options.interrupt_wait).await;
                        drop(reply.send(result));
                    }
                    Some(Cmd::Close { reply }) => {
                        let result = self.shutdown().await;
                        drop(reply.send(result));
                        return;
                    }
                    None => {
                        // The handle was dropped without close: stop everything.
                        drop(self.shutdown().await);
                        return;
                    }
                },
                Some(msg) = self.internal_rx.recv() => {
                    if let Some(turn) = self.handle(msg)
                        && self.turn.as_ref().is_some_and(|t| t.id == turn)
                    {
                        drop(self.interrupt_current(FailureKind::Timeout, self.options.interrupt_wait).await);
                    }
                }
            }
        }
    }

    /// Handles one internal message without waiting. Returns a turn whose deadline
    /// passed, for the caller to interrupt.
    pub(crate) fn handle(&mut self, msg: Internal) -> Option<u64> {
        match msg {
            Internal::Line { generation, line } => self.dispatch(generation, &line),
            Internal::Gone { generation } => self.gone(generation),
            Internal::Decided {
                generation,
                request_id,
                input,
                decision,
            } => {
                let alive = self.generations.get_mut(&generation).is_some_and(|g| {
                    drop(g.perms.remove(&request_id));
                    !g.done
                });
                if alive {
                    // ForSession is not forwarded: OpenChef never asks Claude to persist
                    // permission changes; the approver keeps its own session memory.
                    self.write(
                        generation,
                        &permission_response(&request_id, &input, &decision),
                    );
                }
            }
            Internal::TurnCancelled { turn } => {
                if let Some(t) = self.turn.as_mut().filter(|t| t.id == turn) {
                    t.stop.get_or_insert(FailureKind::Timeout);
                    return Some(turn);
                }
            }
        }
        None
    }

    /// Waits until `done` holds or `deadline` passes, handling messages meanwhile.
    async fn wait_until(&mut self, deadline: Instant, done: impl Fn(&Self) -> bool) -> bool {
        loop {
            if done(self) {
                return true;
            }
            tokio::select! {
                msg = self.internal_rx.recv() => match msg {
                    Some(msg) => drop(self.handle(msg)),
                    None => return done(self),
                },
                () = tokio::time::sleep_until(deadline) => return done(self),
            }
        }
    }

    // --- processes -----------------------------------------------------------------

    /// Starts a process. With `fallback`, a failed start is retried once with the other
    /// identity flag: whether Claude has a transcript is only certain after a result
    /// (D-013).
    async fn spawn(&mut self, schema: Option<Value>, fallback: bool) -> Result<(), Error> {
        let resume = self.started;
        match self.spawn_once(resume, schema.clone()).await {
            Ok(()) => Ok(()),
            Err(e) if !fallback || self.closed || matches!(e, Error::SpawnRejected { .. }) => {
                Err(e)
            }
            Err(first) => {
                self.log.note(Dir::Note, &format!("claude: start with resume={resume} failed, retrying with resume={}: {first}", !resume));
                self.spawn_once(!resume, schema)
                    .await
                    .map_err(|second| start_error(format!("{first}; then {second}")))
            }
        }
    }

    async fn spawn_once(&mut self, resume: bool, schema: Option<Value>) -> Result<(), Error> {
        let mcp = self
            .mcp_config
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let args = build_args(
            &self.spec,
            &Launch {
                session_id: &self.session.id,
                resume,
                schema: schema.as_ref(),
                mcp_config: mcp.as_deref(),
            },
        );
        let spec = Spec {
            program: self.options.binary.clone(),
            args: self
                .options
                .prefix_args
                .iter()
                .chain(args.iter())
                .map(Into::into)
                .collect(),
            dir: Some(self.spec.dir.clone()),
            env: process_env(&self.spec)
                .into_iter()
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
        let generation = self.next_generation;
        self.next_generation += 1;
        let stdin = process.stdin();
        let stdout = process.take_stdout();
        let stderr_pipe = process.take_stderr();
        let tail = Arc::new(Mutex::new(String::new()));
        self.start_readers(generation, stdout, stderr_pipe, Arc::clone(&tail));
        self.generations.insert(
            generation,
            Generation {
                process: Some(Arc::new(process)),
                stdin: Some(stdin),
                stderr: tail,
                ..Generation::detached()
            },
        );
        self.current = Some(generation);
        self.schema = schema;
        self.respawn = false;
        if let Err(e) = self
            .control(generation, "initialize", self.options.init_timeout)
            .await
        {
            self.kill(generation);
            let tail = self.stderr_tail(generation);
            return Err(start_error(if tail.is_empty() {
                format!("initialize: {e}")
            } else {
                format!("initialize: {e} (stderr: {tail})")
            }));
        }
        Ok(())
    }

    fn start_readers(
        &self,
        generation: u64,
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        tail: Arc<Mutex<String>>,
    ) {
        let (stderr_done_tx, stderr_done_rx) = std::sync::mpsc::channel::<()>();
        if let Some(pipe) = stderr {
            let log = self.log.clone();
            drop(
                thread::Builder::new()
                    .name("openchef-claude-stderr".to_owned())
                    .spawn(move || {
                        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                            log.note(Dir::Stderr, &line);
                            let mut t = tail.lock().unwrap_or_else(PoisonError::into_inner);
                            t.push_str(&line);
                            t.push('\n');
                            if t.len() > STDERR_TAIL {
                                let cut = t.len() - STDERR_TAIL;
                                let cut = (cut..t.len())
                                    .find(|i| t.is_char_boundary(*i))
                                    .unwrap_or(t.len());
                                t.drain(..cut);
                            }
                        }
                        drop(stderr_done_tx);
                    }),
            );
        } else {
            drop(stderr_done_tx);
        }
        let log = self.log.clone();
        let tx = self.internal_tx.clone();
        drop(
            thread::Builder::new()
                .name("openchef-claude-reader".to_owned())
                .spawn(move || {
                    if let Some(stdout) = stdout {
                        read_lines(stdout, &log, |line| {
                            tx.send(Internal::Line { generation, line }).is_ok()
                        });
                    }
                    // stdout can close before stderr is fully read; wait briefly for it so a
                    // crash report carries the CLI's last words.
                    let _stderr_done = stderr_done_rx.recv_timeout(EXIT_WAIT);
                    drop(tx.send(Internal::Gone { generation }));
                }),
        );
    }

    fn stderr_tail(&self, generation: u64) -> String {
        self.generations
            .get(&generation)
            .map_or_else(String::new, |g| {
                g.stderr
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .trim()
                    .to_owned()
            })
    }

    /// Stops a generation immediately; the next turn starts a new one.
    fn kill(&mut self, generation: u64) {
        if self.current == Some(generation) {
            self.respawn = true;
        }
        let Some(g) = self.generations.get_mut(&generation) else {
            return;
        };
        g.killed = true;
        if let Some(p) = &g.process {
            let p = Arc::clone(p);
            drop(tokio::spawn(async move {
                p.terminate(Duration::ZERO, &CancellationToken::new()).await
            }));
        }
    }

    fn gone(&mut self, generation: u64) {
        let Some(g) = self.generations.get_mut(&generation) else {
            return;
        };
        g.done = true;
        let killed = g.killed;
        for token in g.perms.values() {
            token.cancel();
        }
        if self.current == Some(generation) && !self.closed {
            self.respawn = true; // an unexpected exit is recovered on the next turn
        }
        if self
            .turn
            .as_ref()
            .is_some_and(|t| t.generation == generation)
        {
            let (kind, message) = if killed {
                (
                    self.stop_kind(),
                    "claude process stopped by OpenChef".to_owned(),
                )
            } else {
                let tail = self.stderr_tail(generation);
                let base = "claude exited during the turn";
                (
                    FailureKind::Crashed,
                    if tail.is_empty() {
                        base.to_owned()
                    } else {
                        format!("{base}: {tail}")
                    },
                )
            };
            self.end_turn(EventBody::TurnFailed(Failure::new(kind, message)));
        }
    }

    // --- writing and control requests ----------------------------------------------

    fn write(&self, generation: u64, value: &Value) {
        let raw = value.to_string().into_bytes();
        self.log.line(Dir::Send, &raw);
        if let Some(stdin) = self
            .generations
            .get(&generation)
            .and_then(|g| g.stdin.as_ref())
        {
            let mut line = raw;
            line.push(b'\n');
            // A failed write means the process is going away, which `gone` reports.
            drop(stdin.queue(line));
        }
    }

    /// Sends a control request and waits for its response.
    async fn control(
        &mut self,
        generation: u64,
        subtype: &str,
        timeout: Duration,
    ) -> Result<ControlResponse, String> {
        self.control_seq += 1;
        let id = format!("{subtype}-{}", self.control_seq);
        drop(self.controls.insert(id.clone(), None));
        self.write(generation, &control_request(&id, subtype));
        let deadline = Instant::now() + timeout;
        let answered = self
            .wait_until(deadline, |a| {
                a.controls.get(&id).is_some_and(Option::is_some)
                    || a.generations.get(&generation).is_none_or(|g| g.done)
            })
            .await;
        let response = self.controls.remove(&id).flatten();
        match response {
            Some(r) if r.subtype == "error" => Err(format!("{subtype} rejected: {}", r.error)),
            Some(r) => Ok(r),
            None if answered => Err(format!("{subtype}: the session is closed")),
            None => Err(format!("{subtype}: timed out after {timeout:?}")),
        }
    }

    // --- turns ---------------------------------------------------------------------

    async fn start_turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        if self.closed {
            return Err(Error::SessionClosed);
        }
        if self.turn.is_some() {
            return Err(Error::TurnInProgress);
        }
        let old = self.current;
        let old_done = old
            .and_then(|g| self.generations.get(&g))
            .is_none_or(|g| g.done);
        let need_spawn = self.respawn || old_done || self.schema != input.output_schema;
        if !need_spawn
            && let Some(g) = old.and_then(|g| self.generations.get(&g))
            && g.written > g.results
        {
            // A live process still owes a result (D-009); a new message would be
            // credited with it.
            return Err(Error::TurnInProgress);
        }
        if need_spawn {
            if let Some(g) = old
                && !old_done
            {
                self.kill(g);
            }
            self.spawn(input.output_schema.clone(), true).await?;
        }
        let Some(generation) = self.current else {
            return Err(Error::SessionClosed);
        };
        let (tx, rx) = TurnSender::new();
        let ordinal = self.generations.get(&generation).map_or(0, |g| g.written);
        if let Some(g) = self.generations.get_mut(&generation) {
            g.written += 1;
        }
        self.next_turn += 1;
        let id = self.next_turn;
        let done = CancellationToken::new();
        let mut state = TurnState {
            id,
            generation,
            ordinal,
            tx,
            stop: None,
            tools: HashSet::new(),
            skip: HashSet::new(),
            done: done.clone(),
        };
        if let Some(q) = &self.quota {
            state.tx.emit(EventBody::Quota(q.clone())); // the latest known signal opens the turn
        }
        self.turn = Some(state);
        self.write(generation, &user_message(&input.text));
        let internal = self.internal_tx.clone();
        drop(tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => drop(internal.send(Internal::TurnCancelled { turn: id })),
                () = done.cancelled() => {}
            }
        }));
        Ok(rx)
    }

    /// Clears the turn before sending its terminal event, so a consumer that starts the
    /// next turn as soon as the stream ends never sees `TurnInProgress`.
    fn end_turn(&mut self, body: EventBody) {
        if let Some(mut t) = self.turn.take() {
            t.done.cancel();
            let _ended = t.tx.finish(body);
        }
    }

    fn stop_kind(&self) -> FailureKind {
        self.turn
            .as_ref()
            .and_then(|t| t.stop)
            .unwrap_or(FailureKind::Interrupted)
    }

    /// Asks Claude to stop and waits up to `wait` for the owed result. If it does not
    /// come, or the message was still queued (D-012), the process is killed and the next
    /// turn starts a new one with `--resume`.
    async fn interrupt_current(&mut self, why: FailureKind, wait: Duration) -> Result<(), Error> {
        let Some(t) = self.turn.as_mut() else {
            return Ok(());
        };
        t.stop.get_or_insert(why);
        let (turn, generation) = (t.id, t.generation);
        let deadline = Instant::now() + wait;
        self.cancel_permissions(generation);
        let response = self.control(generation, "interrupt", wait).await;
        if self.turn.as_ref().is_none_or(|t| t.id != turn) {
            return Ok(());
        }
        if response.as_ref().is_ok_and(|r| r.still_queued() > 0) {
            self.kill(generation);
            let kind = self.stop_kind();
            self.end_turn(EventBody::TurnFailed(Failure::new(
                kind,
                "turn interrupted before Claude started it; process replaced",
            )));
            return Ok(());
        }
        if self
            .wait_until(deadline, |a| a.turn.as_ref().is_none_or(|t| t.id != turn))
            .await
        {
            return Ok(());
        }
        self.kill(generation);
        let kind = self.stop_kind();
        self.end_turn(EventBody::TurnFailed(Failure::new(
            kind,
            "turn did not stop after interrupt; process killed",
        )));
        Ok(())
    }

    fn cancel_permissions(&self, generation: u64) {
        if let Some(g) = self.generations.get(&generation) {
            for token in g.perms.values() {
                token.cancel();
            }
        }
    }

    /// D-010: with a turn in flight, Claude would finish it after EOF and keep writing to
    /// the worktree; give it `close_wait` to stop, then kill without further grace.
    async fn shutdown(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let close_wait = self.options.close_wait;
        if let Some(generation) = self.turn.as_ref().map(|t| t.generation) {
            drop(
                self.interrupt_current(FailureKind::Interrupted, close_wait)
                    .await,
            );
            self.kill(generation);
        } else if let Some(g) = self.current.and_then(|g| self.generations.get_mut(&g)) {
            g.killed = true;
            if let Some(p) = &g.process {
                let p = Arc::clone(p);
                drop(tokio::spawn(async move {
                    p.terminate(openchef_procs::DEFAULT_GRACE, &CancellationToken::new())
                        .await
                }));
            }
        }
        let mut result = Ok(());
        if let Some(current) = self.current {
            let deadline = Instant::now() + close_wait;
            if !self
                .wait_until(deadline, |a| {
                    a.generations.get(&current).is_none_or(|g| g.done)
                })
                .await
            {
                self.kill(current);
                result = Err(start_error("process output did not close".to_owned()));
            }
        }
        // Output closing is not the process being gone: wait until every generation is
        // reaped, so nothing of this session outlives close.
        let processes: Vec<Arc<Process>> = self
            .generations
            .values()
            .filter_map(|g| g.process.clone())
            .collect();
        for p in processes {
            if tokio::time::timeout(close_wait, p.wait()).await.is_err()
                && let Err(e) = p.terminate(Duration::ZERO, &CancellationToken::new()).await
            {
                result = result.and(Err(start_error(format!("kill: {e}"))));
            }
        }
        self.end_turn(EventBody::TurnFailed(Failure::new(
            FailureKind::Interrupted,
            "session closed",
        )));
        self.life.cancel();
        if let Some(tmp) = self.tmp.take()
            && let Err(e) = tmp.close()
        {
            self.log
                .note(Dir::Note, &format!("claude: remove temp dir: {e}"));
        }
        if let Some(e) = self.log.error() {
            result = result.and(Err(start_error(format!("raw log: {e}"))));
        }
        result
    }

    // --- incoming lines ------------------------------------------------------------

    fn active(&mut self, generation: u64) -> Option<&mut TurnState> {
        self.turn.as_mut().filter(|t| t.generation == generation)
    }

    pub(crate) fn dispatch(&mut self, generation: u64, line: &Value) {
        let env: Envelope = match serde_json::from_value(line.clone()) {
            Ok(env) => env,
            Err(e) => {
                return self
                    .log
                    .note(Dir::Note, &format!("claude: protocol error: {e}"));
            }
        };
        match env.kind.as_str() {
            "control_response" => {
                if let Ok(body) = serde_json::from_value::<ControlResponse>(env.response.clone())
                    && let Some(slot) = self.controls.get_mut(&body.request_id)
                {
                    *slot = Some(body);
                }
            }
            "control_request" => self.on_control_request(generation, &env),
            "control_cancel_request" => {
                if let Some(token) = self
                    .generations
                    .get(&generation)
                    .and_then(|g| g.perms.get(&env.request_id))
                {
                    token.cancel();
                }
            }
            "stream_event" if !env.is_subagent() => {
                if let Some(text) = text_delta(&env.event)
                    && let Some(t) = self.active(generation)
                {
                    t.tx.emit(EventBody::Text(text.to_owned()));
                }
            }
            "assistant" if !env.is_subagent() => self.on_assistant(generation, &env.message),
            "user" if !env.is_subagent() => self.on_user(generation, &env.message),
            "rate_limit_event" => {
                if let Some(signal) = quota_signal(&env.rate_limit_info) {
                    self.quota = Some(signal.clone());
                    if let Some(t) = self.active(generation) {
                        t.tx.emit(EventBody::Quota(signal));
                    }
                }
            }
            "result" => self.on_result(generation, line),
            _ => {}
        }
    }

    fn on_control_request(&mut self, generation: u64, env: &Envelope) {
        let req: CanUseTool = serde_json::from_value(env.request.clone()).unwrap_or_default();
        if req.subtype != "can_use_tool" {
            let reply = error_response(
                &env.request_id,
                &format!("OpenChef does not support {}", req.subtype),
            );
            return self.write(generation, &reply);
        }
        let ask_user = self.spec.ask_user.as_ref().map(ask_user_tool);
        if self.active(generation).is_none() {
            return self.write(
                generation,
                &permission_response(
                    &env.request_id,
                    &req.input,
                    &Decision::deny("no OpenChef turn is active"),
                ),
            );
        }
        if ask_user.as_deref() == Some(req.tool_name.as_str()) {
            return self.write(
                generation,
                &permission_response(&env.request_id, &req.input, &Decision::allow()),
            );
        }
        // Register the cancel token now, in stream order: a control_cancel_request read
        // right after must find it.
        let token = self.life.child_token();
        if let Some(g) = self.generations.get_mut(&generation) {
            drop(g.perms.insert(env.request_id.clone(), token.clone()));
        }
        let request = req.approval_request(self.session.clone(), &env.request);
        let approver = Arc::clone(&self.spec.approver);
        let tx = self.internal_tx.clone();
        let request_id = env.request_id.clone();
        drop(tokio::spawn(async move {
            let decision = approver.approve(request, token).await.unwrap_or_else(|e| {
                Decision::deny(format!("OpenChef could not decide this request: {e}"))
            });
            drop(tx.send(Internal::Decided {
                generation,
                request_id,
                input: req.input,
                decision,
            }));
        }));
    }

    fn on_assistant(&mut self, generation: u64, message: &Value) {
        let Some(t) = self.active(generation) else {
            return;
        };
        for b in blocks(message) {
            if b.kind != "tool_use" || b.id.is_empty() {
                continue;
            }
            if b.name == STRUCTURED_OUTPUT_TOOL {
                let _new = t.skip.insert(b.id);
                continue;
            }
            if t.skip.contains(&b.id) || !t.tools.insert(b.id.clone()) {
                continue; // repeated, as with partial messages
            }
            let signature = signature(&b.name, &b.input);
            t.tx.emit(EventBody::ToolCall(ToolCall {
                id: b.id,
                name: b.name,
                input: b.input,
                signature,
            }));
        }
    }

    fn on_user(&mut self, generation: u64, message: &Value) {
        let Some(t) = self.active(generation) else {
            return;
        };
        for b in blocks(message) {
            if b.kind != "tool_result" || t.skip.contains(&b.tool_use_id) {
                continue;
            }
            let output = tool_result_text(&b.content);
            t.tx.emit(EventBody::ToolResult(ToolResult {
                id: b.tool_use_id,
                is_error: b.is_error,
                output,
            }));
        }
    }

    /// Ends the turn whose ordinal matches the result (D-009); any other result is stale
    /// (e.g. the late result of an interrupted turn) and is ignored.
    fn on_result(&mut self, generation: u64, line: &Value) {
        let r: ResultMessage = match serde_json::from_value(line.clone()) {
            Ok(r) => r,
            Err(e) => {
                return self
                    .log
                    .note(Dir::Note, &format!("claude: undecodable result: {e}"));
            }
        };
        let Some(g) = self.generations.get_mut(&generation) else {
            return;
        };
        let index = r.result_index.unwrap_or(g.results);
        g.results += 1;
        self.started = true; // a result means Claude has a transcript for this id
        let matches = self
            .turn
            .as_ref()
            .is_some_and(|t| t.generation == generation && t.ordinal == index);
        if !matches {
            return self.log.note(
                Dir::Note,
                &format!("claude: ignoring result {index} (no matching turn)"),
            );
        }
        let stop = self.turn.as_ref().and_then(|t| t.stop);
        let body = r.outcome(stop, self.quota.as_ref());
        self.end_turn(body);
    }

    // --- test support ----------------------------------------------------------------

    /// Adds a generation without a process (replay tests).
    #[cfg(test)]
    pub(crate) fn attach_detached(&mut self) -> u64 {
        let id = self.next_generation;
        self.next_generation += 1;
        drop(self.generations.insert(id, Generation::detached()));
        self.current = Some(id);
        id
    }

    /// Opens a turn on `generation` as if a user message was written (replay tests).
    #[cfg(test)]
    pub(crate) fn open_turn(&mut self, generation: u64) -> TurnEvents {
        let (tx, rx) = TurnSender::new();
        let ordinal = self.generations.get(&generation).map_or(0, |g| g.written);
        if let Some(g) = self.generations.get_mut(&generation) {
            g.written += 1;
        }
        self.next_turn += 1;
        self.turn = Some(TurnState {
            id: self.next_turn,
            generation,
            ordinal,
            tx,
            stop: None,
            tools: HashSet::new(),
            skip: HashSet::new(),
            done: CancellationToken::new(),
        });
        rx
    }
}

/// Reads JSON lines, logging every raw line before parsing (raw first). `deliver`
/// returns false when nobody listens any more.
fn read_lines(stdout: impl Read, log: &RawLog, mut deliver: impl FnMut(Value) -> bool) {
    let mut reader = BufReader::with_capacity(64 << 10, stdout);
    loop {
        match crate::linerpc::read_line(&mut reader, MAX_LINE) {
            Ok(Some(line)) if line.is_empty() => {}
            Ok(Some(line)) => {
                log.line(Dir::Recv, &line);
                match serde_json::from_slice::<Value>(&line) {
                    Ok(v) => {
                        if !deliver(v) {
                            return;
                        }
                    }
                    Err(e) => log.note(Dir::Note, &format!("claude: protocol error: {e}")),
                }
            }
            Ok(None) => return,
            Err(e) => {
                log.note(Dir::Note, &format!("claude: read: {e}"));
                return;
            }
        }
    }
}

/// The CLI's environment: the spec's, plus the MCP tool timeout Claude reads from
/// `MCP_TOOL_TIMEOUT` (milliseconds) when OpenChef's server needs a longer one.
fn process_env(spec: &SessionSpec) -> Vec<(String, String)> {
    let Some(timeout) = spec.ask_user.as_ref().and_then(|m| m.tool_timeout) else {
        return spec.env.clone();
    };
    let mut env: Vec<(String, String)> = spec
        .env
        .iter()
        .filter(|(k, _)| k != "MCP_TOOL_TIMEOUT")
        .cloned()
        .collect();
    env.push((
        "MCP_TOOL_TIMEOUT".to_owned(),
        timeout.as_millis().to_string(),
    ));
    env
}

/// Validates a spec before anything starts.
pub(crate) fn validate(spec: &SessionSpec) -> Result<(), Error> {
    let _mode = mode_and_tools(spec);
    if !spec.dir.is_dir() {
        return Err(start_error(format!(
            "working directory {} does not exist",
            spec.dir.display()
        )));
    }
    Ok(())
}

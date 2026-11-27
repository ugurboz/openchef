//! A scriptable in-process [`Runtime`] for engine tests (`docs/architecture.md` §18),
//! ported from Go `internal/runtime/fake`. Each [`FakeRuntime`] plays back a queue of
//! scripted turns shared by all of its sessions, so a test can say "the first agent
//! hits its quota, the second one finishes" without any CLI installed.
//!
//! State shared between the runtime handle, its sessions and their turn tasks is a
//! small leaf value behind a `std::sync::Mutex` that is never held across `.await`
//! (D-030 allows this for leaves).

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use openchef_procs::Identity;
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, Decision, Error, EventBody, Failure, FailureKind,
    Question, Runtime, Session, SessionRef, SessionSpec, ToolCall, TurnEvents, TurnInput,
    TurnResult, TurnSender, Usage,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Above every real PID limit (Linux caps at 2^22), so the identities a fake reports
/// through `on_spawn` never belong to a live process.
const PID_BASE: u32 = 1 << 23;

/// Stands in for the agent's file changes; runs in the session directory. An error
/// fails the turn as crashed.
pub type Edit = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

/// One scripted turn.
#[derive(Default)]
pub struct Turn {
    /// Emitted before the terminal event (text, tool calls, quota signals).
    pub events: Vec<EventBody>,
    /// Runs before the turn ends.
    pub edit: Option<Edit>,
    /// Ends the turn with this failure instead of completing.
    pub fail: Option<Failure>,
    /// Blocks the turn until it is interrupted or its cancel token fires.
    pub hang: bool,
    /// Ignores interrupt and close; only the turn's cancel token ends it, like an agent
    /// that does not react to an interrupt request.
    pub ignore_interrupt: bool,
    /// The final answer of a completed turn.
    pub text: String,
    /// Asked of the session's approver before `edit` runs, as an agent asks before
    /// acting. A denial skips `edit`; the turn still completes, with the denial message
    /// as its final text. The decision is recorded ([`FakeRuntime::decisions`]).
    pub approve: Option<ApprovalRequest>,
    /// Put to the approver before `edit`; the answer becomes the final text unless
    /// `text` is set.
    pub ask: Option<Question>,
}

impl fmt::Debug for Turn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Turn")
            .field("events", &self.events)
            .field("edit", &self.edit.is_some())
            .field("fail", &self.fail)
            .field("hang", &self.hang)
            .field("ignore_interrupt", &self.ignore_interrupt)
            .field("text", &self.text)
            .field("approve", &self.approve)
            .field("ask", &self.ask)
            .finish()
    }
}

impl Turn {
    /// A turn that completes with `text`.
    #[must_use]
    pub fn completed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// A turn that fails with `kind`.
    #[must_use]
    pub fn failing(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            fail: Some(Failure::new(kind, message)),
            ..Self::default()
        }
    }

    /// A turn that runs `edit` in the session directory, then completes with `text`.
    #[must_use]
    pub fn editing(
        text: impl Into<String>,
        edit: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            text: text.into(),
            edit: Some(Box::new(edit)),
            ..Self::default()
        }
    }
}

/// A tool-call event with the given signature.
#[must_use]
pub fn tool_call(name: &str, signature: &str) -> EventBody {
    EventBody::ToolCall(ToolCall {
        id: String::new(),
        name: name.to_owned(),
        input: serde_json::Value::Null,
        signature: signature.to_owned(),
    })
}

/// What a recorded call was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// [`Runtime::open`].
    Open,
    /// [`Runtime::resume`].
    Resume,
    /// [`Session::turn`].
    Turn,
    /// [`Session::interrupt`].
    Interrupt,
    /// [`Session::close`].
    Close,
}

/// One recorded interaction, for assertions.
#[derive(Debug, Clone)]
pub struct Call {
    /// What it was.
    pub kind: CallKind,
    /// The session id.
    pub session: String,
    /// The message of a turn.
    pub text: String,
    /// The spec of an open or resume.
    pub spec: Option<SessionSpec>,
}

#[derive(Default)]
struct Shared {
    script: VecDeque<Turn>,
    calls: Vec<Call>,
    sessions: u32,
    open_errors: VecDeque<Error>,
    live: HashSet<String>,
    decisions: Vec<Decision>,
}

/// A fake agent CLI. Clones share the script and the recordings.
#[derive(Clone)]
pub struct FakeRuntime {
    name: &'static str,
    shared: Arc<Mutex<Shared>>,
}

impl fmt::Debug for FakeRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeRuntime")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking test thread poisons the lock; the recordings stay usable.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeRuntime {
    /// A fake runtime named `name` that plays `turns` in order.
    #[must_use]
    pub fn new(name: &'static str, turns: impl IntoIterator<Item = Turn>) -> Self {
        let shared = Shared {
            script: turns.into_iter().collect(),
            ..Shared::default()
        };
        Self {
            name,
            shared: Arc::new(Mutex::new(shared)),
        }
    }

    /// Appends turns to the script.
    pub fn push(&self, turns: impl IntoIterator<Item = Turn>) {
        lock(&self.shared).script.extend(turns);
    }

    /// Makes the next open or resume fail with `error`.
    pub fn fail_next_open(&self, error: Error) {
        lock(&self.shared).open_errors.push_back(error);
    }

    /// Every recorded call, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.shared).calls.clone()
    }

    /// The messages of every turn, in order.
    #[must_use]
    pub fn turns(&self) -> Vec<String> {
        lock(&self.shared)
            .calls
            .iter()
            .filter(|c| c.kind == CallKind::Turn)
            .map(|c| c.text.clone())
            .collect()
    }

    /// The number of scripted turns not yet played.
    #[must_use]
    pub fn remaining(&self) -> usize {
        lock(&self.shared).script.len()
    }

    /// The number of sessions not yet closed.
    #[must_use]
    pub fn live(&self) -> usize {
        lock(&self.shared).live.len()
    }

    /// The approver's decisions on scripted `approve` requests, in order.
    #[must_use]
    pub fn decisions(&self) -> Vec<Decision> {
        lock(&self.shared).decisions.clone()
    }

    fn record(&self, kind: CallKind, session: &str, text: &str) {
        lock(&self.shared).calls.push(Call {
            kind,
            session: session.to_owned(),
            text: text.to_owned(),
            spec: None,
        });
    }

    async fn start(
        &self,
        spec: SessionSpec,
        id: Option<String>,
        kind: CallKind,
    ) -> Result<FakeSession, Error> {
        let (n, id, open_error) = {
            let mut s = lock(&self.shared);
            s.sessions += 1;
            let n = s.sessions;
            let id = id.unwrap_or_else(|| format!("{}-session-{n}", self.name));
            s.calls.push(Call {
                kind,
                session: id.clone(),
                text: String::new(),
                spec: Some(spec.clone()),
            });
            (n, id, s.open_errors.pop_front())
        };
        if let Some(e) = open_error {
            return Err(e);
        }
        spec.spawned(Identity {
            pid: PID_BASE + n,
            start_token: u64::from(n),
        })
        .await?;
        let _inserted = lock(&self.shared).live.insert(id.clone());
        Ok(FakeSession {
            runtime: self.clone(),
            session: SessionRef {
                runtime: self.name.to_owned(),
                id,
            },
            dir: spec.dir,
            approver: spec.approver,
            state: Arc::new(Mutex::new(TurnFlag::default())),
            current: None,
            closed: false,
        })
    }
}

impl Runtime for FakeRuntime {
    type Session = FakeSession;

    fn name(&self) -> &'static str {
        self.name
    }

    async fn open(&self, spec: SessionSpec) -> Result<FakeSession, Error> {
        self.start(spec, None, CallKind::Open).await
    }

    async fn resume(&self, session: SessionRef, spec: SessionSpec) -> Result<FakeSession, Error> {
        if session.runtime != self.name || session.id.is_empty() {
            return Err(Error::Start {
                runtime: self.name,
                message: format!("fake: cannot resume {session:?}"),
            });
        }
        self.start(spec, Some(session.id), CallKind::Resume).await
    }
}

#[derive(Default)]
struct TurnFlag {
    turning: bool,
}

struct Current {
    stop: CancellationToken,
    task: JoinHandle<()>,
    stubborn: bool,
}

/// A session of a [`FakeRuntime`].
pub struct FakeSession {
    runtime: FakeRuntime,
    session: SessionRef,
    dir: PathBuf,
    approver: Arc<dyn Approver>,
    state: Arc<Mutex<TurnFlag>>,
    current: Option<Current>,
    closed: bool,
}

impl fmt::Debug for FakeSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeSession")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Session for FakeSession {
    fn session_ref(&self) -> &SessionRef {
        &self.session
    }

    /// Starts the turn synchronously: the fake has nothing to wait for.
    fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<TurnEvents, Error>> + Send {
        std::future::ready(self.start_turn(&input, cancel))
    }

    fn interrupt(&mut self) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(self.stop_turn())
    }

    /// Like a real CLI, nothing of this session touches the workspace after close. A
    /// stubborn turn ends only with its cancel token; close does not wait for it.
    async fn close(mut self) -> Result<(), Error> {
        self.closed = true;
        if let Some(c) = self.current.take() {
            c.stop.cancel();
            if !c.stubborn {
                // A panicked turn task already failed the test that scripted it.
                let _joined = c.task.await;
            }
        }
        self.runtime.record(CallKind::Close, &self.session.id, "");
        let _removed = lock(&self.runtime.shared).live.remove(&self.session.id);
        Ok(())
    }
}

impl FakeSession {
    fn start_turn(
        &mut self,
        input: &TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        if self.closed {
            return Err(Error::SessionClosed);
        }
        {
            let mut flag = lock(&self.state);
            if flag.turning {
                return Err(Error::TurnInProgress);
            }
            flag.turning = true;
        }
        self.runtime
            .record(CallKind::Turn, &self.session.id, &input.text);
        let script = lock(&self.runtime.shared).script.pop_front();
        let stubborn = script.as_ref().is_some_and(|t| t.ignore_interrupt);
        let stop = CancellationToken::new();
        let (tx, rx) = TurnSender::new();
        let player = Player {
            runtime: self.runtime.clone(),
            dir: self.dir.clone(),
            approver: Arc::clone(&self.approver),
            state: Arc::clone(&self.state),
            stop: if stubborn {
                CancellationToken::new()
            } else {
                stop.clone()
            },
            cancel,
        };
        let task = tokio::spawn(player.play(script, tx));
        self.current = Some(Current {
            stop,
            task,
            stubborn,
        });
        Ok(rx)
    }

    fn stop_turn(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::SessionClosed);
        }
        self.runtime
            .record(CallKind::Interrupt, &self.session.id, "");
        if let Some(c) = &self.current {
            c.stop.cancel();
        }
        Ok(())
    }
}

/// Plays one scripted turn on its own task.
struct Player {
    runtime: FakeRuntime,
    dir: PathBuf,
    approver: Arc<dyn Approver>,
    state: Arc<Mutex<TurnFlag>>,
    stop: CancellationToken,
    cancel: CancellationToken,
}

impl Player {
    async fn play(self, script: Option<Turn>, mut tx: TurnSender) {
        let end = match script {
            Some(turn) => self.outcome(turn, &mut tx).await,
            None => failed(FailureKind::Protocol, "fake: script exhausted"),
        };
        // Like the real adapters, the session accepts the next turn before the terminal
        // event is delivered.
        lock(&self.state).turning = false;
        let _delivered = tx.finish(end); // nobody may be listening any more
    }

    async fn outcome(&self, mut turn: Turn, tx: &mut TurnSender) -> EventBody {
        let start = Instant::now();
        // The edit closure is not `Sync`; keep it out of what the awaits below borrow.
        let mut edit = turn.edit.take();
        for body in std::mem::take(&mut turn.events) {
            tx.emit(body);
        }
        if turn.hang {
            tokio::select! {
                () = self.stop.cancelled() => return interrupted(),
                () = self.cancel.cancelled() => return deadline(),
            }
        }
        if self.stop.is_cancelled() {
            return interrupted();
        }
        if let (approve, ask) = (turn.approve.take(), turn.ask.take())
            && (approve.is_some() || ask.is_some())
        {
            // Interrupt and the turn's cancel token both end the consultation.
            let consult_cancel = CancellationToken::new();
            let watcher = {
                let (ask, stop, cancel) = (
                    consult_cancel.clone(),
                    self.stop.clone(),
                    self.cancel.clone(),
                );
                tokio::spawn(async move {
                    tokio::select! {
                        () = stop.cancelled() => ask.cancel(),
                        () = cancel.cancelled() => ask.cancel(),
                        () = ask.cancelled() => {}
                    }
                })
            };
            let consulted = self.consult(approve, ask, consult_cancel.clone()).await;
            consult_cancel.cancel();
            let _watched = watcher.await; // ends as soon as `ask` is cancelled
            match consulted {
                Err(_) if self.cancel.is_cancelled() => return deadline(),
                Err(_) => return interrupted(),
                Ok((text, allowed)) => {
                    if turn.text.is_empty() {
                        turn.text = text;
                    }
                    if !allowed {
                        edit = None;
                    }
                }
            }
        }
        if let Some(edit) = edit
            && let Err(e) = edit(&self.dir)
        {
            return failed(FailureKind::Crashed, format!("fake: edit: {e}"));
        }
        if let Some(f) = turn.fail {
            return EventBody::TurnFailed(f);
        }
        EventBody::TurnCompleted(TurnResult {
            final_text: turn.text,
            structured: None,
            usage: Usage::default(),
            denials: 0,
            models: Vec::new(),
            duration: start.elapsed(),
        })
    }

    /// Asks the turn's approval and question; returns the text the agent would end with
    /// and whether it may go on to edit.
    async fn consult(
        &self,
        approve: Option<ApprovalRequest>,
        ask: Option<Question>,
        cancel: CancellationToken,
    ) -> Result<(String, bool), ApproverError> {
        let (mut text, mut allowed) = (String::new(), true);
        if let Some(request) = approve {
            let decision = self.approver.approve(request, cancel.clone()).await?;
            allowed = decision.allow;
            text.clone_from(&decision.message);
            lock(&self.runtime.shared).decisions.push(decision);
        }
        if let Some(question) = ask {
            text = self.approver.answer(question, cancel).await?;
        }
        Ok((text, allowed))
    }
}

fn failed(kind: FailureKind, message: impl Into<String>) -> EventBody {
    EventBody::TurnFailed(Failure::new(kind, message))
}

fn interrupted() -> EventBody {
    failed(FailureKind::Interrupted, "fake: interrupted")
}

fn deadline() -> EventBody {
    failed(FailureKind::Timeout, "fake: turn deadline")
}

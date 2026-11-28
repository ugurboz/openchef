//! How a session is started, and the contract every adapter implements (§6.1).

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openchef_core::{Effort, PermissionMode};
use openchef_procs::Identity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Approver, BoxFuture, Error, Event};

/// The CLI-native identity of a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionRef {
    /// `claude`, `codex`, `agy` (or `fake`).
    pub runtime: String,
    /// The Claude session id, the Codex thread id or the ACP session id.
    pub id: String,
}

/// A stdio MCP server command (OpenChef's `ask_user`, D-024).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The server name the agent sees.
    pub name: String,
    /// The program.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its environment.
    pub env: Vec<(String, String)>,
    /// How long the CLI waits for one tool call; `None` keeps the CLI's default.
    /// `ask_user` waits for a person, far beyond the defaults.
    pub tool_timeout: Option<Duration>,
}

/// Called after each agent process starts and before anything is written to it, so
/// the engine records the process first (§5.1, D-019). An error kills the process and
/// fails the operation that started it.
pub type OnSpawn = Arc<dyn Fn(Identity) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// How to run a session.
#[derive(Clone)]
pub struct SessionSpec {
    /// Working directory (the task's worktree).
    pub dir: PathBuf,
    /// Passed to the CLI as is; `None` is the CLI's default.
    pub model: Option<String>,
    /// Normalized effort; adapters map it to the CLI's values. `None` lets the CLI decide.
    pub effort: Option<Effort>,
    /// Permission mode (Claude Code vocabulary, §9).
    pub mode: PermissionMode,
    /// Rules that must never be allowed (passed natively where the CLI supports it).
    pub deny_rules: Vec<String>,
    /// Rules that must always reach the approver.
    pub ask_rules: Vec<String>,
    /// Enables the CLI's web tools. Off by default: under prompt injection they are an
    /// exfiltration channel (D-006).
    pub web_access: bool,
    /// The complete child environment (never inherited implicitly).
    pub env: Vec<(String, String)>,
    /// Decides permission requests and answers questions.
    pub approver: Arc<dyn Approver>,
    /// OpenChef's `ask_user` MCP server, if any.
    pub ask_user: Option<McpServer>,
    /// Receives every raw protocol line.
    pub log_path: Option<PathBuf>,
    /// Called for every process the session starts.
    pub on_spawn: Option<OnSpawn>,
    /// Starts every process of the session inside OpenChef's OS sandbox (§21.3).
    /// The sandbox replaces the CLI's own, which cannot nest (D-032).
    pub confine: Option<openchef_procs::Confine>,
}

impl fmt::Debug for SessionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSpec")
            .field("dir", &self.dir)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("mode", &self.mode)
            .field("deny_rules", &self.deny_rules)
            .field("ask_rules", &self.ask_rules)
            .field("web_access", &self.web_access)
            .field("confined", &self.confine.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionSpec {
    /// `spec` as the session's sandbox starts it; unchanged without one.
    ///
    /// # Errors
    ///
    /// Why the sandbox refused: the process must not start.
    pub fn confined(&self, spec: openchef_procs::Spec) -> Result<openchef_procs::Spec, String> {
        match &self.confine {
            Some(confine) => confine(spec),
            None => Ok(spec),
        }
    }

    /// Reports a newly started process to `on_spawn`. Adapters call it right after the
    /// start and before the first write, and kill the process on error.
    pub async fn spawned(&self, id: Identity) -> Result<(), Error> {
        match &self.on_spawn {
            None => Ok(()),
            Some(f) => f(id).await.map_err(|reason| Error::SpawnRejected {
                pid: id.pid,
                reason,
            }),
        }
    }
}

/// One user message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnInput {
    /// The message.
    pub text: String,
    /// A JSON Schema the final answer must satisfy.
    pub output_schema: Option<Value>,
}

impl TurnInput {
    /// A plain message.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            output_schema: None,
        }
    }
}

/// The events of one turn: every event of this turn and nothing else, ending with
/// exactly one terminal event, after which the channel closes. Producers never block
/// on it (D-002).
pub type TurnEvents = mpsc::UnboundedReceiver<Event>;

/// Starts and resumes sessions for one agent CLI.
pub trait Runtime: Send + Sync + 'static {
    /// The session type.
    type Session: Session;

    /// `claude`, `codex`, `agy`.
    fn name(&self) -> &'static str;

    /// Starts a new session.
    fn open(&self, spec: SessionSpec) -> impl Future<Output = Result<Self::Session, Error>> + Send;

    /// Continues a session started earlier, possibly by another process.
    fn resume(
        &self,
        session: SessionRef,
        spec: SessionSpec,
    ) -> impl Future<Output = Result<Self::Session, Error>> + Send;
}

/// One live conversation. `&mut self` makes concurrent turns impossible.
pub trait Session: Send + 'static {
    /// The identity to persist and resume.
    fn session_ref(&self) -> &SessionRef;

    /// Sends one message and returns this turn's events. `cancel` ends the turn as
    /// `timeout`; [`Session::interrupt`] ends it as `interrupted`.
    fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<TurnEvents, Error>> + Send;

    /// Cancels the turn in progress; the session stays usable.
    fn interrupt(&mut self) -> impl Future<Output = Result<(), Error>> + Send;

    /// Ends the session and its process. Consumes the session; `Drop` still kills the
    /// process tree if this is never awaited.
    fn close(self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Built-in invariants of §9.2(1) and §8.2 written as permission rules (Claude Code
/// syntax). Adapters with native rules pass them in every mode; approvers enforce them
/// for every request. Bash rules are best effort; git stays with the kernel (§8.2).
pub const INVARIANT_DENY_RULES: &[&str] = &[
    "Edit(**/.claude/**)",
    "Write(**/.claude/**)",
    "Edit(**/.git/**)",
    "Write(**/.git/**)",
    "Edit(**/.openchef/**)",
    "Write(**/.openchef/**)",
    "Bash(git commit:*)",
    "Bash(git push:*)",
    "Bash(git reset:*)",
    "Bash(git rebase:*)",
    "Bash(git checkout:*)",
    "Bash(git switch:*)",
    "Bash(git merge:*)",
    "Bash(git tag:*)",
    "Bash(git branch -D:*)",
    "Bash(git branch -d:*)",
    "Bash(git update-ref:*)",
];

/// The highest level in `supported` that does not exceed `wanted`; the lowest supported
/// level when every one is above it; `None` when `supported` is empty or nothing is
/// wanted.
#[must_use]
pub fn nearest_effort(wanted: Option<Effort>, supported: &[Effort]) -> Option<Effort> {
    let wanted = wanted?;
    supported
        .iter()
        .copied()
        .filter(|s| *s <= wanted)
        .max()
        .or_else(|| supported.iter().copied().min())
}

/// Parses a permission mode, accepting Claude Code's legacy alias `default`.
pub fn parse_mode(s: &str) -> Result<PermissionMode, openchef_core::Error> {
    if s == "default" {
        Ok(PermissionMode::Manual)
    } else {
        s.parse()
    }
}

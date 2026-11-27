//! Drives Claude Code headless (`claude -p` with stream-json I/O and the Agent SDK
//! control protocol), per `docs/architecture.md` §6.2 and D-006…D-013.

mod args;
mod protocol;
#[cfg(test)]
mod replay_tests;
mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub use session::ClaudeSession;

use crate::{Error, Runtime, Session, SessionRef, SessionSpec, TurnEvents, TurnInput};

/// The runtime identifier.
pub const NAME: &str = "claude";

/// How to run Claude Code.
#[derive(Debug, Clone)]
pub struct ClaudeOptions {
    /// The executable (default `claude`, resolved through `PATH`).
    pub binary: PathBuf,
    /// Arguments inserted before OpenChef's flags (tests run a scripted fake).
    pub prefix_args: Vec<String>,
    /// How long `initialize` may take (it answers in ~0.2 s, m1-findings §9).
    pub init_timeout: Duration,
    /// D-009: how long an interrupt waits for the owed result.
    pub interrupt_wait: Duration,
    /// D-010: how long close waits for an in-flight turn.
    pub close_wait: Duration,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            prefix_args: Vec::new(),
            init_timeout: Duration::from_secs(15),
            interrupt_wait: Duration::from_secs(10),
            close_wait: Duration::from_secs(3),
        }
    }
}

/// Claude Code as a [`Runtime`].
#[derive(Debug, Clone, Default)]
pub struct ClaudeRuntime {
    options: Arc<ClaudeOptions>,
}

impl ClaudeRuntime {
    /// A runtime with these options.
    #[must_use]
    pub fn new(options: ClaudeOptions) -> Self {
        Self {
            options: Arc::new(options),
        }
    }
}

impl Runtime for ClaudeRuntime {
    type Session = ClaudeSession;

    fn name(&self) -> &'static str {
        NAME
    }

    /// Starts a session with an id chosen by OpenChef (`--session-id`).
    async fn open(&self, spec: SessionSpec) -> Result<ClaudeSession, Error> {
        session::validate(&spec)?;
        ClaudeSession::begin(
            Arc::clone(&self.options),
            spec,
            uuid::Uuid::new_v4().to_string(),
            false,
        )
        .await
    }

    /// Continues a session in a new process (`--resume`).
    async fn resume(&self, session: SessionRef, spec: SessionSpec) -> Result<ClaudeSession, Error> {
        if session.runtime != NAME || session.id.is_empty() {
            return Err(Error::Start {
                runtime: NAME,
                message: format!("cannot resume {session:?}"),
            });
        }
        session::validate(&spec)?;
        ClaudeSession::begin(Arc::clone(&self.options), spec, session.id, true).await
    }
}

impl Session for ClaudeSession {
    fn session_ref(&self) -> &SessionRef {
        ClaudeSession::session_ref(self)
    }

    async fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        ClaudeSession::turn(self, input, cancel).await
    }

    async fn interrupt(&mut self) -> Result<(), Error> {
        ClaudeSession::interrupt(self).await
    }

    async fn close(self) -> Result<(), Error> {
        ClaudeSession::close(self).await
    }
}

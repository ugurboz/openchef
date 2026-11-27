//! Drives Codex CLI through `codex app-server` (newline-delimited JSON-RPC over stdio),
//! per `docs/architecture.md` §6.3 and D-003…D-005.

mod protocol;
#[cfg(test)]
mod replay_tests;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

pub use session::CodexSession;
use session::Thread;

use crate::{Error, Runtime, Session, SessionRef, SessionSpec, TurnEvents, TurnInput};

/// The runtime identifier.
pub const NAME: &str = "codex";

/// How to run Codex.
#[derive(Debug, Clone)]
pub struct CodexOptions {
    /// The executable (default `codex`, resolved through `PATH`).
    pub binary: PathBuf,
    /// The arguments that start the app server (default `["app-server"]`); tests run
    /// a scripted fake.
    pub args: Vec<String>,
    /// Reported to Codex in `initialize`.
    pub client_version: String,
}

impl Default for CodexOptions {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("codex"),
            args: vec!["app-server".to_owned()],
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Codex as a [`Runtime`].
#[derive(Debug, Clone, Default)]
pub struct CodexRuntime {
    options: Arc<CodexOptions>,
}

impl CodexRuntime {
    /// A runtime with these options.
    #[must_use]
    pub fn new(options: CodexOptions) -> Self {
        Self {
            options: Arc::new(options),
        }
    }
}

impl Runtime for CodexRuntime {
    type Session = CodexSession;

    fn name(&self) -> &'static str {
        NAME
    }

    /// Starts a new thread.
    async fn open(&self, spec: SessionSpec) -> Result<CodexSession, Error> {
        CodexSession::begin(&self.options, spec, Thread::New).await
    }

    /// Rejoins an existing thread in a new app-server process.
    async fn resume(&self, session: SessionRef, spec: SessionSpec) -> Result<CodexSession, Error> {
        if session.runtime != NAME || session.id.is_empty() {
            return Err(Error::Start {
                runtime: NAME,
                message: format!("cannot resume {session:?}"),
            });
        }
        CodexSession::begin(&self.options, spec, Thread::Resume(session.id)).await
    }
}

impl Session for CodexSession {
    fn session_ref(&self) -> &SessionRef {
        CodexSession::session_ref(self)
    }

    async fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<TurnEvents, Error> {
        CodexSession::turn(self, input, cancel).await
    }

    async fn interrupt(&mut self) -> Result<(), Error> {
        CodexSession::interrupt(self).await
    }

    async fn close(self) -> Result<(), Error> {
        CodexSession::close(self).await
    }
}

//! Object-safe views of [`Runtime`] and [`Session`], so the engine can hold the
//! configured runtimes in one map and switch between them per attempt.

use std::sync::Arc;

use openchef_runtime::{
    BoxFuture, Error, Runtime, Session, SessionRef, SessionSpec, TurnEvents, TurnInput,
};
use tokio_util::sync::CancellationToken;

/// A runtime the engine can open sessions on.
pub trait DynRuntime: Send + Sync {
    /// The runtime identifier (`claude`, `codex`, `agy`).
    fn name(&self) -> &'static str;
    /// Starts a new conversation.
    fn open(&self, spec: SessionSpec) -> BoxFuture<'_, Result<Box<dyn DynSession>, Error>>;
    /// Continues a conversation in a new process.
    fn resume(
        &self,
        session: SessionRef,
        spec: SessionSpec,
    ) -> BoxFuture<'_, Result<Box<dyn DynSession>, Error>>;
}

/// An open session.
pub trait DynSession: Send {
    /// Its native reference.
    fn session_ref(&self) -> &SessionRef;
    /// Sends one message; the stream ends with exactly one terminal event.
    fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<TurnEvents, Error>>;
    /// Asks the running turn to stop.
    fn interrupt(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Stops the CLI.
    fn close(self: Box<Self>) -> BoxFuture<'static, Result<(), Error>>;
}

impl<R> DynRuntime for R
where
    R: Runtime,
    R::Session: Send + 'static,
{
    fn name(&self) -> &'static str {
        Runtime::name(self)
    }

    fn open(&self, spec: SessionSpec) -> BoxFuture<'_, Result<Box<dyn DynSession>, Error>> {
        Box::pin(
            async move { Ok(Box::new(Runtime::open(self, spec).await?) as Box<dyn DynSession>) },
        )
    }

    fn resume(
        &self,
        session: SessionRef,
        spec: SessionSpec,
    ) -> BoxFuture<'_, Result<Box<dyn DynSession>, Error>> {
        Box::pin(async move {
            Ok(Box::new(Runtime::resume(self, session, spec).await?) as Box<dyn DynSession>)
        })
    }
}

impl<S: Session> DynSession for S {
    fn session_ref(&self) -> &SessionRef {
        Session::session_ref(self)
    }

    fn turn(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<TurnEvents, Error>> {
        Box::pin(Session::turn(self, input, cancel))
    }

    fn interrupt(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(Session::interrupt(self))
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, Result<(), Error>> {
        Box::pin(Session::close(*self))
    }
}

/// The configured runtimes by name.
pub type Runtimes = std::collections::HashMap<String, Arc<dyn DynRuntime>>;

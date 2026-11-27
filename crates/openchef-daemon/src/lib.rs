//! The long-running OpenChef process (`docs/architecture.md` §5, §12.3), ported from Go
//! `internal/daemon`: it holds the single-instance lock, serves the JSON-RPC API on a
//! local socket and owns every agent process. CLIs and the TUI are clients; they come
//! and go without stopping it.

mod app;
mod approvals;
mod approver;
mod env;
mod lock;
mod notify;
mod policy;
pub mod quota;
mod router;
mod server;
mod spawn;
mod tasks;

use std::path::PathBuf;

pub use app::{agents, daemon_options};
pub use approvals::{approval_methods, service_methods};
pub use lock::Lock;
pub use server::{Conn, Init, Method, OnStop, Options, STOP_TIMEOUT, Services, run};
pub use spawn::{Detached, start_detached};
pub use tasks::{MAX_REPLAY, task_methods};

/// Why the daemon could not start, serve or stop.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Another daemon holds the single-instance lock.
    #[error("daemon: another OpenChef daemon is running")]
    Running,
    /// The socket path does not fit a Unix socket address.
    #[error("daemon: socket path {} is {len} bytes; the limit is {max} (set OPENCHEF_HOME to a shorter directory)", path.display())]
    SocketPathTooLong {
        /// The path.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// Building the daemon's services failed.
    #[error("daemon: {0}")]
    Init(String),
    /// Stopping the sessions failed or took too long.
    #[error("daemon: stop: {0}")]
    Stop(String),
    /// A notification could not be sent.
    #[error("daemon: notify connection {id}: {reason}")]
    Notify {
        /// The connection.
        id: u64,
        /// Why.
        reason: String,
    },
    /// A background daemon did not come up.
    #[error("daemon: {0}")]
    StartFailed(String),
    /// Talking to the daemon failed.
    #[error(transparent)]
    Api(#[from] openchef_api::Error),
    /// The daemon socket is not available on this platform yet (Windows: R3.3).
    #[error("daemon: the daemon socket is not available on this platform yet")]
    Unsupported,
    /// A file system or socket operation failed.
    #[error("daemon: {context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

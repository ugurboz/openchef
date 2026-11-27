//! The crate's error type.

/// Why a runtime operation failed (as opposed to a turn that failed, which is an
/// event).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A turn is already running on this session.
    #[error("a turn is already in progress")]
    TurnInProgress,
    /// The session is closed, or its process died.
    #[error("the session is closed")]
    SessionClosed,
    /// `on_spawn` refused a new process; it was killed.
    #[error("process {pid} rejected by on_spawn: {reason}")]
    SpawnRejected {
        /// The process.
        pid: u32,
        /// Why.
        reason: String,
    },
    /// The CLI sent something the adapter does not understand where it must.
    #[error("{runtime}: protocol error: {reason} (line: {})", truncate(line))]
    Protocol {
        /// The runtime.
        runtime: &'static str,
        /// What was wrong.
        reason: String,
        /// The offending line.
        line: String,
    },
    /// The CLI could not be started or inspected.
    #[error("{runtime}: {message}")]
    Start {
        /// The runtime.
        runtime: &'static str,
        /// What went wrong.
        message: String,
    },
    /// Process supervision failed.
    #[error(transparent)]
    Procs(#[from] openchef_procs::Error),
    /// JSON-RPC transport failed.
    #[error(transparent)]
    Rpc(#[from] crate::linerpc::Error),
    /// Writing the raw log failed.
    #[error("raw log: {0}")]
    RawLog(#[from] std::io::Error),
}

fn truncate(line: &str) -> String {
    const MAX: usize = 200;
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    }
}

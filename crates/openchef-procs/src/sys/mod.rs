//! Platform implementations. Each module provides the same items:
//!
//! - `Platform`: the kill domain of one process (`new`, `configure`, `attach`,
//!   `signal_term`, `kill`, `release`);
//! - `SUPPORTS_TERM_SIGNAL`: whether a graceful termination signal exists;
//! - `wait_exit_no_reap(pid)`: block until the process exits, leaving it unreaped;
//! - `start_token(pid)`, `probe(pid)`, `kill_orphan(pid)`.

/// What the kernel reports about a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Probe {
    /// The start token of the process currently holding the PID.
    pub(crate) token: u64,
    /// False for zombies and processes already inside `exit`.
    pub(crate) running: bool,
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

// Windows support is task card R0.4; until then it reports `Error::Unsupported`.
#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub(crate) use unsupported::*;

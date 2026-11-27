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

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(not(any(unix, windows)))]
mod unsupported;
#[cfg(not(any(unix, windows)))]
pub(crate) use unsupported::*;

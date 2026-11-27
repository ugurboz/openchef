//! Starts and stops agent processes so that none of them can outlive its owner
//! (`docs/architecture.md` §5).
//!
//! Guarantees:
//!
//! - Every process gets its own kill domain: a process group on Unix, a Job Object with
//!   `KILL_ON_JOB_CLOSE` on Windows. Grandchildren spawned by the agent belong to it.
//! - When the process exits for any reason, everything left in its kill domain is killed
//!   *before* the process is reaped, so leftovers (dev servers, watchers) cannot leak and a
//!   recycled PID can never be hit by mistake. Signals sent through a [`Process`] are
//!   ordered against reaping by a guard, so they cannot reach a recycled PID either.
//! - On Linux the kernel sends SIGTERM to the process if its supervisor thread dies with
//!   the daemon (`PR_SET_PDEATHSIG`); on Windows the kernel kills the whole job when the
//!   daemon's handle closes. On macOS the process sees EOF on its stdin pipe, and the
//!   daemon sweeps recorded identities at startup with [`kill_if_alive`].
//!
//! This crate talks to the operating system directly, so `unsafe` is allowed here
//! (D-030). Every `unsafe` block must carry a `// SAFETY:` comment.
#![allow(unsafe_code)]

mod error;
mod process;
mod sys;

pub use error::Error;
pub use process::{DEFAULT_GRACE, Exit, Identity, Process, Spec, Stderr, StdinWriter};

/// The start token of a running process: an opaque, platform-specific value derived from
/// the process start time that matches only this instance of `pid`.
pub fn start_token(pid: u32) -> Result<u64, Error> {
    sys::start_token(pid)
}

/// Whether the exact process instance `id` is still running (not a zombie).
pub fn alive(id: &Identity) -> Result<bool, Error> {
    if id.pid == 0 {
        return Ok(false);
    }
    Ok(sys::probe(id.pid)?.is_some_and(|p| p.running && p.token == id.start_token))
}

/// Force-kills the kill domain of `id` if that exact instance is still running. The daemon
/// uses it at startup to remove processes left behind by a crashed instance. Returns
/// whether anything was killed.
pub fn kill_if_alive(id: &Identity) -> Result<bool, Error> {
    if !alive(id)? {
        return Ok(false);
    }
    sys::kill_orphan(id.pid)?;
    Ok(true)
}

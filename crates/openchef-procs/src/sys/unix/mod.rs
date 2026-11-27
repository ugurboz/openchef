//! macOS and Linux: the kill domain is the process group led by the child.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};

use crate::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

pub(crate) use os::{probe, start_token, wait_exit_no_reap};

pub(crate) const SUPPORTS_TERM_SIGNAL: bool = true;

/// The process group id equals the child's pid, so nothing needs to be kept.
pub(crate) struct Platform;

// Every platform exposes the same methods; on Unix several need neither `self` nor a
// `Result`, but Windows (Job Objects) does.
#[allow(clippy::unused_self, clippy::unnecessary_wraps)]
impl Platform {
    pub(crate) fn new() -> Result<Self, Error> {
        Ok(Self)
    }

    /// Puts the child in its own process group and, on Linux, asks for a parent-death
    /// signal.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        let _ = cmd.process_group(0);
        os::set_parent_death_signal(cmd);
    }

    pub(crate) fn attach(&self, _child: &Child) -> Result<(), Error> {
        Ok(())
    }

    pub(crate) fn signal_term(&self, pid: u32) -> Result<(), Error> {
        signal_group(pid, libc::SIGTERM, "SIGTERM")
    }

    pub(crate) fn kill(&self, pid: u32) -> Result<(), Error> {
        signal_group(pid, libc::SIGKILL, "SIGKILL")
    }

    pub(crate) fn release(&self) {}
}

fn to_pid(pid: u32, signal: &'static str) -> Result<libc::pid_t, Error> {
    libc::pid_t::try_from(pid).map_err(|_| Error::Signal {
        pid,
        signal,
        source: io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"),
    })
}

/// Signals the whole process group led by `pid`. A missing group is not an error:
/// everything in it is already gone.
fn signal_group(pid: u32, sig: libc::c_int, name: &'static str) -> Result<(), Error> {
    let group = to_pid(pid, name)?;
    // SAFETY: kill(2) only sends a signal; a negative pid addresses the process group.
    // It reads and writes no memory of this process.
    if unsafe { libc::kill(-group, sig) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        // macOS reports EPERM instead of ESRCH when the only remaining member is the
        // exited-but-unreaped leader. That is "nothing left to signal", not a failure.
        Some(libc::EPERM)
            if matches!(
                probe(pid),
                Ok(None | Some(super::Probe { running: false, .. }))
            ) =>
        {
            Ok(())
        }
        _ => Err(Error::Signal {
            pid,
            signal: name,
            source: err,
        }),
    }
}

/// Kills the group of a process left behind by an earlier daemon, then the process
/// itself in case it left its group.
pub(crate) fn kill_orphan(pid: u32) -> Result<(), Error> {
    signal_group(pid, libc::SIGKILL, "SIGKILL")?;
    let target = to_pid(pid, "SIGKILL")?;
    // SAFETY: as in `signal_group`; a positive pid addresses one process.
    if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(Error::Signal {
            pid,
            signal: "SIGKILL",
            source: err,
        })
    }
}

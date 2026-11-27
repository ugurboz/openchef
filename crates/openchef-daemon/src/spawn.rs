//! Starting the daemon in the background.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use openchef_api::{Client, InitializeResult};

use crate::Error;

/// A daemon process to start in the background.
#[derive(Debug, Clone)]
pub struct Detached {
    /// The executable (this program).
    pub exe: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Its environment; `None` keeps this process's.
    pub env: Option<Vec<(OsString, OsString)>>,
    /// Receives the daemon's output (appended).
    pub log_path: PathBuf,
    /// Where the daemon will answer.
    pub socket: PathBuf,
    /// Bounds the wait for the daemon to answer.
    pub timeout: Duration,
}

/// Puts the child outside this terminal's job control, so it outlives the terminal.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // A new process group: Ctrl-C and the terminal's hangup reach the foreground group,
    // not the daemon.
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

/// Launches `d` as a background daemon with stdin closed and output appended to its
/// log, then waits until it answers on its socket, exits, or the timeout passes.
///
/// # Errors
///
/// When it cannot start, exits during start, or does not answer in time.
pub async fn start_detached(d: &Detached) -> Result<InitializeResult, Error> {
    if let Some(dir) = d.log_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io("create log directory", e))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&d.log_path)
        .map_err(|e| Error::io("open daemon log", e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| Error::io("open daemon log", e))?;
    let mut cmd = Command::new(&d.exe);
    cmd.args(&d.args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    if let Some(env) = &d.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    detach(&mut cmd);
    // The daemon outlives this process: std's Child does not kill on drop.
    let mut child = cmd.spawn().map_err(|e| Error::io("start daemon", e))?;
    let deadline = Instant::now() + d.timeout;
    loop {
        match Client::dial(&d.socket, "openchef-start").await {
            Ok(c) => {
                let info = c.daemon.clone();
                c.close();
                return Ok(info);
            }
            Err(openchef_api::Error::NotRunning) => {}
            Err(e) => return Err(Error::Api(e)),
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::StartFailed(format!(
                "exited during start ({status}); see {}",
                d.log_path.display()
            )));
        }
        if Instant::now() > deadline {
            return Err(Error::StartFailed(format!(
                "not answering after {:?}; see {}",
                d.timeout,
                d.log_path.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

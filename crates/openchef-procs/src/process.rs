//! A supervised child process.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, PoisonError, RwLock, mpsc as std_mpsc};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{Error, sys};

/// The grace period [`Process::close`] gives each stage before escalating.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the exit after a forced kill.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Identifies one process instance across daemon restarts. PIDs are recycled, so a PID
/// alone is never trusted: `start_token` only matches the same instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    /// The process id.
    pub pid: u32,
    /// The platform-specific start token (see [`crate::start_token`]).
    #[serde(rename = "startToken")]
    pub start_token: u64,
}

/// Where the child's stderr goes.
#[derive(Debug, Default)]
pub enum Stderr {
    /// Discarded.
    #[default]
    Discard,
    /// Shared with the parent (tests and debugging only).
    Inherit,
    /// Appended to this file (e.g. the session's `stderr.log`).
    File(File),
    /// A pipe the caller reads with [`Process::take_stderr`].
    Piped,
}

/// Describes a process to start.
#[derive(Debug, Default)]
pub struct Spec {
    /// The program to run.
    pub program: PathBuf,
    /// Arguments after the program name.
    pub args: Vec<OsString>,
    /// The working directory; the daemon's when `None`.
    pub dir: Option<PathBuf>,
    /// The complete environment of the child. It is never inherited implicitly: callers
    /// build a filtered environment (architecture §13).
    pub env: Vec<(OsString, OsString)>,
    /// Where stderr goes.
    pub stderr: Stderr,
}

/// Rewrites a process's spec before it starts, e.g. to start it inside OpenChef's OS
/// sandbox (§21.3). An error refuses to start the process: nothing runs unconfined.
pub type Confine = std::sync::Arc<dyn Fn(Spec) -> Result<Spec, String> + Send + Sync>;

/// How a process ended.
#[derive(Debug, Clone)]
pub enum Exit {
    /// The process was reaped with this status.
    Status(ExitStatus),
    /// Waiting for the process failed; its fate is unknown.
    WaitFailed(String),
}

impl Exit {
    /// Whether the process exited with status 0.
    #[must_use]
    pub fn success(&self) -> bool {
        match self {
            Self::Status(status) => status.success(),
            Self::WaitFailed(_) => false,
        }
    }
}

/// State shared by the [`Process`] handle and its supervisor thread.
///
/// `reaped` orders signals against reaping: a signal is only sent while the read guard
/// shows the leader unreaped, and the supervisor takes the write guard to reap. The PID
/// (and process group id) therefore cannot be recycled while a signal is in flight. The
/// lock guards one `bool` and is never held across an `.await` (D-030).
struct Shared {
    pid: u32,
    reaped: RwLock<bool>,
    platform: sys::Platform,
}

impl Shared {
    /// Runs `f` against the kill domain unless the leader has been reaped, in which case
    /// there is nothing left to signal.
    fn signal(
        &self,
        f: impl FnOnce(&sys::Platform, u32) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let reaped = self.reaped.read().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        f(&self.platform, self.pid)
    }
}

enum StdinMsg {
    Write(Vec<u8>, oneshot::Sender<io::Result<()>>),
    Close,
}

/// Writes to a child's stdin from async code. Cheap to clone; every clone writes to the
/// same pipe, in the order the writes are awaited.
#[derive(Debug, Clone)]
pub struct StdinWriter {
    pid: u32,
    tx: mpsc::UnboundedSender<StdinMsg>,
}

impl std::fmt::Debug for StdinMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write(data, _) => write!(f, "Write({} bytes)", data.len()),
            Self::Close => f.write_str("Close"),
        }
    }
}

impl StdinWriter {
    /// Writes and flushes `data`. Fails once stdin is closed.
    pub async fn write(&self, data: Vec<u8>) -> Result<(), Error> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(StdinMsg::Write(data, reply))
            .map_err(|_| Error::StdinClosed { pid: self.pid })?;
        done.await
            .map_err(|_| Error::StdinClosed { pid: self.pid })?
            .map_err(|source| Error::StdinWrite {
                pid: self.pid,
                source,
            })
    }

    /// Queues `data` without waiting for the write; writes keep their order. Fails only
    /// when stdin is already closed. Use it where waiting is not possible (an actor's
    /// message loop); a failed write shows up as the process going away.
    pub fn queue(&self, data: Vec<u8>) -> Result<(), Error> {
        let (reply, _ignored) = oneshot::channel();
        self.tx
            .send(StdinMsg::Write(data, reply))
            .map_err(|_| Error::StdinClosed { pid: self.pid })
    }

    /// Closes stdin after the writes already queued; the child sees EOF.
    pub fn close(&self) {
        // A send error means the writer thread already ended and stdin is closed.
        best_effort(self.tx.send(StdinMsg::Close));
    }
}

/// A running, supervised child process.
///
/// One supervisor thread per process spawns the child, waits for it to exit, cleans up
/// its kill domain and reaps it. Dropping the handle kills the kill domain if the process
/// is still running. Async methods need a Tokio runtime with the time driver enabled.
pub struct Process {
    id: Identity,
    shared: Arc<Shared>,
    exit: watch::Receiver<Option<Exit>>,
    stdin: StdinWriter,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("id", &self.id)
            .field("exit", &*self.exit.borrow())
            .finish_non_exhaustive()
    }
}

struct Started {
    shared: Arc<Shared>,
    token: u64,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
}

impl Process {
    /// Starts `spec` under supervision. This blocks briefly while the child is spawned
    /// (fork/exec or `CreateProcess`); it does not wait for the child to do anything.
    pub fn start(spec: Spec) -> Result<Self, Error> {
        if spec.program.as_os_str().is_empty() {
            return Err(Error::EmptyProgram);
        }
        let program = spec.program.clone();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (exit_tx, exit_rx) = watch::channel(None);
        // The supervisor thread spawns the child itself: on Linux the parent-death
        // signal is tied to the spawning thread, and this thread lives exactly as long
        // as the child.
        thread::Builder::new()
            .name("openchef-procs-supervisor".to_owned())
            .spawn(move || supervise(spec, &ready_tx, &exit_tx))
            .map_err(|e| Error::Supervisor {
                program: program.clone(),
                reason: e.to_string(),
            })?;
        let started = ready_rx.recv().map_err(|_| Error::Supervisor {
            program: program.clone(),
            reason: "ended before reporting the start".to_owned(),
        })??;

        let pid = started.shared.pid;
        let stdin = spawn_stdin_writer(pid, started.stdin, &program)?;
        Ok(Self {
            id: Identity {
                pid,
                start_token: started.token,
            },
            shared: started.shared,
            exit: exit_rx,
            stdin,
            stdout: Some(started.stdout),
            stderr: started.stderr,
        })
    }

    /// The persistable identity of the process.
    #[must_use]
    pub fn identity(&self) -> Identity {
        self.id
    }

    /// A writer for the child's stdin.
    #[must_use]
    pub fn stdin(&self) -> StdinWriter {
        self.stdin.clone()
    }

    /// Takes the child's stdout. The caller drains it (it is a blocking pipe; read it on
    /// a dedicated thread) and drops it when done.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Takes the child's stderr when the spec asked for [`Stderr::Piped`]. Like stdout,
    /// it is a blocking pipe the caller drains on its own thread.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// How the process ended, if it has been reaped.
    #[must_use]
    pub fn exit(&self) -> Option<Exit> {
        self.exit.borrow().clone()
    }

    /// Waits until the process has exited, its kill domain was cleaned up and it was
    /// reaped.
    pub async fn wait(&self) -> Exit {
        let mut rx = self.exit.clone();
        match rx.wait_for(Option::is_some).await {
            Ok(exit) => exit
                .clone()
                .unwrap_or_else(|| Exit::WaitFailed("no exit status".to_owned())),
            Err(_) => Exit::WaitFailed("supervisor ended without an exit status".to_owned()),
        }
    }

    /// Stops the process and everything in its kill domain. It escalates in three stages,
    /// each bounded by `grace`: close stdin (agent CLIs exit on EOF), send a termination
    /// signal to the group (Unix only), force-kill. A cancelled `cancel` skips the
    /// remaining grace periods. Returns once the process is reaped.
    pub async fn terminate(
        &self,
        grace: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        if self.exit().is_some() {
            return Ok(());
        }
        self.stdin.close();
        if self.exited_within(grace, cancel).await {
            return Ok(());
        }
        if sys::SUPPORTS_TERM_SIGNAL {
            self.shared.signal(sys::Platform::signal_term)?;
            if self.exited_within(grace, cancel).await {
                return Ok(());
            }
        }
        self.shared.signal(sys::Platform::kill)?;
        if self
            .exited_within(KILL_TIMEOUT, &CancellationToken::new())
            .await
        {
            Ok(())
        } else {
            Err(Error::KillTimeout { pid: self.id.pid })
        }
    }

    /// Terminates the process with [`DEFAULT_GRACE`] and drops stdout if it was not
    /// taken. Idempotent.
    pub async fn close(&mut self) -> Result<(), Error> {
        let result = self
            .terminate(DEFAULT_GRACE, &CancellationToken::new())
            .await;
        drop(self.stdout.take());
        result
    }

    /// Whether the process was reaped within `limit`. A cancelled token ends the wait
    /// early so the caller escalates immediately.
    async fn exited_within(&self, limit: Duration, cancel: &CancellationToken) -> bool {
        let mut rx = self.exit.clone();
        tokio::select! {
            // An error means the supervisor is gone, so nothing is left to wait for.
            _ = rx.wait_for(Option::is_some) => true,
            () = tokio::time::sleep(limit) => false,
            () = cancel.cancelled() => self.exit().is_some(),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stdin.close();
        if self.exit().is_none() {
            // Best effort: drop cannot report errors, and the supervisor still cleans up
            // the kill domain when the leader exits.
            best_effort(self.shared.signal(sys::Platform::kill));
        }
    }
}

/// The supervisor thread: spawn, report, wait without reaping, clean up, reap.
fn supervise(
    spec: Spec,
    ready: &std_mpsc::SyncSender<Result<Started, Error>>,
    exit: &watch::Sender<Option<Exit>>,
) {
    let program = spec.program.clone();
    let platform = match sys::Platform::new() {
        Ok(platform) => platform,
        Err(e) => {
            report(ready, Err(e));
            return;
        }
    };
    let mut cmd = Command::new(&spec.program);
    let _ = cmd
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(match spec.stderr {
            Stderr::Discard => Stdio::null(),
            Stderr::Inherit => Stdio::inherit(),
            Stderr::File(file) => Stdio::from(file),
            Stderr::Piped => Stdio::piped(),
        });
    if let Some(dir) = &spec.dir {
        let _ = cmd.current_dir(dir);
    }
    platform.configure(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(source) => {
            report(ready, Err(Error::Spawn { program, source }));
            return;
        }
    };
    let pid = child.id();
    let started = platform
        .attach(&child)
        .and_then(|()| crate::sys::start_token(pid))
        .and_then(|token| match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => Ok((token, stdin, stdout)),
            _ => Err(Error::Supervisor {
                program: program.clone(),
                reason: "missing stdio pipes".to_owned(),
            }),
        });
    let shared = Arc::new(Shared {
        pid,
        reaped: RwLock::new(false),
        platform,
    });
    let (token, stdin, stdout) = match started {
        Ok(parts) => parts,
        Err(e) => {
            // Undo the start: kill the domain, reap, report the original error.
            if shared.platform.kill(pid).is_err() {
                best_effort(child.kill());
            }
            best_effort(child.wait());
            shared.platform.release();
            report(ready, Err(e));
            return;
        }
    };
    let stderr = child.stderr.take();
    report(
        ready,
        Ok(Started {
            shared: Arc::clone(&shared),
            token,
            stdin,
            stdout,
            stderr,
        }),
    );

    let status = if sys::wait_exit_no_reap(pid) {
        // The leader is a zombie, so its PID and group id are still reserved: kill
        // whatever it left behind, then reap while holding the guard.
        best_effort(shared.platform.kill(pid));
        let mut reaped = shared
            .reaped
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *reaped = true;
        child.wait()
    } else {
        // Waiting without reaping is unavailable; reap first, then mark it, without
        // holding the guard while blocked.
        let status = child.wait();
        *shared
            .reaped
            .write()
            .unwrap_or_else(PoisonError::into_inner) = true;
        status
    };
    shared.platform.release();
    let _ = exit.send_replace(Some(match status {
        Ok(status) => Exit::Status(status),
        Err(e) => Exit::WaitFailed(e.to_string()),
    }));
}

fn report(ready: &std_mpsc::SyncSender<Result<Started, Error>>, result: Result<Started, Error>) {
    // The receiver only disappears if `Process::start` itself is gone; nothing to do then.
    best_effort(ready.send(result));
}

/// Starts the thread that owns the child's stdin and performs blocking writes for
/// [`StdinWriter`]s. Dropping every writer, or `close`, closes the pipe.
fn spawn_stdin_writer(
    pid: u32,
    mut stdin: ChildStdin,
    program: &std::path::Path,
) -> Result<StdinWriter, Error> {
    let (tx, mut rx) = mpsc::unbounded_channel::<StdinMsg>();
    thread::Builder::new()
        .name("openchef-procs-stdin".to_owned())
        .spawn(move || {
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    StdinMsg::Write(data, reply) => {
                        let result = stdin.write_all(&data).and_then(|()| stdin.flush());
                        // The writer may have stopped waiting; the result is then unused.
                        best_effort(reply.send(result));
                    }
                    StdinMsg::Close => break,
                }
            }
        })
        .map_err(|e| Error::Supervisor {
            program: program.to_path_buf(),
            reason: e.to_string(),
        })?;
    Ok(StdinWriter { pid, tx })
}

/// Ignores the outcome of a best-effort step whose failure changes nothing; every call
/// site says why in a comment.
fn best_effort<T, E>(result: Result<T, E>) {
    drop(result);
}

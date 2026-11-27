//! Runs a step's verification command in its workspace (`docs/architecture.md` §7.2),
//! ported from Go `internal/verify`: through the platform shell, in its own kill domain
//! (`openchef-procs`), with a minimal environment, a timeout that stops the whole
//! process tree, and output bounded to its head and tail before an agent sees it.

mod env;
mod output;

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub use env::minimal_env;
use openchef_procs::{Exit, Process, Stderr};
use output::HeadTail;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// How long a run may take when the spec does not say.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);
/// Output bytes kept for the agent when the spec does not say: 8 KiB head + 8 KiB tail.
pub const DEFAULT_MAX_OUTPUT: usize = 16 << 10;
/// How long the command gets after the termination signal before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Bounds waiting for output once the tree is gone; a process that escaped the kill
/// domain could otherwise hold a pipe open forever.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// One verification run.
#[derive(Debug, Clone)]
pub struct Spec {
    /// A shell command line, e.g. `cargo test`.
    pub command: String,
    /// The workspace to run in.
    pub dir: PathBuf,
    /// The complete environment (see [`minimal_env`]); it is never inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Bounds the run; [`DEFAULT_TIMEOUT`] when `None`.
    pub timeout: Option<Duration>,
    /// Output bytes kept; [`DEFAULT_MAX_OUTPUT`] when `None`.
    pub max_output: Option<usize>,
}

/// The outcome of a run that started. A failing command is an outcome with `passed`
/// false, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Exit status 0 within the timeout.
    pub passed: bool,
    /// The exit status; `None` when stopped by a signal or the timeout.
    pub exit_code: Option<i32>,
    /// The timeout stopped it.
    pub timed_out: bool,
    /// stdout and stderr in arrival order (approximate across the two pipes), bounded.
    pub output: String,
    /// Output was dropped from the middle.
    pub truncated: bool,
    /// Wall time.
    pub duration: Duration,
}

impl Outcome {
    /// One line for logs and the live view.
    #[must_use]
    pub fn summary(&self) -> String {
        let took =
            Duration::from_millis(u64::try_from(self.duration.as_millis()).unwrap_or(u64::MAX));
        match (self.passed, self.timed_out, self.exit_code) {
            (true, _, _) => format!("passed in {took:?}"),
            (false, true, _) => format!("timed out after {took:?}"),
            (false, false, Some(code)) => format!("failed with exit code {code} in {took:?}"),
            (false, false, None) => format!("killed by a signal after {took:?}"),
        }
    }
}

/// Why a run did not produce an [`Outcome`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The spec cannot be run.
    #[error("verify: invalid spec: {0}")]
    InvalidSpec(String),
    /// The shell could not be started.
    #[error("verify: start {command:?}: {source}")]
    Start {
        /// The command line.
        command: String,
        /// Why.
        #[source]
        source: openchef_procs::Error,
    },
    /// The process tree could not be stopped.
    #[error("verify: stop {command:?}: {source}")]
    Stop {
        /// The command line.
        command: String,
        /// Why.
        #[source]
        source: openchef_procs::Error,
    },
    /// The caller cancelled; the tree was stopped first.
    #[error("verify: {command:?} cancelled")]
    Cancelled {
        /// The command line.
        command: String,
    },
}

/// The shell and arguments that run `command`: `/bin/sh -c` on Unix, `cmd.exe /d /s /c`
/// on Windows (from `ComSpec` in `env`, else the system root).
fn shell_command(command: &str, env: &[(OsString, OsString)]) -> (OsString, Vec<OsString>) {
    if !cfg!(windows) {
        return ("/bin/sh".into(), vec!["-c".into(), command.into()]);
    }
    let lookup = |key: &str| {
        env.iter()
            .rev()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    let shell = lookup("ComSpec").unwrap_or_else(|| {
        let mut root = lookup("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        root.push(r"\System32\cmd.exe");
        root
    });
    (
        shell,
        ["/d", "/s", "/c", command].map(OsString::from).to_vec(),
    )
}

/// Copies one pipe into the shared buffer on its own thread; the receiver fires at EOF.
fn drain(
    pipe: Option<impl Read + Send + 'static>,
    out: &Arc<Mutex<HeadTail>>,
) -> oneshot::Receiver<()> {
    let (done, rx) = oneshot::channel();
    let Some(mut pipe) = pipe else {
        let _sent = done.send(()); // the receiver may have stopped waiting
        return rx;
    };
    let out = Arc::clone(out);
    let spawned = thread::Builder::new()
        .name("openchef-verify-output".to_owned())
        .spawn(move || {
            let mut buf = [0u8; 16 << 10];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .write(&buf[..n]);
            }
            let _sent = done.send(()); // the receiver may have stopped waiting
        });
    // Without the thread the output is lost; the receiver then reports at once.
    drop(spawned);
    rx
}

/// Runs `spec`. An error means the command could not be run at all, the tree could not
/// be stopped, or `cancel` fired (the tree is stopped before this returns).
///
/// # Errors
///
/// See [`Error`].
pub async fn run(spec: &Spec, cancel: &CancellationToken) -> Result<Outcome, Error> {
    if spec.command.trim().is_empty() {
        return Err(Error::InvalidSpec("empty command".into()));
    }
    if spec.dir.as_os_str().is_empty() {
        return Err(Error::InvalidSpec("empty directory".into()));
    }
    if !spec.dir.is_dir() {
        return Err(Error::InvalidSpec(format!(
            "directory {} does not exist",
            spec.dir.display()
        )));
    }
    let timeout = spec.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let limit = spec.max_output.unwrap_or(DEFAULT_MAX_OUTPUT);
    let (program, args) = shell_command(&spec.command, &spec.env);
    let process_spec = openchef_procs::Spec {
        program: program.into(),
        args,
        dir: Some(spec.dir.clone()),
        env: spec.env.clone(),
        stderr: Stderr::Piped,
    };
    let start = Instant::now();
    let started = tokio::task::spawn_blocking(move || Process::start(process_spec)).await;
    let mut process = match started {
        Ok(Ok(p)) => p,
        Ok(Err(source)) => {
            return Err(Error::Start {
                command: spec.command.clone(),
                source,
            });
        }
        Err(e) => return Err(Error::InvalidSpec(format!("start task: {e}"))),
    };
    // Verification takes no input; EOF now also keeps stdin-reading tools from hanging.
    process.stdin().close();
    let out = Arc::new(Mutex::new(HeadTail::new(limit)));
    let stdout_done = drain(process.take_stdout(), &out);
    let stderr_done = drain(process.take_stderr(), &out);

    let (mut timed_out, mut cancelled) = (false, false);
    let exit = tokio::select! {
        exit = process.wait() => Some(exit),
        () = tokio::time::sleep(timeout) => { timed_out = true; None }
        () = cancel.cancelled() => { cancelled = true; None }
    };
    if (timed_out || cancelled)
        && let Err(source) = process
            .terminate(STOP_GRACE, &CancellationToken::new())
            .await
    {
        return Err(Error::Stop {
            command: spec.command.clone(),
            source,
        });
    }
    let drained = async {
        let _stdout = stdout_done.await;
        let _stderr = stderr_done.await;
    };
    // An escaped process may hold a pipe; its reader thread then ends on its own.
    let _complete = tokio::time::timeout(DRAIN_WAIT, drained).await;
    let (output, truncated) = out.lock().unwrap_or_else(PoisonError::into_inner).result();
    if cancelled {
        return Err(Error::Cancelled {
            command: spec.command.clone(),
        });
    }
    let exit_code = match exit {
        Some(Exit::Status(status)) => status.code(),
        Some(Exit::WaitFailed(_)) | None => None,
    };
    Ok(Outcome {
        passed: !timed_out && exit_code == Some(0),
        exit_code: if timed_out { None } else { exit_code },
        timed_out,
        output,
        truncated,
        duration: start.elapsed(),
    })
}

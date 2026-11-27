//! Runtime detection for `openchef doctor` and `openchef init`: which agent CLIs are
//! installed, their versions and whether they are logged in. Only read-only status
//! commands are run.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::Runtime;

openchef_core::string_enum! {
    /// Whether a CLI reports a login.
    pub enum Login: "login state" {
        /// Logged in.
        LoggedIn => "logged_in",
        /// Not logged in.
        LoggedOut => "logged_out",
        /// The CLI cannot tell (or its answer was not understood).
        Unknown => "unknown",
    }
}

/// What doctor found for one agent CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeStatus {
    /// The runtime.
    pub runtime: Runtime,
    /// The executable name looked up on `PATH`.
    pub binary: &'static str,
    /// Whether it was found.
    pub found: bool,
    /// Where it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The version it reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Login state.
    pub login: Login,
    /// The CLI's own words about the login, or how to log in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_detail: Option<String>,
    /// Why it cannot be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl RuntimeStatus {
    /// Whether OpenChef can drive this runtime. An unknown login does not block it.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.found && self.problem.is_none() && self.login != Login::LoggedOut
    }
}

/// The result of one command. A non-zero exit is a result, not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// Exit code (−1 when killed by a signal).
    pub code: i32,
}

/// Runs a status command. Errors are failures to run it at all (missing binary,
/// timeout).
pub trait Runner: Send + Sync {
    /// Runs `program` with `args`, giving up after `timeout`.
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, String>;
}

/// Finds an executable on `PATH`.
type LookPath = Box<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// Finds and inspects agent CLIs.
pub struct Detector {
    look_path: LookPath,
    runner: Box<dyn Runner>,
    timeout: Duration,
}

impl std::fmt::Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Detector")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Detector {
    /// A detector for the real `PATH` and processes.
    #[must_use]
    pub fn system() -> Self {
        Self::new(look_path, SystemRunner, Duration::from_secs(15))
    }

    /// A detector with explicit lookup and runner (tests).
    pub fn new(
        look_path: impl Fn(&str) -> Option<PathBuf> + Send + Sync + 'static,
        runner: impl Runner + 'static,
        timeout: Duration,
    ) -> Self {
        Self {
            look_path: Box::new(look_path),
            runner: Box::new(runner),
            timeout,
        }
    }

    /// Inspects every supported runtime. It never fails as a whole; problems are
    /// reported per runtime.
    #[must_use]
    pub fn detect(&self) -> Vec<RuntimeStatus> {
        Runtime::ALL.iter().map(|&rt| self.inspect(rt)).collect()
    }

    fn inspect(&self, runtime: Runtime) -> RuntimeStatus {
        let binary = runtime.as_str();
        let mut st = RuntimeStatus {
            runtime,
            binary,
            found: false,
            path: None,
            version: None,
            login: Login::Unknown,
            login_detail: None,
            problem: None,
        };
        let Some(path) = (self.look_path)(binary) else {
            st.problem = Some("not installed (not found on PATH)".to_owned());
            return st;
        };
        st.found = true;
        match self.runner.run(&path, &["--version"], self.timeout) {
            Err(e) => st.problem = Some(format!("version check failed: {e}")),
            Ok(out) if out.code != 0 => {
                st.problem = Some(format!("version check exited with code {}", out.code));
            }
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                st.version = match runtime {
                    Runtime::Codex => text.split_whitespace().last(),
                    Runtime::Claude | Runtime::Agy => text.split_whitespace().next(),
                }
                .map(str::to_owned);
                let (login, detail) = self.login(runtime, &path);
                st.login = login;
                st.login_detail = detail;
            }
        }
        st.path = Some(path);
        st
    }

    fn login(&self, runtime: Runtime, path: &Path) -> (Login, Option<String>) {
        match runtime {
            Runtime::Claude => match self.runner.run(path, &["auth", "status"], self.timeout) {
                Err(e) => (Login::Unknown, Some(format!("claude auth status: {e}"))),
                Ok(out) => claude_login(&out),
            },
            Runtime::Codex => match self.runner.run(path, &["login", "status"], self.timeout) {
                Err(e) => (Login::Unknown, Some(format!("codex login status: {e}"))),
                Ok(out) if out.code != 0 => {
                    (Login::LoggedOut, Some("run `codex login`".to_owned()))
                }
                Ok(out) => {
                    // codex prints its status ("Logged in using ChatGPT") on stderr.
                    let text = String::from_utf8_lossy(
                        if out.stdout.iter().all(u8::is_ascii_whitespace) {
                            &out.stderr
                        } else {
                            &out.stdout
                        },
                    )
                    .trim()
                    .to_owned();
                    (Login::LoggedIn, Some(text).filter(|t| !t.is_empty()))
                }
            },
            // agy exposes no login status command (checked on agy 1.5.16); report it as
            // unknown rather than guess.
            Runtime::Agy => (
                Login::Unknown,
                Some("agy has no login status command".to_owned()),
            ),
        }
    }
}

fn claude_login(out: &CommandOutput) -> (Login, Option<String>) {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Status {
        logged_in: Option<bool>,
        #[serde(default)]
        auth_method: String,
    }
    match serde_json::from_slice::<Status>(&out.stdout)
        .ok()
        .and_then(|s| s.logged_in.map(|l| (l, s.auth_method)))
    {
        Some((true, method)) => (Login::LoggedIn, Some(method).filter(|m| !m.is_empty())),
        Some((false, _)) => (Login::LoggedOut, Some("run `claude auth login`".to_owned())),
        None => (
            Login::Unknown,
            Some(format!(
                "claude auth status: unexpected output (exit {})",
                out.code
            )),
        ),
    }
}

/// Finds `binary` on `PATH` (with `PATHEXT` on Windows).
fn look_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| dir.join(format!("{binary}{ext}")))
        })
        .find(|candidate| is_executable(candidate))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Runs commands as real child processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("run {}: {e}", program.display()))?;
        // Drain both pipes on their own threads so a chatty command cannot block on a
        // full pipe while we wait for it.
        let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut pipe) = pipe {
                    // A read error leaves what was read so far, which is all we can report.
                    drop(pipe.read_to_end(&mut buf));
                }
                buf
            })
        };
        let stdout = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let stderr = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    // The command is abandoned; failing to kill or reap it changes nothing.
                    drop(child.kill());
                    drop(child.wait());
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(format!("wait for {}: {e}", program.display())),
            }
        };
        Ok(CommandOutput {
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
            code: status.code().unwrap_or(-1),
        })
    }
}

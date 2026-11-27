//! The `openchef` command line. It contains no business logic: commands parse flags,
//! call into the library crates and render results. [`run`] is the whole program, with
//! its environment passed in, so tests drive it without a real `PATH` or home.

mod cli;
mod commands;
mod output;
mod render;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as _;
use openchef_config::{Detector, Paths};

pub use commands::version::BuildInfo;

/// Starts a background daemon when a command needs one.
pub type StartDaemon = Arc<dyn Fn(&Paths) -> anyhow::Result<()> + Send + Sync>;

/// Checks the OS sandbox: what enforces it, or why it cannot apply.
pub type SandboxProbe = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// The input and output an MCP server speaks over (`openchef mcp ask-user`).
pub type McpStdio =
    Arc<dyn Fn() -> (Box<dyn std::io::Read + Send>, Box<dyn Write + Send>) + Send + Sync>;

/// The environment commands depend on, replaceable in tests.
pub struct Deps {
    /// Finds and inspects agent CLIs.
    pub detector: Detector,
    /// OpenChef home override; `None` resolves `OPENCHEF_HOME` or `~/.openchef`.
    pub home: Option<PathBuf>,
    /// Whether questions may be asked on the terminal.
    pub interactive: bool,
    /// Starts the daemon for `run`; `None` launches this executable's `daemon run`
    /// detached.
    pub start_daemon: Option<StartDaemon>,
    /// The working directory `run` acts on; `None` is the process's.
    pub cwd: Option<PathBuf>,
    /// Where `mcp ask-user` reads and writes; `None` is the process's stdin/stdout.
    pub mcp_stdio: Option<McpStdio>,
    /// `doctor`'s sandbox check; `None` is the platform's real one.
    pub sandbox: Option<SandboxProbe>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("detector", &self.detector)
            .field("home", &self.home)
            .field("interactive", &self.interactive)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Deps {
    /// The real environment.
    #[must_use]
    pub fn system() -> Self {
        use std::io::IsTerminal as _;
        Self {
            detector: Detector::system(),
            home: None,
            interactive: std::io::stdin().is_terminal(),
            start_daemon: None,
            cwd: None,
            mcp_stdio: None,
            sandbox: None,
        }
    }

    fn paths(&self, flag: Option<&PathBuf>) -> anyhow::Result<Paths> {
        Ok(match flag.or(self.home.as_ref()) {
            Some(home) => Paths::in_home(home)?,
            None => Paths::resolve()?,
        })
    }
}

/// Exit codes.
pub mod exit {
    /// Success.
    pub const OK: i32 = 0;
    /// The command failed, or found problems (e.g. `doctor`).
    pub const FAILURE: i32 = 1;
    /// Bad usage, or a command that is not built yet.
    pub const USAGE: i32 = 2;
    /// `daemon status`: no daemon is running (as `systemctl status` for an inactive
    /// unit).
    pub const NOT_RUNNING: i32 = 3;
    /// The sandbox helper could not confine the command, which did not run (as `env`'s
    /// 125 for its own failures).
    pub const SANDBOX: i32 = 125;
}

/// Runs the program with `args` (including the program name) and returns the exit
/// code. Normal output goes to `out`, errors to `err`.
pub fn run<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write, deps: &Deps) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // The Linux sandbox helper (`openchef __sandbox --policy … -- CMD`) confines this
    // process and execs; it never reaches the command parser.
    if args
        .get(1)
        .is_some_and(|a| a == openchef_sandbox::HELPER_COMMAND)
    {
        let failed = match openchef_sandbox::run_helper(&args[2..]) {
            Ok(never) => match never {},
            Err(e) => e,
        };
        drop(writeln!(err, "openchef: {failed}"));
        return exit::SANDBOX;
    }
    let parsed = match cli::Cli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            let code = if e.use_stderr() {
                exit::USAGE
            } else {
                exit::OK
            };
            let target: &mut dyn Write = if e.use_stderr() { err } else { out };
            // If the terminal is gone there is nobody left to tell.
            drop(write!(target, "{}", e.render()));
            return code;
        }
    };
    match commands::dispatch(parsed, out, err, deps) {
        Ok(code) => code,
        Err(e) => {
            drop(writeln!(err, "error: {e:#}"));
            exit::FAILURE
        }
    }
}

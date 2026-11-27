//! The command tree (`docs/architecture.md` §12.1).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Orchestrate AI coding agents across your subscriptions.
///
/// OpenChef plans work, hands each step to the best available coding agent (Claude
/// Code, Codex, Antigravity), verifies the result itself, and fails over to another
/// subscription when one runs out of quota.
#[derive(Debug, Parser)]
#[command(name = "openchef", version, disable_help_subcommand = true)]
pub(crate) struct Cli {
    /// The OpenChef home directory.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help = "OpenChef home directory (default: $OPENCHEF_HOME or ~/.openchef)"
    )]
    pub(crate) home: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Print version information.
    Version {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check installed agent CLIs, logins and configuration.
    ///
    /// Reports which agent CLIs (claude, codex, agy) are installed, their versions and
    /// login state, and whether the configuration is valid. Exits non-zero when no
    /// agent CLI is usable or the configuration is invalid.
    Doctor {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Detect installed agent CLIs and write the configuration.
    ///
    /// Writes one pool per subscription and agents and routing for the CLIs that are
    /// usable. Use --plan to record your subscription tier, e.g. --plan claude=max.
    Init(InitArgs),
    /// Start, stop or inspect the background daemon.
    ///
    /// The daemon runs every agent session. It starts automatically when a task needs
    /// it; these commands control it directly.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Run one step in a new worktree, verify it and commit it.
    ///
    /// Runs the prompt as a single step on a routed agent in a worktree of the current
    /// repository, then verifies it with --verify and commits the result on branch
    /// openchef/<task>. Output streams live; Ctrl-C detaches without cancelling (use
    /// `openchef cancel`).
    Run(RunArgs),
    /// List tasks, or show one task's steps and attempts.
    Status(StatusArgs),
    /// Show a task's events; -f follows them live.
    Logs(LogsArgs),
    /// Cancel a task and stop its agent.
    Cancel(TaskArgs),
    /// Retry a task that needs attention or waits for quota.
    Retry(TaskArgs),
    /// Not built yet.
    #[command(external_subcommand)]
    Pending(Vec<String>),
}

#[derive(Debug, clap::Args)]
pub(crate) struct InitArgs {
    /// Overwrite an existing configuration.
    #[arg(long)]
    pub(crate) force: bool,
    /// Print the configuration instead of writing it.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Subscription tier per runtime, e.g. claude=max (repeatable).
    #[arg(long = "plan", value_name = "RUNTIME=TIER")]
    pub(crate) plans: Vec<String>,
    /// Read your CLIs' logins to check plan usage (asked when omitted).
    #[arg(long, value_enum, value_name = "ON|OFF")]
    pub(crate) quota_probes: Option<OnOff>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Run the daemon in the foreground (what `daemon start` launches).
    #[command(hide = true)]
    Run,
    /// Start the daemon in the background.
    Start(JsonFlag),
    /// Stop the daemon; running tasks resume on next start.
    Stop(JsonFlag),
    /// Show whether the daemon is running (exit status 3 when it is not).
    Status(JsonFlag),
}

#[derive(Debug, clap::Args)]
pub(crate) struct JsonFlag {
    /// Print the result as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct RunArgs {
    /// What to do.
    #[arg(required = true, num_args = 1.., value_name = "PROMPT")]
    pub(crate) prompt: Vec<String>,
    /// Command that must pass in the workspace, e.g. "cargo test".
    #[arg(long, value_name = "COMMAND")]
    pub(crate) verify: Option<String>,
    /// Pin an agent from config.toml instead of routing.
    #[arg(long, value_name = "NAME")]
    pub(crate) agent: Option<String>,
    /// Permission mode for this task (manual, acceptEdits, plan, auto, dontAsk,
    /// bypassPermissions).
    #[arg(long, value_name = "MODE")]
    pub(crate) permission_mode: Option<String>,
    /// Print only the final result.
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// Return immediately; follow with `openchef logs <task> -f`.
    #[arg(long)]
    pub(crate) detach: bool,
    /// Print events as JSON lines.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct StatusArgs {
    /// A task id; without one, tasks are listed.
    pub(crate) task: Option<String>,
    /// Include finished tasks.
    #[arg(long)]
    pub(crate) all: bool,
    /// Show at most this many tasks.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: u32,
    /// Print as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct LogsArgs {
    /// The task id.
    pub(crate) task: String,
    /// Keep following new events.
    #[arg(short, long)]
    pub(crate) follow: bool,
    /// Print events as JSON lines.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct TaskArgs {
    /// The task id.
    pub(crate) task: String,
    /// Print the task as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OnOff {
    On,
    Off,
}

/// Commands of §12.1 that later milestones build, and the milestone that does.
pub(crate) const PENDING: &[(&str, &str)] = &[
    ("pipeline", "R5"),
    ("approve", "R4"),
    ("deny", "R4"),
    ("answer", "R4"),
    ("approvals", "R4"),
    ("merge", "R5"),
    ("plan", "R5"),
    ("deferred", "R7"),
    ("report", "R7"),
    ("attach", "R3"),
    ("quota", "R4"),
];

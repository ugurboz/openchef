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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OnOff {
    On,
    Off,
}

/// Commands of §12.1 that later milestones build, and the milestone that does.
pub(crate) const PENDING: &[(&str, &str)] = &[
    ("pipeline", "R5"),
    ("run", "R3"),
    ("status", "R3"),
    ("logs", "R3"),
    ("approve", "R4"),
    ("deny", "R4"),
    ("answer", "R4"),
    ("approvals", "R4"),
    ("retry", "R3"),
    ("cancel", "R3"),
    ("merge", "R5"),
    ("plan", "R5"),
    ("deferred", "R7"),
    ("report", "R7"),
    ("attach", "R3"),
    ("quota", "R4"),
    ("daemon", "R3"),
];

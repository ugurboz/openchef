//! Permission rules, layered settings, shell command parsing and the evaluator
//! (`docs/architecture.md` §9), with Claude Code's vocabulary: modes, allow/ask/deny
//! rules in Claude Code syntax and one evaluator for every runtime. Shell commands are
//! parsed into simple commands, never pattern-matched as strings.

mod eval;
pub mod pathguard;
mod rule;
mod settings;
pub mod shell;

use std::path::PathBuf;

pub use eval::{Decision, Evaluator, Request, Verdict};
pub use rule::Rule;
pub use settings::{LOCAL_SETTINGS, PROJECT_SETTINGS, Policy, load};

/// Why a rule, a settings file or a command could not be used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A rule string is malformed.
    #[error("permissions: invalid rule {rule:?}: {reason}")]
    BadRule {
        /// The rule as written.
        rule: String,
        /// What is wrong.
        reason: String,
    },
    /// A settings file is unreadable or invalid.
    #[error("permissions: {}: {}", path.display(), problems.join("; "))]
    Settings {
        /// The file.
        path: PathBuf,
        /// Every problem found.
        problems: Vec<String>,
    },
    /// A command line cannot be analysed safely.
    #[error("unparsable command: {0}")]
    Unparsable(String),
    /// The evaluator needs an absolute workspace.
    #[error("permissions: workspace {} is not absolute", .0.display())]
    Workspace(PathBuf),
    /// A relative path came without a base to resolve it against.
    #[error("pathguard: relative path {} without a base", .0.display())]
    RelativeWithoutBase(PathBuf),
    /// Resolving symlinks failed for a reason other than a missing file.
    #[error("pathguard: resolve {}: {source}", path.display())]
    Resolve {
        /// The path being resolved.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

//! Every git operation OpenChef performs (`docs/architecture.md` §8): task worktrees
//! outside the user's repository, step commits with OpenChef trailers, and patch
//! capture and reset on failover. Agents never move refs; this crate does.
//!
//! The user's working tree and current branch are never written here. Requires git
//! 2.25 or newer (`--pathspec-from-file`).

mod commit;
mod git;
mod integrate;
#[cfg(test)]
mod tests;
mod worktree;

use std::path::PathBuf;

pub use commit::{StepCommit, TRAILER_AGENT, TRAILER_POOL, TRAILER_STEP, TRAILER_TASK};
pub use git::Repo;
pub use integrate::{Merged, Rebased};
pub use worktree::Worktree;

/// The per-worktree directory OpenChef owns (patches, scratch files). Its contents
/// are never staged or cleaned.
pub const CONTROL_DIR: &str = ".openchef";

/// Why a git operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// git is not installed or not on `PATH`.
    #[error("gitws: git not found: {0}")]
    GitNotFound(#[source] std::io::Error),
    /// The path is not inside a (non-bare) git working tree.
    #[error("gitws: not a git repository: {}: {reason}", path.display())]
    NotRepository {
        /// The path.
        path: PathBuf,
        /// What git said.
        reason: String,
    },
    /// A path that should hold a task worktree holds something else.
    #[error("gitws: {} is not the expected worktree: {reason}", path.display())]
    WorktreeMismatch {
        /// The path.
        path: PathBuf,
        /// What differs.
        reason: String,
    },
    /// git exited with an error.
    #[error("gitws: git {args}: {stderr}")]
    Git {
        /// The arguments after the global options.
        args: String,
        /// git's stderr (or the exit status when it said nothing).
        stderr: String,
    },
    /// The caller passed something unusable.
    #[error("gitws: {0}")]
    Invalid(String),
    /// A file system or process operation failed.
    #[error("gitws: {context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

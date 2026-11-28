//! Integration (§8.1 `team`, D-041): rebasing a step branch onto the integration branch,
//! fast-forwarding the integration branch, and `openchef merge` into the user's branch.

use crate::commit::FALLBACK_IDENTITY;
use crate::git::Repo;
use crate::{Error, Worktree};

/// How a rebase went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebased {
    /// The branch now sits on top of the target; its new head.
    Clean(String),
    /// It conflicted and was aborted: the branch is unchanged. The conflicting files.
    Conflict(Vec<String>),
}

/// How `openchef merge` went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The user's branch moved forward to this commit.
    FastForward(String),
    /// A merge commit was made.
    Merge(String),
    /// It conflicted and was aborted: nothing changed. The conflicting files.
    Conflict(Vec<String>),
}

/// The unmerged paths of a stopped rebase or merge in `dir`.
async fn conflicts(git: &crate::git::Git, dir: &std::path::Path) -> Result<Vec<String>, Error> {
    let out = git
        .line(dir, &["diff", "--name-only", "--diff-filter=U"])
        .await?;
    Ok(out.lines().map(str::to_owned).collect())
}

impl Worktree {
    /// Rebases this worktree's branch onto `onto` (a branch or commit). A conflict is
    /// aborted, leaving the branch as it was, and reported with its files.
    ///
    /// # Errors
    ///
    /// Git failures other than a conflict.
    pub async fn rebase_onto(&self, onto: &str) -> Result<Rebased, Error> {
        let mut args: Vec<&str> = Vec::new();
        if !self.has_identity().await {
            args.extend(FALLBACK_IDENTITY);
        }
        args.extend(["rebase", "--quiet", "--no-autostash", onto]);
        match self.git.run(&self.path, None, &args).await {
            Ok(_) => Ok(Rebased::Clean(self.head().await?)),
            Err(Error::Git { stderr, args }) => {
                let files = conflicts(&self.git, &self.path).await.unwrap_or_default();
                // Abort whatever stopped, conflict or not, so the branch is untouched.
                let aborted = self.git.run(&self.path, None, &["rebase", "--abort"]).await;
                if files.is_empty() {
                    return Err(Error::Git { args, stderr });
                }
                aborted?;
                Ok(Rebased::Conflict(files))
            }
            Err(e) => Err(e),
        }
    }

    /// Fast-forwards this worktree's branch (checked out here) to `to`.
    ///
    /// # Errors
    ///
    /// When `to` does not descend from the branch, or git fails.
    pub async fn fast_forward(&self, to: &str) -> Result<String, Error> {
        self.git
            .run(&self.path, None, &["merge", "--ff-only", "--quiet", to])
            .await?;
        self.head().await
    }
}

impl Repo {
    /// Brings `branch` into the user's current branch (`openchef merge`): a
    /// fast-forward when possible, else a merge commit; a conflict is aborted. The one
    /// operation that writes the user's working tree (§8.2).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] on a detached HEAD or tracked changes; git failures.
    pub async fn merge_into_current(&self, branch: &str) -> Result<Merged, Error> {
        let current = self
            .git
            .line(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .map_err(|_| Error::Invalid("HEAD is detached; check out a branch first".into()))?;
        let dirty = self
            .git
            .line(
                &self.root,
                &["status", "--porcelain", "--untracked-files=no"],
            )
            .await?;
        if !dirty.is_empty() {
            return Err(Error::Invalid(format!(
                "{current} has uncommitted changes; commit or stash them first"
            )));
        }
        if self
            .git
            .run(&self.root, None, &["merge", "--ff-only", "--quiet", branch])
            .await
            .is_ok()
        {
            return Ok(Merged::FastForward(self.head().await?));
        }
        let mut args: Vec<&str> = Vec::new();
        let name = self.git.line(&self.root, &["config", "user.name"]).await;
        let email = self.git.line(&self.root, &["config", "user.email"]).await;
        if !matches!((name, email), (Ok(n), Ok(e)) if !n.is_empty() && !e.is_empty()) {
            args.extend(FALLBACK_IDENTITY);
        }
        args.extend(["merge", "--no-ff", "--no-edit", "--quiet", branch]);
        match self.git.run(&self.root, None, &args).await {
            Ok(_) => Ok(Merged::Merge(self.head().await?)),
            Err(Error::Git { args, stderr }) => {
                let files = conflicts(&self.git, &self.root).await.unwrap_or_default();
                let aborted = self.git.run(&self.root, None, &["merge", "--abort"]).await;
                if files.is_empty() {
                    return Err(Error::Git { args, stderr });
                }
                aborted?;
                Ok(Merged::Conflict(files))
            }
            Err(e) => Err(e),
        }
    }
}

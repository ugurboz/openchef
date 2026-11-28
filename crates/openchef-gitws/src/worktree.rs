//! Task worktrees outside the user's repository.

use std::path::{Path, PathBuf};

use crate::git::{Git, Repo};
use crate::{CONTROL_DIR, Error};

/// One task's (or team step's) linked worktree.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Where it is checked out.
    pub path: PathBuf,
    /// Its branch.
    pub branch: String,
    pub(crate) git: Git,
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

impl Repo {
    /// Creates `branch` at `base` and checks it out in a new linked worktree at `path`.
    /// Idempotent for recovery: if `path` already is a worktree of this repository on
    /// `branch`, it is returned as is.
    ///
    /// # Errors
    ///
    /// [`Error::WorktreeMismatch`] when `path` holds anything else; [`Error::Invalid`]
    /// for a relative path or an invalid branch name.
    pub async fn create_worktree(
        &self,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Worktree, Error> {
        if !path.is_absolute() {
            return Err(Error::Invalid(format!(
                "worktree path {} is not absolute",
                path.display()
            )));
        }
        if let Err(e) = self
            .git
            .run(&self.root, None, &["check-ref-format", "--branch", branch])
            .await
        {
            return Err(Error::Invalid(format!(
                "invalid branch name {branch:?}: {e}"
            )));
        }
        let wt = Worktree {
            path: path.to_path_buf(),
            branch: branch.to_owned(),
            git: self.git.clone(),
        };
        match std::fs::metadata(path) {
            Ok(_) => {
                self.check_existing(&wt).await?;
                return Ok(wt);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(format!("stat {}", path.display()), e)),
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io("create worktree parent", e))?;
        }
        let path_arg = path.to_string_lossy();
        let base_arg = format!("{base}^{{commit}}");
        self.git
            .run(
                &self.root,
                None,
                &[
                    "worktree", "add", "--quiet", "-b", branch, &path_arg, &base_arg,
                ],
            )
            .await?;
        Ok(wt)
    }

    async fn check_existing(&self, wt: &Worktree) -> Result<(), Error> {
        let mismatch = |reason: String| Error::WorktreeMismatch {
            path: wt.path.clone(),
            reason,
        };
        let top = wt
            .git
            .line(&wt.path, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|e| mismatch(e.to_string()))?;
        let common = wt
            .git
            .line(
                &wt.path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
            .map_err(|e| mismatch(e.to_string()))?;
        let branch = wt
            .git
            .line(&wt.path, &["symbolic-ref", "--short", "HEAD"])
            .await
            .map_err(|_| mismatch("detached HEAD".into()))?;
        let repo = Path::new(&common)
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);
        if !same_path(Path::new(&top), &wt.path)
            || !same_path(&repo, &self.root)
            || branch != wt.branch
        {
            return Err(mismatch(format!(
                "top {top}, repository {}, branch {branch}",
                repo.display()
            )));
        }
        Ok(())
    }

    /// Removes the worktree at `path` (forcefully: it holds only OpenChef's work) and
    /// prunes git's record of it; with `delete_branch`, the branch too. A path that no
    /// longer exists is not an error.
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn remove_worktree(
        &self,
        path: &Path,
        branch: &str,
        delete_branch: bool,
    ) -> Result<(), Error> {
        match std::fs::metadata(path) {
            Ok(_) => {
                let p = path.to_string_lossy();
                self.git
                    .run(
                        &self.root,
                        None,
                        &["worktree", "remove", "--force", "--force", &p],
                    )
                    .await?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(format!("stat {}", path.display()), e)),
        }
        self.git
            .run(&self.root, None, &["worktree", "prune"])
            .await?;
        if delete_branch && !branch.is_empty() {
            match self
                .git
                .run(&self.root, None, &["branch", "-D", "--", branch])
                .await
            {
                Err(Error::Git { stderr, .. }) if stderr.contains("not found") => {}
                other => drop(other?),
            }
        }
        Ok(())
    }
}

impl Worktree {
    /// The worktree's current commit.
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn head(&self) -> Result<String, Error> {
        self.git
            .line(&self.path, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
    }

    /// Paths with uncommitted changes (modified, added, deleted, untracked; renames as
    /// both sides), excluding ignored files and [`CONTROL_DIR`].
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn changed(&self) -> Result<Vec<String>, Error> {
        let out = self
            .git
            .run(
                &self.path,
                None,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--no-renames",
                ],
            )
            .await?;
        let control = format!("{CONTROL_DIR}/");
        Ok(out
            .split(|b| *b == 0)
            .filter(|e| e.len() >= 4)
            .map(|e| String::from_utf8_lossy(&e[3..]).into_owned())
            .filter(|p| p != CONTROL_DIR && !p.starts_with(&control))
            .collect())
    }

    /// The step's uncommitted change for a reviewer: `git diff HEAD` of tracked files and
    /// the contents of untracked ones, cut at `limit` bytes. Read-only: the index is not
    /// touched.
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn diff(&self, limit: usize) -> Result<String, Error> {
        let tracked = self
            .git
            .run(
                &self.path,
                None,
                &["diff", "--no-color", "--no-ext-diff", "HEAD", "--"],
            )
            .await?;
        let mut out = String::from_utf8_lossy(&tracked).into_owned();
        let untracked = self
            .git
            .run(
                &self.path,
                None,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )
            .await?;
        let control = format!("{CONTROL_DIR}/");
        for raw in untracked.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            if out.len() >= limit {
                break;
            }
            let rel = String::from_utf8_lossy(raw).into_owned();
            if rel == CONTROL_DIR || rel.starts_with(&control) {
                continue;
            }
            let body = std::fs::read(self.path.join(&rel)).unwrap_or_default();
            let text = if body.contains(&0) {
                "(binary file)".to_owned()
            } else {
                String::from_utf8_lossy(&body).into_owned()
            };
            out.push_str("new file: ");
            out.push_str(&rel);
            out.push('\n');
            out.push_str(&text);
            out.push('\n');
        }
        if out.len() > limit {
            let mut cut = limit;
            while !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
        }
        Ok(out)
    }
}

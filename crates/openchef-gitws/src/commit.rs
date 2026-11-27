//! Step commits with OpenChef trailers (§8.2), patch capture and reset on failover.

use std::path::Path;

use crate::worktree::Worktree;
use crate::{CONTROL_DIR, Error};

/// The task id trailer.
pub const TRAILER_TASK: &str = "OpenChef-Task";
/// The step number trailer.
pub const TRAILER_STEP: &str = "OpenChef-Step";
/// The agent trailer.
pub const TRAILER_AGENT: &str = "OpenChef-Agent";
/// The pool trailer.
pub const TRAILER_POOL: &str = "OpenChef-Pool";

/// Used only when the user has no git identity configured.
const FALLBACK_IDENTITY: [&str; 4] = [
    "-c",
    "user.name=OpenChef",
    "-c",
    "user.email=openchef@localhost",
];

/// The commit that records a verified step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepCommit {
    /// The task id.
    pub task: String,
    /// The step number, from 1.
    pub step: u32,
    /// The agent that did the work.
    pub agent: String,
    /// Its pool.
    pub pool: String,
    /// The first line of the message, e.g. the step title.
    pub subject: String,
}

impl StepCommit {
    fn message(&self) -> Result<String, Error> {
        if self.task.is_empty() || self.step < 1 || self.agent.is_empty() || self.pool.is_empty() {
            return Err(Error::Invalid(format!(
                "step commit needs task, step, agent and pool: {self:?}"
            )));
        }
        let first = self.subject.lines().next().unwrap_or("").trim();
        let subject = if first.is_empty() {
            format!("OpenChef step {}", self.step)
        } else {
            first.to_owned()
        };
        Ok(format!(
            "{subject}\n\n{TRAILER_TASK}: {}\n{TRAILER_STEP}: {}\n{TRAILER_AGENT}: {}\n{TRAILER_POOL}: {}\n",
            self.task, self.step, self.agent, self.pool
        ))
    }
}

impl Worktree {
    /// Commits the agent's changes ([`Worktree::changed`], so never [`CONTROL_DIR`] or
    /// ignored files) as the record of a verified step and returns the commit.
    /// Idempotent: if the branch already has a commit for this task and step since
    /// `base` (a replay after a crash), that commit is returned. A step without changes
    /// still gets an empty commit, so every verified step is in history. Hooks do not
    /// run.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an incomplete [`StepCommit`]; git failures.
    pub async fn commit_step(&self, base: &str, c: &StepCommit) -> Result<String, Error> {
        let message = c.message()?;
        if let Some(sha) = self.find_step_commit(base, &c.task, c.step).await? {
            return Ok(sha);
        }
        let paths = self.changed().await?;
        if !paths.is_empty() {
            let mut spec = paths.join("\0").into_bytes();
            spec.push(0);
            self.git
                .run(
                    &self.path,
                    Some(&spec),
                    &[
                        "add",
                        "--all",
                        "--pathspec-from-file=-",
                        "--pathspec-file-nul",
                    ],
                )
                .await?;
        }
        let mut args: Vec<&str> = Vec::new();
        if !self.has_identity().await {
            args.extend(FALLBACK_IDENTITY);
        }
        args.extend([
            "commit",
            "--quiet",
            "--no-verify",
            "--allow-empty",
            "--cleanup=verbatim",
            "--file=-",
        ]);
        self.git
            .run(&self.path, Some(message.as_bytes()), &args)
            .await?;
        self.head().await
    }

    async fn has_identity(&self) -> bool {
        let name = self.git.line(&self.path, &["config", "user.name"]).await;
        let email = self.git.line(&self.path, &["config", "user.email"]).await;
        matches!((name, email), (Ok(n), Ok(e)) if !n.is_empty() && !e.is_empty())
    }

    /// The commit recording `task`'s `step` on the branch since `base`, if any.
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn find_step_commit(
        &self,
        base: &str,
        task: &str,
        step: u32,
    ) -> Result<Option<String>, Error> {
        let format = format!(
            "--format=%H%x1f%(trailers:key={TRAILER_TASK},valueonly,separator=%x2C)%x1f%(trailers:key={TRAILER_STEP},valueonly,separator=%x2C)%x1e"
        );
        let range = format!("{base}..HEAD");
        let out = self
            .git
            .run(&self.path, None, &["log", &format, &range])
            .await?;
        let want = step.to_string();
        let text = String::from_utf8_lossy(&out);
        for record in text.split('\x1e') {
            let fields: Vec<&str> = record.trim().split('\x1f').collect();
            if let [sha, t, s] = fields.as_slice()
                && t.trim() == task
                && s.trim() == want
            {
                return Ok(Some((*sha).to_owned()));
            }
        }
        Ok(None)
    }

    /// Saves every uncommitted change (staged, unstaged, untracked; binary included;
    /// [`CONTROL_DIR`] excluded) to `file`, by convention
    /// `.openchef/patches/step-N-<agent>.patch`. Returns whether there was anything to
    /// save; no file is written otherwise. The index is left as HEAD's.
    ///
    /// # Errors
    ///
    /// Git or file system failures.
    pub async fn capture_patch(&self, file: &Path) -> Result<bool, Error> {
        let exclude = format!(":(exclude,top){CONTROL_DIR}");
        self.git
            .run(&self.path, None, &["add", "--all", "--", ".", &exclude])
            .await?;
        let patch = self
            .git
            .run(
                &self.path,
                None,
                &[
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                ],
            )
            .await;
        let reset = self.git.run(&self.path, None, &["reset", "--quiet"]).await;
        let patch = patch?;
        reset?;
        if patch.is_empty() {
            return Ok(false);
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io("create patch directory", e))?;
        }
        std::fs::write(file, patch)
            .map_err(|e| Error::io(format!("write patch {}", file.display()), e))?;
        Ok(true)
    }

    /// Discards every uncommitted change and moves the branch to `commit` (the last
    /// verified step), keeping [`CONTROL_DIR`].
    ///
    /// # Errors
    ///
    /// When git fails.
    pub async fn reset_to(&self, commit: &str) -> Result<(), Error> {
        let target = format!("{commit}^{{commit}}");
        self.git
            .run(&self.path, None, &["reset", "--hard", "--quiet", &target])
            .await?;
        let exclude = format!("--exclude=/{CONTROL_DIR}/");
        self.git
            .run(
                &self.path,
                None,
                &["clean", "-d", "--force", "--quiet", &exclude],
            )
            .await?;
        Ok(())
    }
}

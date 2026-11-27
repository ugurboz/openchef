//! Running git with a controlled environment, and the user's repository.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::Error;

/// Executes git with a controlled environment.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
}

/// Drops variables that would redirect git (`GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_INDEX_FILE`, …) and makes output stable and non-interactive.
pub(crate) fn git_env(
    from: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = from
        .into_iter()
        .filter(|(k, _)| {
            let up = k.to_string_lossy().to_uppercase();
            !(up.starts_with("GIT_") || up == "LC_ALL" || up == "LANG" || up == "LANGUAGE")
        })
        .collect();
    for (k, v) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
        ("GIT_OPTIONAL_LOCKS", "0"),
    ] {
        out.push((k.into(), v.into()));
    }
    out
}

impl Git {
    /// git from `PATH` with the process environment, cleaned.
    pub(crate) fn system() -> Self {
        Self {
            program: PathBuf::from("git"),
            env: git_env(std::env::vars_os()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_env(env: Vec<(OsString, OsString)>) -> Self {
        Self {
            program: PathBuf::from("git"),
            env,
        }
    }

    /// Runs git in `dir` and returns stdout.
    pub(crate) async fn run(
        &self,
        dir: &Path,
        stdin: Option<&[u8]>,
        args: &[&str],
    ) -> Result<Vec<u8>, Error> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("-C")
            .arg(dir)
            .args(["-c", "core.quotePath=false"])
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::GitNotFound(e)
            } else {
                Error::io("start git", e)
            }
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input)
                .await
                .map_err(|e| Error::io("write to git", e))?;
            drop(pipe); // EOF
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| Error::io("wait for git", e))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(Error::Git {
            args: args.join(" "),
            stderr: if stderr.is_empty() {
                out.status.to_string()
            } else {
                stderr
            },
        })
    }

    /// Runs git and returns trimmed stdout.
    pub(crate) async fn line(&self, dir: &Path, args: &[&str]) -> Result<String, Error> {
        let out = self.run(dir, None, args).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
}

/// The user's repository.
#[derive(Debug, Clone)]
pub struct Repo {
    /// The absolute path of the main working tree.
    pub root: PathBuf,
    pub(crate) git: Git,
}

impl Repo {
    /// Finds the repository containing `dir`. A linked worktree resolves to its main
    /// working tree, so tasks started anywhere in a repository share one repo id.
    ///
    /// # Errors
    ///
    /// [`Error::NotRepository`] outside a (non-bare) working tree; [`Error::GitNotFound`].
    pub async fn open(dir: &Path) -> Result<Self, Error> {
        Self::open_with(dir, Git::system()).await
    }

    pub(crate) async fn open_with(dir: &Path, git: Git) -> Result<Self, Error> {
        let not_repo = |reason: String| Error::NotRepository {
            path: dir.to_path_buf(),
            reason,
        };
        let common = match git
            .line(
                dir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
        {
            Ok(c) => PathBuf::from(c),
            Err(Error::GitNotFound(e)) => return Err(Error::GitNotFound(e)),
            Err(e) => return Err(not_repo(e.to_string())),
        };
        match git.line(dir, &["rev-parse", "--is-bare-repository"]).await {
            Ok(bare) if bare == "false" => {}
            Ok(_) => return Err(not_repo("bare repository".into())),
            Err(e) => return Err(not_repo(e.to_string())),
        }
        let root = if common.file_name().is_some_and(|n| n == ".git") {
            common.parent().map(Path::to_path_buf).unwrap_or(common)
        } else {
            // Unusual layout (separate git dir): fall back to this tree's top level.
            PathBuf::from(
                git.line(dir, &["rev-parse", "--show-toplevel"])
                    .await
                    .map_err(|e| not_repo(e.to_string()))?,
            )
        };
        // Resolve symlinks (macOS /var → /private/var) so every entry point agrees. On
        // Windows canonical paths carry a `\\?\` prefix git does not need; git's own
        // spelling is already stable there.
        #[cfg(unix)]
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Ok(Self { root, git })
    }

    /// A stable identifier for paths under `~/.openchef/worktrees`: the sanitized
    /// directory name and 12 hex digits of the root's SHA-256.
    #[must_use]
    pub fn id(&self) -> String {
        let digest = Sha256::digest(self.root.to_string_lossy().as_bytes());
        let hex = digest.iter().take(6).fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            // Writing to a String cannot fail.
            let _infallible = write!(s, "{b:02x}");
            s
        });
        let base = self
            .root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let name: String = base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{}-{hex}", name.trim_start_matches('.'))
    }

    /// The commit the user's checkout is on.
    ///
    /// # Errors
    ///
    /// When git fails (for example, no commit yet).
    pub async fn head(&self) -> Result<String, Error> {
        self.git
            .line(&self.root, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
    }
}

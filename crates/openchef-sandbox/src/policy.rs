//! The policy intent (§21.3): what a confined process may write and read. The same
//! intent drives Seatbelt on macOS and Landlock on Linux.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// What a confined process may touch. Everything under `home` is unreadable except
/// `readable` and `writable`; nothing is writable except `writable` (and a few devices);
/// `private` stays unreadable even inside an allowed directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The user's home directory.
    pub home: PathBuf,
    /// Read-write: the worktree, the session's temp dir, the agent CLI's state, caches.
    pub writable: Vec<PathBuf>,
    /// Read-only additions under `home`: toolchains, git configuration, the
    /// repository's git directory.
    pub readable: Vec<PathBuf>,
    /// Never readable or writable: OpenChef's own state and secrets.
    pub private: Vec<PathBuf>,
}

impl Policy {
    /// The policy with every path absolute and resolved through symlinks where it
    /// exists (Seatbelt and Landlock both check the real path: `/tmp` is
    /// `/private/tmp` on macOS, `/etc/resolv.conf` points into `/run` on Linux).
    /// Paths that do not exist yet are kept as given.
    ///
    /// # Errors
    ///
    /// [`Error::Policy`] for a relative path.
    pub fn canonical(&self) -> Result<Self, Error> {
        let one = |p: &PathBuf| -> Result<PathBuf, Error> {
            if !p.is_absolute() {
                return Err(Error::Policy(format!(
                    "{} is not an absolute path",
                    p.display()
                )));
            }
            Ok(resolve(p))
        };
        let many = |ps: &[PathBuf]| -> Result<Vec<PathBuf>, Error> {
            let mut out: Vec<PathBuf> = ps.iter().map(one).collect::<Result<_, _>>()?;
            out.sort();
            out.dedup();
            Ok(out)
        };
        Ok(Self {
            home: one(&self.home)?,
            writable: many(&self.writable)?,
            readable: many(&self.readable)?,
            private: many(&self.private)?,
        })
    }

    /// The toolchain and configuration locations under `home` that builds and agents
    /// read (S1, S2): language toolchains, version managers, git configuration.
    #[must_use]
    pub fn toolchains(home: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = [
            ".local/bin",
            ".local/share",
            ".cargo",
            ".rustup",
            ".nvm",
            ".volta",
            ".bun",
            ".npm-global",
            ".asdf",
            ".pyenv",
            ".sdkman",
            "go",
            ".gitconfig",
            ".config/git",
        ]
        .iter()
        .map(|rel| home.join(rel))
        .collect();
        if cfg!(target_os = "macos") {
            // Claude Code reads its login from the Keychain through securityd.
            out.push(home.join("Library/Keychains"));
        }
        out
    }
}

/// `p` resolved through symlinks when it exists; an existing parent resolves the rest.
fn resolve(p: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(p) {
        return real;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if parent != p => resolve(parent).join(name),
        _ => p.to_path_buf(),
    }
}

/// An agent CLI whose own state must stay writable inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
    /// Antigravity.
    Agy,
}

/// The paths an agent CLI writes for its sessions and logins (S1 §4): without them
/// `--resume` cannot find a session. `uid` names Claude's per-user temp directory.
#[must_use]
pub fn agent_state(cli: AgentCli, home: &Path, uid: u32) -> Vec<PathBuf> {
    match cli {
        AgentCli::Claude => {
            let tmp = if cfg!(target_os = "macos") {
                "/private/tmp"
            } else {
                "/tmp"
            };
            vec![
                home.join(".claude"),
                home.join(".claude.json"),
                PathBuf::from(tmp).join(format!("claude-{uid}")),
            ]
        }
        AgentCli::Codex => vec![home.join(".codex")],
        AgentCli::Agy => vec![home.join(".gemini")],
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{AgentCli, Policy, agent_state};

    #[test]
    fn canonical_resolves_and_refuses_relative() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap_or_else(|e| panic!("{e}"));
        let p = Policy {
            home: dir.path().to_path_buf(),
            writable: vec![real.clone(), real.join("not-yet"), real.clone()],
            readable: Vec::new(),
            private: Vec::new(),
        };
        let c = p.canonical().unwrap_or_else(|e| panic!("{e}"));
        let real = std::fs::canonicalize(&real).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            c.writable,
            [real.clone(), real.join("not-yet")],
            "deduplicated"
        );
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap_or_else(|e| panic!("{e}"));
            let c = Policy {
                writable: vec![link],
                ..p.clone()
            }
            .canonical()
            .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(c.writable, [real], "symlinks resolve to their target");
        }
        let bad = Policy {
            readable: vec![PathBuf::from("rel")],
            ..p
        };
        assert!(bad.canonical().is_err());
    }

    #[test]
    fn agent_state_paths() {
        let home = Path::new("/h");
        assert!(agent_state(AgentCli::Claude, home, 501).contains(&home.join(".claude.json")));
        assert!(
            agent_state(AgentCli::Claude, home, 501)
                .iter()
                .any(|p| p.ends_with("claude-501"))
        );
        assert_eq!(agent_state(AgentCli::Codex, home, 0), [home.join(".codex")]);
        assert_eq!(agent_state(AgentCli::Agy, home, 0), [home.join(".gemini")]);
        assert!(Policy::toolchains(home).contains(&home.join(".rustup")));
    }
}

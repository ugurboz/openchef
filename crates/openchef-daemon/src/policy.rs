//! Each session's permission policy (§9.1), ported from Go `internal/app/policy.go`:
//! the user's `~/.openchef/settings.toml` and the repository's project and local
//! files, read again for every session so edits apply to the next one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openchef_config::Paths;
use openchef_core::{Mode, PermissionMode, Task};
use openchef_permissions::Evaluator;
use openchef_runtime::INVARIANT_DENY_RULES;

/// Why `bypassPermissions` was refused for a task.
pub(crate) const BYPASS_OUTSIDE_WORKTREE: &str =
    "bypassPermissions is only allowed in an isolated worktree";

/// SQLite's companions of the state database, private like the database itself.
const STATE_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Builds session policies from the settings files.
#[derive(Debug, Clone)]
pub(crate) struct Policies {
    /// The user settings file.
    pub(crate) settings: PathBuf,
    /// The user's home directory, for `~/` rules.
    pub(crate) home: PathBuf,
    /// OpenChef's own files, which no agent may read or write.
    pub(crate) private: Vec<PathBuf>,
}

/// One session's policy: the evaluator its approver consults, and the rules passed
/// natively to CLIs that decide some requests themselves (§9.3).
#[derive(Debug, Clone)]
pub(crate) struct SessionPolicy {
    /// The task's permission mode.
    pub(crate) mode: PermissionMode,
    /// Decides the session's requests.
    pub(crate) eval: Arc<Evaluator>,
    /// The invariant deny rules, then the settings' deny rules.
    pub(crate) deny_rules: Vec<String>,
    /// The settings' ask rules.
    pub(crate) ask_rules: Vec<String>,
}

impl Policies {
    /// The policies for an OpenChef home: everything in it is private except the
    /// worktrees agents work in.
    pub(crate) fn new(paths: &Paths) -> Result<Self, String> {
        let home = std::env::home_dir().ok_or("cannot locate the user's home directory")?;
        let mut private = vec![
            paths.config.clone(),
            paths.settings.clone(),
            paths.state.clone(),
            paths.logs.clone(),
            paths.sessions.clone(),
            paths.socket.clone(),
            paths.lock.clone(),
        ];
        private.extend(STATE_SIDECARS.iter().map(|suffix| {
            let mut name = paths.state.clone().into_os_string();
            name.push(suffix);
            PathBuf::from(name)
        }));
        Ok(Self {
            settings: paths.settings.clone(),
            home,
            private,
        })
    }

    /// A repository's configured default permission mode.
    pub(crate) fn default_mode(&self, repo_root: &Path) -> Result<PermissionMode, String> {
        openchef_permissions::load(Some(&self.settings), Some(repo_root))
            .map(|p| p.mode)
            .map_err(|e| e.to_string())
    }

    /// The policy for a session of `t`, which must have its worktree.
    pub(crate) fn session(&self, t: &Task) -> Result<SessionPolicy, String> {
        let mut policy = openchef_permissions::load(Some(&self.settings), Some(&t.repo_root))
            .map_err(|e| e.to_string())?;
        let Some(workspace) = &t.worktree else {
            return Err(format!("task {} has no workspace yet", t.id));
        };
        if t.permissions == PermissionMode::BypassPermissions && t.mode != Mode::Isolated {
            return Err(BYPASS_OUTSIDE_WORKTREE.into());
        }
        // The task's mode, chosen when it was created, wins over defaultMode.
        policy.mode = t.permissions;
        let deny_rules = INVARIANT_DENY_RULES
            .iter()
            .map(|r| (*r).to_owned())
            .chain(policy.deny.iter().map(|r| r.raw.clone()))
            .collect();
        let ask_rules = policy.ask.iter().map(|r| r.raw.clone()).collect();
        let eval = Evaluator::new(policy, workspace, &self.home, self.private.clone())
            .map_err(|e| e.to_string())?;
        Ok(SessionPolicy {
            mode: t.permissions,
            eval: Arc::new(eval),
            deny_rules,
            ask_rules,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use openchef_config::Paths;
    use openchef_core::{
        Backend, Importance, Kind, Mode, PermissionMode, Task, TaskId, TaskStatus,
    };
    use openchef_runtime::INVARIANT_DENY_RULES;
    use time::OffsetDateTime;

    use super::{BYPASS_OUTSIDE_WORKTREE, Policies};

    /// Policies with `settings` as the user file, and a workspace with a `.git`.
    pub(crate) fn test_policies(dir: &Path, settings: &str) -> (Policies, PathBuf) {
        let user = dir.join("settings.toml");
        if !settings.is_empty() {
            std::fs::write(&user, settings).unwrap_or_else(|e| panic!("{e}"));
        }
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap_or_else(|e| panic!("{e}"));
        let pols = Policies {
            settings: user,
            home: dir.join("home"),
            private: vec![dir.join("state.db")],
        };
        (pols, ws)
    }

    pub(crate) fn test_task(ws: &Path, permissions: PermissionMode) -> Task {
        let now = OffsetDateTime::UNIX_EPOCH;
        Task {
            id: TaskId::generate().unwrap_or_else(|e| panic!("{e}")),
            kind: Kind::Run,
            status: TaskStatus::Running,
            prompt: "p".into(),
            repo_root: ws.to_path_buf(),
            base_commit: None,
            mode: Mode::Isolated,
            backend: Backend::Local,
            importance: Importance::Normal,
            agent_pin: None,
            permissions,
            branch: None,
            worktree: Some(ws.to_path_buf()),
            resume_at: None,
            reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn session_policy() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (pols, ws) = test_policies(
            dir.path(),
            "[permissions]\ndefaultMode = \"acceptEdits\"\ndeny = [\"Read(./.env)\"]\nask = [\"Bash(git push:*)\"]\n",
        );
        assert_eq!(pols.default_mode(&ws), Ok(PermissionMode::AcceptEdits));
        let ps = pols
            .session(&test_task(&ws, PermissionMode::Manual))
            .unwrap_or_else(|e| panic!("{e}"));
        // The task's mode wins; deny and ask rules reach the CLI natively, after the
        // invariants.
        assert_eq!(ps.mode, PermissionMode::Manual);
        assert_eq!(ps.eval.mode(), PermissionMode::Manual);
        assert_eq!(ps.deny_rules.len(), INVARIANT_DENY_RULES.len() + 1);
        assert_eq!(
            ps.deny_rules.last().map(String::as_str),
            Some("Read(./.env)")
        );
        assert_eq!(ps.ask_rules, ["Bash(git push:*)"]);

        let mut team = test_task(&ws, PermissionMode::BypassPermissions);
        team.mode = Mode::Team;
        assert_eq!(
            pols.session(&team).err().as_deref(),
            Some(BYPASS_OUTSIDE_WORKTREE)
        );
        let mut fresh = test_task(&ws, PermissionMode::Manual);
        fresh.worktree = None;
        assert!(
            pols.session(&fresh).is_err(),
            "a session without a workspace"
        );

        let other = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (bad, ws2) = test_policies(other.path(), "[permissions]\ndeny = [\"Bash(\"]\n");
        assert!(
            bad.session(&test_task(&ws2, PermissionMode::Manual))
                .is_err(),
            "a broken deny rule loaded"
        );
    }

    #[test]
    fn private_files() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let paths = Paths::in_home(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        let pols = Policies::new(&paths).unwrap_or_else(|e| panic!("{e}"));
        for p in [&paths.config, &paths.settings, &paths.state, &paths.socket] {
            assert!(pols.private.contains(p), "{} is not private", p.display());
        }
        let wal = PathBuf::from(format!("{}-wal", paths.state.display()));
        assert!(pols.private.contains(&wal));
        assert!(
            !pols
                .private
                .iter()
                .any(|p| p.starts_with(&paths.worktrees) || paths.worktrees.starts_with(p)),
            "worktrees must stay reachable"
        );
    }
}

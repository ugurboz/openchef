//! Each sandboxed task's policy (§21.3, D-036): what its agents and verification
//! commands may write and read, built from the task's worktree, the agent CLI and
//! OpenChef's own private files.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openchef_config::Paths;
use openchef_core::Task;
use openchef_procs::Confine;
use openchef_sandbox::{AgentCli, Policy, agent_state, private_dir, sandbox_env, wrap};

/// Builds sandbox policies for one daemon.
#[derive(Debug, Clone)]
pub(crate) struct Sandboxes {
    /// This executable: the Linux helper is `openchef __sandbox`.
    pub(crate) exe: PathBuf,
    /// The user's home directory.
    pub(crate) home: PathBuf,
    /// OpenChef's home.
    pub(crate) paths: Paths,
    /// OpenChef's own files, never readable inside the sandbox.
    pub(crate) private: Vec<PathBuf>,
}

/// A policy with the environment variables it needs.
pub(crate) struct Confinement {
    pub(crate) confine: Confine,
    pub(crate) env: Vec<(String, String)>,
}

/// The agent CLI a runtime name stands for.
fn cli(runtime: &str) -> Option<AgentCli> {
    match runtime {
        "claude" => Some(AgentCli::Claude),
        "codex" => Some(AgentCli::Codex),
        "agy" => Some(AgentCli::Agy),
        _ => None,
    }
}

/// The worktree's git directories: its own admin directory (index, HEAD; writable)
/// and the repository's common directory (objects, refs, config; read-only). A linked
/// worktree's `.git` is a file `gitdir: <common>/worktrees/<name>`.
fn git_dirs(worktree: &Path) -> Option<(PathBuf, PathBuf)> {
    let text = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let admin = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let admin = if admin.is_absolute() {
        admin
    } else {
        worktree.join(admin)
    };
    let common = admin.parent()?.parent()?.to_path_buf();
    Some((admin, common))
}

/// The owner of `home`: names Claude's per-user temp directory without `unsafe`.
fn uid_of(home: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(home).map_or(0, |m| m.uid())
    }
    #[cfg(not(unix))]
    {
        let _home = home;
        0
    }
}

impl Sandboxes {
    /// The policy every process of `t` shares: the worktree, a private temp dir, the
    /// repository's caches, its git directories and the toolchains.
    fn base(&self, t: &Task) -> Result<(Policy, Vec<(String, String)>), String> {
        let Some(worktree) = &t.worktree else {
            return Err(format!("task {} has no workspace yet", t.id));
        };
        let sandbox = self.paths.home.join("sandbox");
        let temp =
            private_dir(&sandbox.join("tmp").join(t.id.as_str())).map_err(|e| e.to_string())?;
        // Worktrees live in <worktrees>/<repo-id>/<task>, so the parent names the repo.
        let repo = worktree
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| OsString::from("default"), std::ffi::OsStr::to_os_string);
        let cache = private_dir(&sandbox.join("cache").join(repo)).map_err(|e| e.to_string())?;
        let mut writable = vec![worktree.clone(), temp.clone(), cache.clone()];
        let mut readable = Policy::toolchains(&self.home);
        if let Some((admin, common)) = git_dirs(worktree) {
            writable.push(admin);
            readable.push(common);
        }
        let policy = Policy {
            home: self.home.clone(),
            writable,
            readable,
            private: self.private.clone(),
        };
        Ok((policy, sandbox_env(&temp, &cache)))
    }

    fn confinement(
        &self,
        policy: &Policy,
        env: Vec<(String, String)>,
    ) -> Result<Confinement, String> {
        // Resolve once, so a bad path fails the session before any process starts.
        let policy = policy.canonical().map_err(|e| e.to_string())?;
        let exe = self.exe.clone();
        let confine: Confine =
            Arc::new(move |spec| wrap(&policy, spec, &exe).map_err(|e| e.to_string()));
        Ok(Confinement { confine, env })
    }

    /// The confinement of an agent session of `t` on `runtime`: the task's policy plus
    /// the CLI's own state, which `--resume` needs.
    pub(crate) fn session(&self, t: &Task, runtime: &str) -> Result<Confinement, String> {
        let (mut policy, env) = self.base(t)?;
        let Some(cli) = cli(runtime) else {
            return Err(format!("runtime {runtime} cannot run sandboxed yet"));
        };
        policy
            .writable
            .extend(agent_state(cli, &self.home, uid_of(&self.home)));
        self.confinement(&policy, env)
    }

    /// The confinement of `t`'s verification commands.
    pub(crate) fn verify(&self, t: &Task) -> Result<(Confine, Vec<(OsString, OsString)>), String> {
        let (policy, env) = self.base(t)?;
        let c = self.confinement(&policy, env)?;
        let env = c
            .env
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Ok((c.confine, env))
    }
}

/// `env` with `extra` replacing variables of the same name.
pub(crate) fn with_env(
    mut env: Vec<(String, String)>,
    extra: Vec<(String, String)>,
) -> Vec<(String, String)> {
    env.retain(|(k, _)| !extra.iter().any(|(e, _)| e == k));
    env.extend(extra);
    env
}

#[cfg(test)]
mod tests {
    use openchef_config::Paths;

    use super::{Sandboxes, git_dirs, with_env};
    use crate::policy::tests::test_task;

    #[test]
    fn git_dirs_of_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap_or_else(|e| panic!("{e}"));
        let admin = dir.path().join("repo/.git/worktrees/t1");
        std::fs::write(ws.join(".git"), format!("gitdir: {}\n", admin.display()))
            .unwrap_or_else(|e| panic!("{e}"));
        let (a, common) = git_dirs(&ws).unwrap_or_else(|| panic!("no git dirs"));
        assert_eq!(a, admin);
        assert_eq!(common, dir.path().join("repo/.git"));
        assert!(git_dirs(dir.path()).is_none(), "not a worktree");
    }

    #[test]
    fn session_policy_covers_the_task() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let paths = Paths::in_home(&dir.path().join("oc")).unwrap_or_else(|e| panic!("{e}"));
        let ws = paths.worktrees.join("repo-1").join("t");
        std::fs::create_dir_all(&ws).unwrap_or_else(|e| panic!("{e}"));
        let s = Sandboxes {
            exe: "/bin/openchef".into(),
            home: dir.path().to_path_buf(),
            paths: paths.clone(),
            private: vec![paths.state.clone()],
        };
        let t = test_task(&ws, openchef_core::PermissionMode::Manual);
        let (policy, env) = s.base(&t).unwrap_or_else(|e| panic!("{e}"));
        assert!(policy.writable.contains(&ws));
        let temp = paths.home.join("sandbox/tmp").join(t.id.as_str());
        assert!(temp.is_dir() && policy.writable.contains(&temp));
        let cache = paths.home.join("sandbox/cache/repo-1");
        assert!(policy.writable.contains(&cache), "per-repository cache");
        assert!(env.contains(&("TMPDIR".to_owned(), temp.to_string_lossy().into_owned())));
        assert!(s.session(&t, "claude").is_ok());
        assert!(s.session(&t, "fake").is_err(), "unknown CLIs fail closed");
        let mut none = t.clone();
        none.worktree = None;
        assert!(s.verify(&none).is_err());
    }

    #[test]
    fn extra_env_replaces() {
        let env = with_env(
            vec![("TMPDIR".into(), "/tmp".into()), ("A".into(), "1".into())],
            vec![("TMPDIR".into(), "/t".into())],
        );
        assert_eq!(
            env,
            [
                ("A".to_owned(), "1".to_owned()),
                ("TMPDIR".to_owned(), "/t".to_owned())
            ]
        );
    }

    /// The daemon's own policy, enforced by the real kernel: a verification command of
    /// a sandboxed task in a linked worktree of a repository under the home directory
    /// can run git and write its worktree, and cannot read a secret or OpenChef's state.
    #[cfg(target_os = "macos")]
    #[test]
    fn verify_confinement_is_enforced() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let home = std::fs::canonicalize(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        let paths = Paths::in_home(&home.join(".openchef")).unwrap_or_else(|e| panic!("{e}"));
        paths.ensure_dirs().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&paths.state, "db").unwrap_or_else(|e| panic!("{e}"));
        let repo = home.join("code/repo");
        std::fs::create_dir_all(&repo).unwrap_or_else(|e| panic!("{e}"));
        let git = |dir: &std::path::Path, args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(dir).args(args).status();
            assert!(ok.is_ok_and(|s| s.success()), "git {args:?}");
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        let ws = paths.worktrees.join("repo-1").join("t");
        git(&repo, &["worktree", "add", "-q", &ws.to_string_lossy()]);
        std::fs::write(home.join("secret.txt"), "s3cret").unwrap_or_else(|e| panic!("{e}"));
        let s = Sandboxes {
            exe: "/unused".into(),
            home: home.clone(),
            paths: paths.clone(),
            private: vec![paths.state.clone()],
        };
        let mut t = test_task(&ws, openchef_core::PermissionMode::Manual);
        t.backend = openchef_core::Backend::Sandbox;
        let (confine, env) = s.verify(&t).unwrap_or_else(|e| panic!("{e}"));
        let sh = |script: &str| {
            let spec = openchef_procs::Spec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                dir: Some(ws.clone()),
                env: env.clone(),
                stderr: openchef_procs::Stderr::Discard,
            };
            let w = confine(spec).unwrap_or_else(|e| panic!("{e}"));
            Command::new(&w.program)
                .args(&w.args)
                .current_dir(&ws)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(w.env.iter().map(|(k, v)| (k, v)))
                .output()
                .unwrap_or_else(|e| panic!("{e}"))
        };
        let ok = sh("echo x > out.txt && git status --short && echo t > \"$TMPDIR/t\"");
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert!(ws.join("out.txt").exists());
        for denied in [
            format!("cat '{}'", home.join("secret.txt").display()),
            format!("cat '{}'", paths.state.display()),
            format!("echo x > '{}'", home.join("escape.txt").display()),
            format!("echo x > '{}'", repo.join("in-main-checkout.txt").display()),
        ] {
            assert!(!sh(&denied).status.success(), "allowed: {denied}");
        }
    }
}

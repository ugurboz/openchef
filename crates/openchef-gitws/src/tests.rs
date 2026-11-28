//! Ported from `internal/gitws/gitws_test.go`. Every test uses a fresh repository and
//! a git isolated from the developer's and CI's configuration.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code"
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::git::{Git, git_env};
use crate::{CONTROL_DIR, Error, Repo, StepCommit, Worktree};

struct Fixture {
    repo: Repo,
    git: Git,
    dirs: Vec<tempfile::TempDir>,
}

fn test_git(home: &Path) -> Git {
    let mut env = git_env(std::env::vars_os());
    for k in ["HOME", "XDG_CONFIG_HOME", "USERPROFILE"] {
        env.push((OsString::from(k), home.as_os_str().to_owned()));
    }
    env.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    Git::with_env(env)
}

async fn must(g: &Git, dir: &Path, args: &[&str]) -> String {
    g.line(dir, args)
        .await
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// A repository with one commit, an identity and an ignore rule.
async fn new_repo() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let g = test_git(home.path());
    let dir = parent.path().join("my repo");
    std::fs::create_dir(&dir).unwrap();
    must(&g, &dir, &["init", "--quiet", "--initial-branch=main"]).await;
    must(&g, &dir, &["config", "user.name", "Test User"]).await;
    must(&g, &dir, &["config", "user.email", "test@example.com"]).await;
    write(&dir.join("README.md"), "hello\n");
    write(&dir.join(".gitignore"), "*.log\n");
    write(&dir.join("gone.txt"), "delete me\n");
    must(&g, &dir, &["add", "."]).await;
    must(&g, &dir, &["commit", "--quiet", "-m", "initial"]).await;
    let repo = Repo::open_with(&dir, g.clone()).await.unwrap();
    Fixture {
        repo,
        git: g,
        dirs: vec![home, parent],
    }
}

async fn new_worktree(f: &mut Fixture) -> (Worktree, String) {
    let base = f.repo.head().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("worktrees")
        .join(f.repo.id())
        .join("k3m9x2q7ab");
    f.dirs.push(dir);
    let wt = f
        .repo
        .create_worktree(&path, "openchef/k3m9x2q7ab", &base)
        .await
        .unwrap();
    (wt, base)
}

fn step(n: u32, agent: &str, pool: &str) -> StepCommit {
    StepCommit {
        task: "k3m9x2q7ab".into(),
        step: n,
        agent: agent.into(),
        pool: pool.into(),
        subject: String::new(),
    }
}

#[tokio::test]
async fn open_resolves_main_tree() {
    let mut f = new_repo().await;
    let sub = f.repo.root.join("pkg").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    let from_sub = Repo::open_with(&sub, f.git.clone()).await.unwrap();
    assert_eq!(from_sub.root, f.repo.root);
    let (wt, _) = new_worktree(&mut f).await;
    let from_wt = Repo::open_with(&wt.path, f.git.clone()).await.unwrap();
    assert_eq!(
        (from_wt.root.clone(), from_wt.id()),
        (f.repo.root.clone(), f.repo.id())
    );
    let id = f.repo.id();
    assert!(
        id.starts_with("my_repo-") && id.len() == "my_repo-".len() + 12,
        "{id}"
    );
    let elsewhere = tempfile::tempdir().unwrap();
    assert!(matches!(
        Repo::open_with(elsewhere.path(), f.git.clone()).await,
        Err(Error::NotRepository { .. })
    ));
}

/// Creating and using a task worktree never touches the user's checkout.
#[tokio::test]
async fn worktree_leaves_user_tree_alone() {
    let mut f = new_repo().await;
    let root = f.repo.root.clone();
    write(&root.join("README.md"), "user edit in progress\n");
    write(&root.join("notes.txt"), "untracked\n");
    let before = must(&f.git, &root, &["status", "--porcelain"]).await;
    let branch_before = must(&f.git, &root, &["symbolic-ref", "HEAD"]).await;
    let (wt, base) = new_worktree(&mut f).await;
    write(&wt.path.join("feature.go"), "package x\n");
    wt.commit_step(
        &base,
        &StepCommit {
            subject: "Add x".into(),
            ..step(1, "opus", "claude-max")
        },
    )
    .await
    .unwrap();
    wt.reset_to(&base).await.unwrap();
    assert_eq!(
        must(&f.git, &root, &["status", "--porcelain"]).await,
        before,
        "user status changed"
    );
    assert_eq!(
        must(&f.git, &root, &["symbolic-ref", "HEAD"]).await,
        branch_before,
        "user branch changed"
    );
    assert_eq!(read(&root.join("README.md")), "user edit in progress\n");
    assert!(
        !wt.path.starts_with(&root),
        "worktree inside the repository"
    );
}

#[tokio::test]
async fn create_worktree_is_idempotent() {
    let mut f = new_repo().await;
    let (wt, base) = new_worktree(&mut f).await;
    let again = f
        .repo
        .create_worktree(&wt.path, &wt.branch, &base)
        .await
        .unwrap();
    assert_eq!(again.path, wt.path);
    assert!(matches!(
        f.repo
            .create_worktree(&wt.path, "openchef/other", &base)
            .await,
        Err(Error::WorktreeMismatch { .. })
    ));
    let plain_dir = tempfile::tempdir().unwrap();
    let plain = plain_dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert!(matches!(
        f.repo
            .create_worktree(&plain, "openchef/plain", &base)
            .await,
        Err(Error::WorktreeMismatch { .. })
    ));
    assert!(
        f.repo
            .create_worktree(Path::new("relative/path"), "openchef/rel", &base)
            .await
            .is_err(),
        "relative path accepted"
    );
    let fresh = plain_dir.path().join("x");
    assert!(
        f.repo
            .create_worktree(&fresh, "bad..name", &base)
            .await
            .is_err(),
        "invalid branch name accepted"
    );
}

#[tokio::test]
async fn commit_step() {
    let mut f = new_repo().await;
    let (wt, base) = new_worktree(&mut f).await;
    write(&wt.path.join("api").join("limit.go"), "package api\n");
    write(&wt.path.join("README.md"), "changed\n");
    write(&wt.path.join("debug.log"), "ignored\n");
    write(
        &wt.path
            .join(CONTROL_DIR)
            .join("patches")
            .join("step-1-opus.patch"),
        "x",
    );
    std::fs::remove_file(wt.path.join("gone.txt")).unwrap();
    let mut changed = wt.changed().await.unwrap();
    changed.sort();
    assert_eq!(changed, ["README.md", "api/limit.go", "gone.txt"]);

    let c = StepCommit {
        subject: "Add rate limiting\nignored second line".into(),
        ..step(1, "opus", "claude-max")
    };
    let sha = wt.commit_step(&base, &c).await.unwrap();
    let mut files: Vec<String> = must(
        &f.git,
        &wt.path,
        &["show", "--name-only", "--format=", &sha],
    )
    .await
    .split_whitespace()
    .map(str::to_owned)
    .collect();
    files.sort();
    assert_eq!(files, ["README.md", "api/limit.go", "gone.txt"]);
    let msg = must(&f.git, &wt.path, &["log", "-1", "--format=%B", &sha]).await;
    for want in [
        "Add rate limiting\n",
        "OpenChef-Task: k3m9x2q7ab",
        "OpenChef-Step: 1",
        "OpenChef-Agent: opus",
        "OpenChef-Pool: claude-max",
    ] {
        assert!(msg.contains(want), "message lacks {want:?}:\n{msg}");
    }
    assert_eq!(
        must(&f.git, &wt.path, &["log", "-1", "--format=%an <%ae>", &sha]).await,
        "Test User <test@example.com>"
    );

    // A replay after a crash finds the existing commit instead of committing twice.
    write(&wt.path.join("later.go"), "package later\n");
    assert_eq!(wt.commit_step(&base, &c).await.unwrap(), sha);
    assert_eq!(
        must(
            &f.git,
            &wt.path,
            &["rev-list", "--count", &format!("{base}..HEAD")]
        )
        .await,
        "1"
    );

    // A step with nothing to commit is still recorded.
    std::fs::remove_file(wt.path.join("later.go")).unwrap();
    let empty = wt
        .commit_step(&base, &step(2, "gpt", "chatgpt-plus"))
        .await
        .unwrap();
    assert_ne!(empty, sha);
    assert_eq!(
        wt.find_step_commit(&base, "k3m9x2q7ab", 2).await.unwrap(),
        Some(empty)
    );
    assert_eq!(
        wt.find_step_commit(&base, "otherrtask", 1).await.unwrap(),
        None
    );
    let incomplete = StepCommit {
        task: "k3m9x2q7ab".into(),
        ..StepCommit::default()
    };
    assert!(matches!(
        wt.commit_step(&base, &incomplete).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn capture_patch_and_reset() {
    let mut f = new_repo().await;
    let (wt, base) = new_worktree(&mut f).await;
    let patch = wt
        .path
        .join(CONTROL_DIR)
        .join("patches")
        .join("step-1-opus.patch");
    assert!(!wt.capture_patch(&patch).await.unwrap(), "clean tree saved");
    assert!(!patch.exists(), "patch written for a clean tree");

    write(&wt.path.join("README.md"), "partial work\n");
    write(&wt.path.join("new").join("file.go"), "package new\n");
    std::fs::write(wt.path.join("blob.bin"), [0u8, 1, 2, 255, 0]).unwrap();
    std::fs::remove_file(wt.path.join("gone.txt")).unwrap();
    write(&wt.path.join("trace.log"), "ignored stays out\n");

    assert!(wt.capture_patch(&patch).await.unwrap());
    let body = read(&patch);
    assert!(
        !body.contains("trace.log") && !body.contains(CONTROL_DIR),
        "patch includes ignored or control files"
    );
    wt.reset_to(&base).await.unwrap();
    assert!(
        wt.changed().await.unwrap().is_empty(),
        "changes after reset"
    );
    assert!(patch.exists(), "reset removed the saved patch");

    // The handoff agent (or a human) can re-apply the saved work exactly.
    must(
        &f.git,
        &wt.path,
        &["apply", "--binary", &patch.to_string_lossy()],
    )
    .await;
    assert_eq!(read(&wt.path.join("README.md")), "partial work\n");
    assert_eq!(
        std::fs::read(wt.path.join("blob.bin")).unwrap(),
        [0u8, 1, 2, 255, 0]
    );
    assert!(!wt.path.join("gone.txt").exists(), "deletion not in patch");
}

#[tokio::test]
async fn state_identifies_content() {
    let mut f = new_repo().await;
    let (wt, _base) = new_worktree(&mut f).await;
    let clean = wt.state().await.unwrap();
    write(&wt.path.join("a.txt"), "one\n");
    let one = wt.state().await.unwrap();
    write(&wt.path.join("a.txt"), "two\n");
    let two = wt.state().await.unwrap();
    write(&wt.path.join(CONTROL_DIR).join("note"), "ignored\n");
    assert_eq!(
        wt.state().await.unwrap(),
        two,
        "the control dir is not content"
    );
    write(&wt.path.join("a.txt"), "one\n");
    assert_eq!(wt.state().await.unwrap(), one, "back to an earlier state");
    assert!(clean != one && one != two);
    assert_eq!(
        wt.changed().await.unwrap(),
        ["a.txt"],
        "the index is left as HEAD's"
    );
}

#[tokio::test]
async fn reset_moves_back_to_verified_commit() {
    let mut f = new_repo().await;
    let (wt, base) = new_worktree(&mut f).await;
    write(&wt.path.join("a.go"), "package a\n");
    let first = wt.commit_step(&base, &step(1, "opus", "p")).await.unwrap();
    write(&wt.path.join("b.go"), "package b\n");
    wt.commit_step(&base, &step(2, "opus", "p")).await.unwrap();
    wt.reset_to(&first).await.unwrap();
    assert_eq!(wt.head().await.unwrap(), first);
}

#[tokio::test]
async fn remove_worktree() {
    let mut f = new_repo().await;
    let (wt, _) = new_worktree(&mut f).await;
    write(&wt.path.join("dirty.go"), "package dirty\n");
    f.repo
        .remove_worktree(&wt.path, &wt.branch, true)
        .await
        .unwrap();
    assert!(!wt.path.exists(), "worktree still on disk");
    assert_eq!(
        must(&f.git, &f.repo.root, &["branch", "--list", &wt.branch]).await,
        "",
        "branch kept"
    );
    f.repo
        .remove_worktree(&wt.path, &wt.branch, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn fallback_identity() {
    let mut f = new_repo().await;
    let root: PathBuf = f.repo.root.clone();
    must(&f.git, &root, &["config", "--unset", "user.name"]).await;
    must(&f.git, &root, &["config", "--unset", "user.email"]).await;
    let (wt, base) = new_worktree(&mut f).await;
    let sha = wt.commit_step(&base, &step(1, "opus", "p")).await.unwrap();
    assert_eq!(
        must(&f.git, &wt.path, &["log", "-1", "--format=%an <%ae>", &sha]).await,
        "OpenChef <openchef@localhost>"
    );
}

#[test]
fn git_env_cleans() {
    let input = [
        ("PATH", "/bin"),
        ("GIT_DIR", "/evil"),
        ("git_work_tree", "/x"),
        ("LANG", "tr_TR.UTF-8"),
        ("HOME", "/h"),
    ];
    let got: Vec<(String, String)> =
        git_env(input.map(|(k, v)| (OsString::from(k), OsString::from(v))))
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
    let want = [
        ("PATH", "/bin"),
        ("HOME", "/h"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
        ("GIT_OPTIONAL_LOCKS", "0"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    assert_eq!(got, want);
}

#[tokio::test]
async fn diff_shows_tracked_and_untracked_changes() {
    let mut f = new_repo().await;
    let (wt, _base) = new_worktree(&mut f).await;
    std::fs::write(wt.path.join("README.md"), "changed\n").unwrap();
    std::fs::write(wt.path.join("new.rs"), "fn main() {}\n").unwrap();
    std::fs::create_dir_all(wt.path.join(CONTROL_DIR)).unwrap();
    std::fs::write(wt.path.join(CONTROL_DIR).join("x"), "secret").unwrap();
    let d = wt.diff(10_000).await.unwrap();
    assert!(d.contains("+changed"), "{d}");
    assert!(d.contains("new file: new.rs\nfn main() {}"), "{d}");
    assert!(!d.contains("secret"), "the control dir stays out: {d}");
    assert!(
        wt.changed().await.unwrap().contains(&"new.rs".to_owned()),
        "index untouched"
    );
    assert!(wt.diff(5).await.unwrap().len() <= 5);
}

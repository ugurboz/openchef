//! Ported from `internal/pathguard/pathguard_test.go`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use openchef_permissions::pathguard::check;

fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    for d in [
        ws.join("src"),
        ws.join(".git").join("hooks"),
        outside.clone(),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    (dir, ws, outside)
}

fn refused(ws: &Path, p: &Path) -> Option<String> {
    check(ws, p).unwrap_or_else(|e| panic!("check({}): {e}", p.display()))
}

#[test]
fn check_allows_and_refuses() {
    let (_dir, ws, outside) = workspace();
    let ok = [
        ws.join("src").join("a.go"),
        ws.join("new").join("dir").join("b.go"),
        PathBuf::from("src/c.go"),
        ws.join(".gitignore"),
        ws.join("git").join("x"),
    ];
    for p in &ok {
        assert_eq!(refused(&ws, p), None, "{} must be allowed", p.display());
    }
    let mut bad = vec![
        outside.join("x"),
        ws.join("..").join("outside").join("x"),
        ws.join(".git").join("hooks").join("pre-commit"),
        ws.join("sub").join(".claude").join("settings.json"),
        ws.join(".openchef").join("x"),
        PathBuf::from("../outside/y"),
    ];
    if cfg!(any(target_os = "macos", windows)) {
        bad.push(ws.join(".GIT").join("hooks").join("post-commit"));
        bad.push(ws.join(".Claude").join("settings.json"));
    }
    for p in &bad {
        assert!(refused(&ws, p).is_some(), "{} must be refused", p.display());
    }
    assert!(
        check(Path::new(""), Path::new("/x")).unwrap().is_some(),
        "an empty workspace allowed a write"
    );
}

#[cfg(unix)]
#[test]
fn symlink_escape() {
    use std::os::unix::fs::symlink;
    let (_dir, ws, outside) = workspace();
    symlink(&outside, ws.join("vendor")).unwrap();
    symlink(ws.join(".git"), ws.join("x")).unwrap();
    for p in [
        ws.join("vendor").join("etc").join("passwd"),
        ws.join("x").join("hooks").join("post-checkout"),
        ws.join("vendor"),
    ] {
        assert!(
            refused(&ws, &p).is_some(),
            "{}: a symlink must not open a way out",
            p.display()
        );
    }
    // A workspace reached through a symlink (macOS /var -> /private/var) still works.
    let alias = ws.parent().unwrap().join("alias");
    symlink(&ws, &alias).unwrap();
    assert_eq!(refused(&alias, &ws.join("src").join("a.go")), None);
}

#[cfg(windows)]
#[test]
fn windows_aliases() {
    let (_dir, ws, _) = workspace();
    for name in [".git.", ".git ", ".git::$INDEX_ALLOCATION", "GIT~1"] {
        let why = refused(&ws, &ws.join(name).join("hooks").join("x")).unwrap_or_default();
        assert!(why.contains("managed by OpenChef"), "{name:?}: {why:?}");
    }
}

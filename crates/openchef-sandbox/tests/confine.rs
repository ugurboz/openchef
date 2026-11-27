//! The policy enforced by the real kernel (S1, S2): a confined command cannot read a
//! secret in the home directory or write outside its writable paths, and can work in
//! its worktree.
#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use openchef_procs::{Spec, Stderr};
use openchef_sandbox::{Policy, check, wrap};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    ws: PathBuf,
    tmp: PathBuf,
    secret: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let home = root.join("home");
    let ws = home.join("ws");
    let tmp = home.join("tmp");
    let state = home.join(".openchef");
    for d in [&ws, &tmp, &state] {
        std::fs::create_dir_all(d).unwrap();
    }
    let secret = home.join("secret.txt");
    std::fs::write(&secret, "s3cret").unwrap();
    std::fs::write(state.join("state.db"), "db").unwrap();
    Fixture {
        _dir: dir,
        home,
        ws,
        tmp,
        secret,
        state,
    }
}

fn policy(f: &Fixture) -> Policy {
    Policy {
        home: f.home.clone(),
        writable: vec![f.ws.clone(), f.tmp.clone()],
        readable: Vec::new(),
        private: vec![f.state.clone()],
    }
}

/// Runs `sh -c script` confined; returns success and stderr.
fn confined(f: &Fixture, script: &str) -> (bool, String) {
    let spec = Spec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        dir: Some(f.ws.clone()),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        stderr: Stderr::Discard,
    };
    let helper = Path::new(env!("CARGO_BIN_EXE_sandbox-test-helper"));
    let w = wrap(&policy(f), spec, helper).unwrap();
    let out = Command::new(&w.program)
        .args(&w.args)
        .current_dir(w.dir.as_ref().unwrap())
        .env_clear()
        .envs(w.env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn kernel_enforces_the_policy() {
    match check() {
        Ok(_) => {}
        // Only a Linux kernel without Landlock may skip (`openchef doctor` reports it in
        // the product); macOS always has Seatbelt.
        Err(e) if cfg!(target_os = "linux") => {
            eprintln!("skipped: {e}");
            return;
        }
        Err(e) => panic!("sandbox unavailable: {e}"),
    }
    let f = fixture();
    let secret = f.secret.display();
    let (ok, err) = confined(&f, &format!("cat '{secret}'"));
    assert!(!ok, "read a secret in the home directory: {err}");
    let (ok, _) = confined(&f, &format!("echo x > '{}/escape.txt'", f.home.display()));
    assert!(!ok, "wrote outside the worktree");
    assert!(!f.home.join("escape.txt").exists());
    let (ok, _) = confined(&f, &format!("cat '{}/state.db'", f.state.display()));
    assert!(!ok, "read OpenChef's private state");
    let (ok, err) = confined(
        &f,
        &format!(
            "echo x > out.txt && echo y > '{}/t' && cat /etc/hosts > /dev/null",
            f.tmp.display()
        ),
    );
    assert!(ok, "worktree, temp dir and system reads must work: {err}");
    assert!(f.ws.join("out.txt").exists());
}

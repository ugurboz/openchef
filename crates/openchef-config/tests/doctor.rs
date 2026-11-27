//! Ported from `internal/doctor/doctor_test.go` (Go reference).

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openchef_config::{
    CommandOutput, Detector, Login, Runner, Runtime, RuntimeStatus, SystemRunner,
};

/// Answers commands from a `"path args…"` table.
struct Table(BTreeMap<String, Result<CommandOutput, String>>);

impl Runner for Table {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<CommandOutput, String> {
        let key = format!("{} {}", program.display(), args.join(" "));
        self.0
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Err(format!("unexpected command: {key}")))
    }
}

// Returns a `Result` because the command table holds runner results.
#[allow(clippy::unnecessary_wraps)]
fn out(stdout: &str) -> Result<CommandOutput, String> {
    Ok(CommandOutput {
        stdout: stdout.into(),
        stderr: Vec::new(),
        code: 0,
    })
}

fn fake(installed: &[(&str, &str)], cmds: &[(&str, Result<CommandOutput, String>)]) -> Detector {
    let installed: BTreeMap<String, PathBuf> = installed
        .iter()
        .map(|(b, p)| ((*b).to_owned(), PathBuf::from(p)))
        .collect();
    let table = Table(
        cmds.iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    );
    Detector::new(
        move |bin| installed.get(bin).cloned(),
        table,
        Duration::from_secs(1),
    )
}

fn by_runtime(statuses: Vec<RuntimeStatus>) -> BTreeMap<Runtime, RuntimeStatus> {
    statuses.into_iter().map(|s| (s.runtime, s)).collect()
}

#[test]
fn detect_all_installed_and_logged_in() {
    let d = fake(
        &[
            ("claude", "/bin/claude"),
            ("codex", "/bin/codex"),
            ("agy", "/bin/agy"),
        ],
        &[
            ("/bin/claude --version", out("2.1.325 (Claude Code)\n")),
            (
                "/bin/claude auth status",
                out(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#),
            ),
            ("/bin/codex --version", out("codex-cli 0.172.0\n")),
            (
                "/bin/codex login status",
                Ok(CommandOutput {
                    stdout: Vec::new(),
                    stderr: b"Logged in using ChatGPT\n".to_vec(),
                    code: 0,
                }),
            ),
            ("/bin/agy --version", out("1.5.16\n")),
        ],
    );
    let got = by_runtime(d.detect());
    let status = |runtime, binary, path: &str, version: &str, login, detail: &str| RuntimeStatus {
        runtime,
        binary,
        found: true,
        path: Some(path.into()),
        version: Some(version.to_owned()),
        login,
        login_detail: Some(detail.to_owned()),
        problem: None,
    };
    assert_eq!(
        got[&Runtime::Claude],
        status(
            Runtime::Claude,
            "claude",
            "/bin/claude",
            "2.1.325",
            Login::LoggedIn,
            "claude.ai"
        )
    );
    assert_eq!(
        got[&Runtime::Codex],
        status(
            Runtime::Codex,
            "codex",
            "/bin/codex",
            "0.172.0",
            Login::LoggedIn,
            "Logged in using ChatGPT"
        )
    );
    assert_eq!(
        got[&Runtime::Agy],
        status(
            Runtime::Agy,
            "agy",
            "/bin/agy",
            "1.5.16",
            Login::Unknown,
            "agy has no login status command"
        )
    );
    assert!(got.values().all(RuntimeStatus::usable));
}

#[test]
fn detect_problems() {
    let d = fake(
        &[("claude", "/bin/claude"), ("codex", "/bin/codex")],
        &[
            ("/bin/claude --version", out("2.1.325 (Claude Code)\n")),
            (
                "/bin/claude auth status",
                Ok(CommandOutput {
                    stdout: br#"{"loggedIn": false}"#.to_vec(),
                    stderr: Vec::new(),
                    code: 1,
                }),
            ),
            (
                "/bin/codex --version",
                Ok(CommandOutput {
                    code: 127,
                    ..CommandOutput::default()
                }),
            ),
        ],
    );
    let got = by_runtime(d.detect());
    assert!(got[&Runtime::Claude].login == Login::LoggedOut && !got[&Runtime::Claude].usable());
    assert!(got[&Runtime::Codex].problem.is_some() && !got[&Runtime::Codex].usable());
    assert!(
        !got[&Runtime::Agy].found
            && got[&Runtime::Agy].problem.is_some()
            && !got[&Runtime::Agy].usable()
    );
}

#[test]
fn claude_status_unexpected_output() {
    let d = fake(
        &[("claude", "/bin/claude")],
        &[
            ("/bin/claude --version", out("2.1.325 (Claude Code)")),
            ("/bin/claude auth status", out("not json")),
        ],
    );
    let s = &by_runtime(d.detect())[&Runtime::Claude];
    assert_eq!(s.login, Login::Unknown);
    assert!(
        s.login_detail
            .as_deref()
            .unwrap()
            .contains("unexpected output")
    );
    assert!(
        s.usable(),
        "an unknown login state must not make the runtime unusable"
    );
}

#[test]
fn command_failure_is_reported() {
    let d = fake(
        &[("claude", "/bin/claude")],
        &[("/bin/claude --version", Err("timed out".to_owned()))],
    );
    let s = &by_runtime(d.detect())[&Runtime::Claude];
    assert!(
        s.found && s.problem.as_deref().unwrap().contains("timed out"),
        "{s:?}"
    );
}

#[test]
fn system_runner() {
    let exe = std::env::current_exe().unwrap();
    // The test binary itself: `--list` prints the tests and exits 0.
    let res = SystemRunner
        .run(&exe, &["--list"], Duration::from_secs(30))
        .unwrap();
    assert_eq!(res.code, 0);
    let res = SystemRunner
        .run(&exe, &["--no-such-flag"], Duration::from_secs(30))
        .unwrap();
    assert_ne!(res.code, 0, "a non-zero exit is a result, not an error");
    assert!(
        SystemRunner
            .run(
                Path::new("/definitely/not/here"),
                &[],
                Duration::from_secs(1)
            )
            .is_err()
    );
}

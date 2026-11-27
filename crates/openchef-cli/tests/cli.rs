//! Ported from `internal/cli/doctor_init_test.go` and `cli_test.go` (Go reference), run
//! through `openchef_cli::run` with fake agent CLIs and a temporary home.

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use openchef_cli::{Deps, exit, run};
use openchef_config::{CommandOutput, Config, Detector, Paths, PlanTier, Provider, Runner};

/// Fake CLIs: `installed` are on PATH; `logged_in` report a login.
struct FakeCli {
    logged_in: Vec<&'static str>,
}

impl Runner for FakeCli {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<CommandOutput, String> {
        let bin = program.file_name().unwrap().to_string_lossy().into_owned();
        let logged_in = self.logged_in.contains(&bin.as_str());
        let ok = |s: &str| {
            Ok(CommandOutput {
                stdout: s.into(),
                stderr: Vec::new(),
                code: 0,
            })
        };
        let fail = |s: &str| {
            Ok(CommandOutput {
                stdout: s.into(),
                stderr: Vec::new(),
                code: 1,
            })
        };
        match args.join(" ").as_str() {
            "--version" => ok("1.0.0"),
            "auth status" if logged_in => ok(r#"{"loggedIn":true,"authMethod":"claude.ai"}"#),
            "auth status" => fail(r#"{"loggedIn":false}"#),
            "login status" if logged_in => ok("Logged in using ChatGPT"),
            "login status" => fail("Not logged in"),
            other => Err(format!("unexpected command {other}")),
        }
    }
}

struct Env {
    deps: Deps,
    paths: Paths,
    _dir: tempfile::TempDir,
}

fn fake_env(installed: &[&'static str], logged_in: &[&'static str]) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let installed: Vec<&'static str> = installed.to_vec();
    let detector = Detector::new(
        move |bin| {
            installed
                .contains(&bin)
                .then(|| PathBuf::from(format!("/fake/{bin}")))
        },
        FakeCli {
            logged_in: logged_in.to_vec(),
        },
        Duration::from_secs(1),
    );
    let paths = Paths::in_home(&home).unwrap();
    Env {
        deps: Deps {
            detector,
            home: Some(home),
            interactive: false,
            start_daemon: None,
            cwd: None,
            mcp_stdio: None,
            sandbox: Some(std::sync::Arc::new(|| Ok("test sandbox".to_owned()))),
        },
        paths,
        _dir: dir,
    }
}

fn run_with(env: &Env, args: &[&str]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        std::iter::once("openchef").chain(args.iter().copied()),
        &mut out,
        &mut err,
        &env.deps,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn doctor_without_config() {
    let env = fake_env(&["claude", "codex"], &["claude"]);
    let (code, out, _) = run_with(&env, &["doctor"]);
    assert_eq!(code, exit::OK, "{out}");
    for want in [
        "not found — run `openchef init`",
        "claude",
        "logged in (claude.ai)",
        "NOT logged in",
        "not installed",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
}

#[test]
fn doctor_unhealthy_when_nothing_usable() {
    let env = fake_env(&[], &[]);
    let (code, out, _) = run_with(&env, &["doctor"]);
    assert_eq!(code, exit::FAILURE);
    assert!(out.contains("install and log in"), "{out}");
}

#[test]
fn doctor_reports_invalid_config() {
    let env = fake_env(&["claude"], &["claude"]);
    env.paths.ensure_dirs().unwrap();
    std::fs::write(&env.paths.config, "[pools.x]\nprovider = \"acme\"\n").unwrap();
    let (code, out, _) = run_with(&env, &["doctor", "--json"]);
    assert_eq!(code, exit::FAILURE);
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["config"]["status"], "invalid");
    assert_ne!(report["config"]["problems"], serde_json::json!([]));
    assert_eq!(report["healthy"], false);
}

#[test]
fn doctor_json_is_stable() {
    let env = fake_env(&["claude", "codex"], &["claude"]);
    let (_, out, _) = run_with(&env, &["doctor", "--json"]);
    let mut report: serde_json::Value = serde_json::from_str(&out).unwrap();
    // Temporary paths differ per run.
    report["paths"] = serde_json::Value::String("<paths>".to_owned());
    insta::assert_json_snapshot!(report);
}

#[test]
fn init_writes_valid_config_and_doctor_accepts() {
    let env = fake_env(&["claude", "codex", "agy"], &["claude", "codex"]);
    let (code, out, err) = run_with(&env, &["init", "--plan", "claude=max"]);
    assert_eq!(code, exit::OK, "{out}{err}");
    assert!(
        out.contains(&format!("Wrote {}", env.paths.config.display()))
            && out.contains("claude=max"),
        "{out}"
    );
    let cfg = Config::load(&env.paths.config).unwrap();
    assert_eq!(
        cfg.pools.len(),
        3,
        "pools for claude, codex and agy (login unknown is usable)"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&env.paths.config)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let (code, out, _) = run_with(&env, &["doctor"]);
    assert_eq!(code, exit::OK);
    assert!(
        out.contains(&format!("valid ({})", env.paths.config.display())),
        "{out}"
    );
}

#[test]
fn init_skips_unusable_runtimes() {
    let env = fake_env(&["claude", "codex"], &["codex"]);
    let (code, out, _) = run_with(&env, &["init"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("Skipped:  claude"), "{out}");
    let cfg = Config::load(&env.paths.config).unwrap();
    assert!(
        !cfg.agents.contains_key("opus"),
        "agents generated for a logged-out runtime"
    );
}

#[test]
fn init_refuses_overwrite_unless_forced() {
    let env = fake_env(&["claude"], &["claude"]);
    assert_eq!(run_with(&env, &["init"]).0, exit::OK);
    let (code, _, err) = run_with(&env, &["init"]);
    assert_eq!(code, exit::FAILURE);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(
        run_with(&env, &["init", "--force", "--plan", "claude=ultra"]).0,
        exit::OK
    );
    let cfg = Config::load(&env.paths.config).unwrap();
    assert_eq!(
        cfg.pools["claude"].plan_tier,
        Some(PlanTier::Ultra),
        "forced overwrite not applied"
    );
}

#[test]
fn init_dry_run_writes_nothing() {
    let env = fake_env(&["claude"], &["claude"]);
    let (code, out, _) = run_with(&env, &["init", "--dry-run"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("[pools.claude]"), "{out}");
    assert!(!env.paths.config.exists(), "dry run wrote a file");
}

#[test]
fn init_errors() {
    let none = fake_env(&[], &[]);
    let (code, _, err) = run_with(&none, &["init"]);
    assert_eq!(code, exit::FAILURE);
    assert!(err.contains("no usable agent CLI"), "{err}");
    let env = fake_env(&["claude"], &["claude"]);
    for bad in ["cursor=pro", "claude=gold", "claude"] {
        assert_eq!(
            run_with(&env, &["init", "--plan", bad]).0,
            exit::FAILURE,
            "--plan {bad}"
        );
    }
}

#[test]
fn init_records_probe_consent() {
    for (args, want) in [
        (&[][..], true),
        (&["--quota-probes", "off"][..], false),
        (&["--quota-probes", "on"][..], true),
    ] {
        let env = fake_env(&["claude"], &["claude"]);
        let mut all = vec!["init"];
        all.extend_from_slice(args);
        let (code, out, err) = run_with(&env, &all);
        assert_eq!(code, exit::OK, "{args:?}: {err}");
        let cfg = Config::load(&env.paths.config).unwrap();
        assert_eq!(cfg.quota.probes, Some(want), "{args:?}");
        assert_eq!(
            cfg.quota.probes_enabled(Provider::Anthropic),
            want,
            "{args:?}"
        );
        assert!(out.contains("Probes:"), "{args:?}: {out}");
    }
    let env = fake_env(&["claude"], &["claude"]);
    assert_eq!(
        run_with(&env, &["init", "--quota-probes", "maybe"]).0,
        exit::USAGE
    );
}

#[test]
fn version() {
    let env = fake_env(&[], &[]);
    let (code, out, _) = run_with(&env, &["version"]);
    assert_eq!(code, exit::OK);
    assert!(
        out.starts_with(&format!("openchef {}", env!("CARGO_PKG_VERSION"))),
        "{out}"
    );
    let (_, json, _) = run_with(&env, &["version", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert!(v["platform"].as_str().unwrap().contains('/'));
}

#[test]
fn pending_and_unknown_commands() {
    let env = fake_env(&[], &[]);
    let (code, _, err) = run_with(&env, &["report"]);
    assert_eq!(code, exit::USAGE);
    assert!(err.contains("not implemented yet (R7)"), "{err}");
    let (code, _, err) = run_with(&env, &["frobnicate"]);
    assert_eq!(code, exit::USAGE);
    assert!(err.contains("unknown command"), "{err}");
}

#[test]
fn help_without_command() {
    let env = fake_env(&[], &[]);
    let (code, out, _) = run_with(&env, &[]);
    assert_eq!(code, exit::OK);
    assert!(
        out.contains("Usage: openchef") && out.contains("doctor") && out.contains("init"),
        "{out}"
    );
}

#[test]
fn binary_runs() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_openchef"))
        .arg("version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("openchef ")
    );
}

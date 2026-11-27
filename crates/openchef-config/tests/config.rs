//! Ported from `internal/config/config_test.go` (Go reference, tag `go-reference`).

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::collections::BTreeMap;

use openchef_config::{
    Config, DEFAULT_MAX_CONCURRENT, EffortProfile, Error, HOME_ENV, Paths, Phase, PlanTier,
    Provider, Runtime, Strategy, default_config,
};

const VALID: &str = r#"
[pools.claude]
provider = "anthropic"
billing  = "subscription"
plan_tier = "max"

[pools.api]
provider    = "openai"
billing     = "api"
api_key_env = "OPENAI_API_KEY"
effort_profile = "economy"
max_concurrent = 4

[agents.opus]
runtime = "claude"
model   = "opus"
pool    = "claude"
tier    = "frontier"
effort  = "high"

[agents.gpt]
runtime = "codex"
pool    = "api"
tier    = "frontier"

[routing.implement]
strategy = "weighted"
agents   = ["opus", "gpt"]
weights  = { opus = 60, gpt = 40 }

[routing.review]
agents = ["gpt", "opus"]
"#;

fn problems(err: Error) -> Vec<String> {
    match err {
        Error::Invalid { problems, .. } => problems,
        other => panic!("want Error::Invalid, got {other:?}"),
    }
}

#[test]
fn parse_valid_applies_defaults() {
    let cfg = Config::parse(VALID).unwrap();
    let claude = &cfg.pools["claude"];
    assert_eq!(claude.max_concurrent, DEFAULT_MAX_CONCURRENT);
    assert_eq!(claude.effort_profile, EffortProfile::Auto);
    let api = &cfg.pools["api"];
    assert_eq!(
        (api.max_concurrent, api.effort_profile),
        (4, EffortProfile::Economy)
    );
    assert_eq!(cfg.routing[&Phase::Review].strategy, Strategy::Auto);
    assert_eq!(cfg.routing[&Phase::Implement].weights["opus"], 60);
}

#[test]
fn parse_rejects_unknown_keys_with_position() {
    let raw = format!(
        "{VALID}\n[agents.opus2]\nruntime = \"claude\"\npool = \"claude\"\ntier = \"fast\"\nmodel_id = \"haiku\"\n"
    );
    let err = Config::parse(&raw).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(r#"unknown key "agents.opus2.model_id""#) && msg.contains("line "),
        "{msg}"
    );
    insta::assert_snapshot!(msg, @r#"
    invalid configuration:
      - line 38, column 1: unknown key "agents.opus2.model_id"
    "#);
}

#[test]
fn parse_reports_every_unknown_key() {
    let raw = format!("{VALID}\n[quota]\nprobe = true\n[agents.gpt2]\nruntme = \"codex\"\n");
    assert_eq!(problems(Config::parse(&raw).unwrap_err()).len(), 2);
}

#[test]
fn parse_syntax_error_has_position() {
    let err = Config::parse("[pools.claude\nprovider = 1").unwrap_err();
    assert!(matches!(err, Error::Syntax { line: 1, .. }), "{err:?}");
}

#[test]
fn wrong_value_type_has_position() {
    let err = Config::parse("[pools.p]\nprovider = 1\n").unwrap_err();
    assert!(matches!(err, Error::Syntax { line: 2, .. }), "{err:?}");
}

#[test]
fn validate_reports_every_problem() {
    let raw = r#"
[pools.p]
provider = "acme"
billing  = "api"
plan_tier = "gold"
effort_profile = "turbo"

[pools.q]
provider = "google"
billing  = "subscription"
api_key_env = "X"

[agents.a]
runtime = "cursor"
pool    = "missing"
tier    = "ultra"
effort  = "xhigh"

[routing.deploy]
agents = []

[routing.implement]
strategy = "weighted"
agents = ["a", "a", "ghost"]
weights = { a = 0, other = 3 }

[routing.review]
agents = ["a"]
weights = { a = 1 }
"#;
    let got = problems(Config::parse(raw).unwrap_err());
    let want = [
        r#"pools.p.provider: "acme""#,
        "pools.p.api_key_env: required for api billing",
        r#"pools.p.plan_tier: "gold""#,
        r#"pools.p.effort_profile: "turbo""#,
        "pools.q.api_key_env: only allowed",
        r#"agents.a.runtime: "cursor""#,
        r#"agents.a.tier: "ultra""#,
        r#"agents.a.effort: "xhigh""#,
        r#"agents.a.pool: unknown pool "missing""#,
        r#"routing.deploy: "deploy" is not one of"#,
        "routing.deploy.agents: at least one agent",
        r#"routing.implement.agents: "a" listed twice"#,
        r#"routing.implement.agents: unknown agent "ghost""#,
        "routing.implement.weights.a: must be a positive integer",
        r#"routing.implement.weights: "other" is not in routing.implement.agents"#,
        r#"routing.review.weights: only used with strategy = "weighted""#,
    ];
    for fragment in want {
        assert!(
            got.iter().any(|p| p.contains(fragment)),
            "missing {fragment:?} in {got:#?}"
        );
    }
    assert_eq!(got.len(), want.len(), "{got:#?}");
}

#[test]
fn validate_empty() {
    let got = problems(Config::parse("").unwrap_err()).join("\n");
    assert!(
        got.contains("at least one pool") && got.contains("at least one agent"),
        "{got}"
    );
}

#[test]
fn weighted_needs_weights() {
    let raw = VALID.replacen("weights  = { opus = 60, gpt = 40 }", "", 1);
    let got = problems(Config::parse(&raw).unwrap_err()).join("\n");
    assert!(got.contains("weights: required"), "{got}");
}

#[test]
fn load() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    assert!(matches!(
        Config::load(&missing),
        Err(Error::NotFound { .. })
    ));
    let path = dir.path().join("config.toml");
    std::fs::write(&path, VALID).unwrap();
    let _cfg = Config::load(&path).unwrap();
    std::fs::write(&path, "[pools.x]\nprovider = \"acme\"").unwrap();
    let err = Config::load(&path).unwrap_err();
    assert!(
        err.to_string().contains(&path.display().to_string()),
        "error should name the file: {err}"
    );
}

#[test]
fn default_round_trips() {
    let cases: [(&str, &[Runtime]); 4] = [
        ("all", &[Runtime::Claude, Runtime::Codex, Runtime::Agy]),
        ("claude only", &[Runtime::Claude]),
        ("codex only", &[Runtime::Codex]),
        ("agy only", &[Runtime::Agy]),
    ];
    let tiers = BTreeMap::from([(Runtime::Claude, PlanTier::Max)]);
    for (name, runtimes) in cases {
        let cfg = default_config(runtimes, &tiers);
        let text = cfg.to_toml().unwrap();
        assert!(
            !text.contains("[pools]\n"),
            "{name}: redundant parent table header:\n{text}"
        );
        let back = Config::parse(&text)
            .unwrap_or_else(|e| panic!("{name}: generated config does not validate: {e}\n{text}"));
        assert_eq!(back, cfg, "{name}: round trip");
        for phase in Phase::ALL {
            assert!(
                !back.routing[phase].agents.is_empty(),
                "{name}: phase {phase} has no agents"
            );
        }
    }
}

#[test]
fn default_content() {
    let cfg = default_config(
        &[Runtime::Claude, Runtime::Codex],
        &BTreeMap::from([(Runtime::Claude, PlanTier::Max)]),
    );
    assert_eq!(cfg.pools["claude"].plan_tier, Some(PlanTier::Max));
    assert_eq!(cfg.pools["chatgpt"].plan_tier, Some(PlanTier::Plus));
    assert!(
        !cfg.agents.contains_key("gemini"),
        "agy agent generated although agy is not installed"
    );
    assert_eq!(cfg.routing[&Phase::Review].agents, ["gpt", "opus"]);
    assert_eq!(cfg.routing[&Phase::Helper].agents, ["haiku", "gpt"]);
}

#[test]
fn default_toml_is_stable() {
    let cfg = default_config(
        &[Runtime::Claude, Runtime::Codex, Runtime::Agy],
        &BTreeMap::new(),
    );
    insta::assert_snapshot!(cfg.to_toml().unwrap());
}

#[test]
fn paths() {
    let dir = tempfile::tempdir().unwrap();
    let p = Paths::in_home(dir.path()).unwrap();
    assert_eq!(p.home, dir.path());
    assert_eq!(p.config, dir.path().join("config.toml"));
    assert_eq!(p.state, dir.path().join("state.db"));
    assert!(matches!(
        Paths::in_home(std::path::Path::new("")),
        Err(Error::NoHome)
    ));
    // HOME_ENV is read by `Paths::resolve`; setting it here would race other tests.
    assert_eq!(HOME_ENV, "OPENCHEF_HOME");
}

#[test]
fn ensure_dirs_and_save_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let p = Paths::in_home(&dir.path().join("home")).unwrap();
    p.ensure_dirs().unwrap();
    let cfg = Config::parse(VALID).unwrap();
    cfg.save(&p.config).unwrap();
    assert_eq!(Config::load(&p.config).unwrap(), cfg);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p.home), 0o700);
        assert_eq!(mode(&p.config), 0o600);
    }
}

#[test]
fn quota_switches() {
    let base = "[pools.p]\nprovider = \"anthropic\"\nbilling = \"subscription\"\n[agents.a]\nruntime = \"claude\"\npool = \"p\"\ntier = \"frontier\"\n";
    let cfg = Config::parse(base).unwrap();
    assert!(
        cfg.quota.probes_enabled(Provider::Anthropic),
        "probes are on by default (§10.2)"
    );
    let cfg = Config::parse(&format!(
        "{base}[quota]\nprobes = false\n[quota.providers]\nanthropic = true\n"
    ))
    .unwrap();
    assert!(
        !cfg.quota.probes_enabled(Provider::Anthropic),
        "global off must win"
    );
    let cfg = Config::parse(&format!("{base}[quota.providers]\nopenai = false\n")).unwrap();
    assert!(
        !cfg.quota.probes_enabled(Provider::OpenAi)
            && cfg.quota.probes_enabled(Provider::Anthropic)
    );
    assert!(
        Config::parse(&format!("{base}[quota.providers]\nmistral = false\n")).is_err(),
        "unknown provider accepted"
    );
}

#[test]
fn budgets() {
    let agent = "[agents.a]\nruntime = \"claude\"\npool = \"p\"\ntier = \"frontier\"\n";
    let ok = format!(
        "[pools.p]\nprovider = \"anthropic\"\nbilling = \"api\"\napi_key_env = \"ANTHROPIC_API_KEY\"\ndaily_budget_usd = 5\nmonthly_budget_usd = 50\n{agent}"
    );
    let cfg = Config::parse(&ok).unwrap();
    assert_eq!(
        (
            cfg.pools["p"].daily_budget_usd,
            cfg.pools["p"].monthly_budget_usd
        ),
        (Some(5.0), Some(50.0))
    );
    for bad in [
        format!(
            "[pools.p]\nprovider = \"anthropic\"\nbilling = \"subscription\"\ndaily_budget_usd = 5\n{agent}"
        ),
        format!(
            "[pools.p]\nprovider = \"anthropic\"\nbilling = \"api\"\napi_key_env = \"K\"\nmonthly_budget_usd = -1\n{agent}"
        ),
    ] {
        assert!(Config::parse(&bad).is_err(), "accepted:\n{bad}");
    }
}

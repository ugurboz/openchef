//! Maps a session spec to Claude Code flags (D-006, D-007, D-008, D-011, D-013). Pure,
//! so the mapping can be tested exhaustively.

use openchef_core::PermissionMode;
use serde_json::{Value, json};

use crate::{INVARIANT_DENY_RULES, McpServer, SessionSpec};

/// Tools every session gets. Subagents (`Task`) and tools that act outside the turn
/// (cron, remote triggers, notifications, monitors, worktrees, messaging) never are.
const DEFAULT_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "NotebookEdit",
    "TodoWrite",
];
const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep"];
const WEB_TOOLS: &[&str] = &["WebFetch", "WebSearch"];

/// One process start.
#[derive(Debug, Clone, Default)]
pub(crate) struct Launch<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) resume: bool,
    pub(crate) schema: Option<&'a Value>,
    pub(crate) mcp_config: Option<&'a str>,
}

/// D-008: `--restricted` refuses `bypassPermissions`, and Claude's own plan mode switches
/// models, so OpenChef's plan mode is `dontAsk` with read-only tools; bypass becomes
/// `acceptEdits` with an approver that allows what rules and invariants do not deny.
pub(crate) fn mode_and_tools(spec: &SessionSpec) -> (&'static str, Vec<&'static str>) {
    let mut tools = DEFAULT_TOOLS.to_vec();
    if spec.web_access {
        tools.extend_from_slice(WEB_TOOLS);
    }
    match spec.mode {
        PermissionMode::Manual => ("manual", tools),
        PermissionMode::AcceptEdits | PermissionMode::BypassPermissions => ("acceptEdits", tools),
        PermissionMode::Auto => ("auto", tools),
        PermissionMode::DontAsk => ("dontAsk", tools),
        PermissionMode::Plan => ("dontAsk", READ_ONLY_TOOLS.to_vec()),
    }
}

pub(crate) fn ask_user_tool(m: &McpServer) -> String {
    format!("mcp__{}__ask_user", m.name)
}

fn dedupe<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for s in items {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// The `--settings` payload: native deny rules (spec + invariants), native ask rules,
/// and an allow rule for OpenChef's own `ask_user` so it works even in `dontAsk` (D-007).
pub(crate) fn settings_json(spec: &SessionSpec) -> String {
    let deny = dedupe(
        INVARIANT_DENY_RULES
            .iter()
            .copied()
            .chain(spec.deny_rules.iter().map(String::as_str)),
    );
    let mut perms = json!({ "deny": deny });
    if !spec.ask_rules.is_empty() {
        perms["ask"] = json!(dedupe(spec.ask_rules.iter().map(String::as_str)));
    }
    if let Some(m) = &spec.ask_user {
        perms["allow"] = json!([ask_user_tool(m)]);
    }
    json!({ "permissions": perms }).to_string()
}

/// The `--mcp-config` file content for OpenChef's `ask_user` server.
pub(crate) fn mcp_config_json(m: &McpServer) -> String {
    let mut server = json!({ "command": m.command.to_string_lossy(), "args": m.args });
    if !m.env.is_empty() {
        let env: serde_json::Map<String, Value> =
            m.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        server["env"] = Value::Object(env);
    }
    json!({ "mcpServers": { m.name.clone(): server } }).to_string()
}

/// The full argument list after the program name.
pub(crate) fn build_args(spec: &SessionSpec, launch: &Launch<'_>) -> Vec<String> {
    let (mode, tools) = mode_and_tools(spec);
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--restricted",
        "--strict-mcp-config",
        "--permission-prompt-tool",
        "stdio",
        "--disallowedTools",
        "AskUserQuestion",
        "--permission-mode",
        mode,
        "--tools",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(tools.join(","));
    if let Some(model) = &spec.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = spec.effort {
        // Claude accepts low|medium|high|xhigh|max, a superset of the normalized scale.
        args.extend(["--effort".to_owned(), effort.to_string()]);
    }
    args.extend(["--settings".to_owned(), settings_json(spec)]);
    if let Some(path) = launch.mcp_config {
        args.extend(["--mcp-config".to_owned(), path.to_owned()]);
    }
    let identity = if launch.resume {
        "--resume"
    } else {
        "--session-id"
    };
    args.extend([identity.to_owned(), launch.session_id.to_owned()]);
    if let Some(schema) = launch.schema {
        args.extend(["--json-schema".to_owned(), schema.to_string()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use openchef_core::{Effort, PermissionMode};
    use serde_json::{Value, json};

    use super::{Launch, build_args, mcp_config_json, mode_and_tools, settings_json};
    use crate::{
        ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, INVARIANT_DENY_RULES,
        McpServer, Question, SessionSpec,
    };

    struct Never;
    impl Approver for Never {
        fn approve(
            &self,
            _: ApprovalRequest,
            _: tokio_util::sync::CancellationToken,
        ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
            Box::pin(async { Ok(Decision::deny("test")) })
        }
        fn answer(
            &self,
            _: Question,
            _: tokio_util::sync::CancellationToken,
        ) -> BoxFuture<'static, Result<String, ApproverError>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    pub(crate) fn spec() -> SessionSpec {
        SessionSpec {
            dir: PathBuf::from("/w"),
            model: None,
            effort: None,
            mode: PermissionMode::Manual,
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            web_access: false,
            env: Vec::new(),
            approver: Arc::new(Never),
            ask_user: None,
            log_path: None,
            on_spawn: None,
        }
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }

    #[test]
    fn mode_mapping() {
        let cases = [
            (PermissionMode::Manual, "manual", 8),
            (PermissionMode::AcceptEdits, "acceptEdits", 8),
            (PermissionMode::BypassPermissions, "acceptEdits", 8),
            (PermissionMode::Auto, "auto", 8),
            (PermissionMode::DontAsk, "dontAsk", 8),
            (PermissionMode::Plan, "dontAsk", 3),
        ];
        for (mode, want, tools) in cases {
            let s = SessionSpec { mode, ..spec() };
            let (got, t) = mode_and_tools(&s);
            assert_eq!((got, t.len()), (want, tools), "{mode}");
        }
        let web = SessionSpec {
            web_access: true,
            ..spec()
        };
        assert!(mode_and_tools(&web).1.contains(&"WebFetch"));
        assert!(
            !mode_and_tools(&spec()).1.contains(&"WebFetch"),
            "web tools are off by default (D-006)"
        );
        assert!(
            !mode_and_tools(&spec()).1.contains(&"Task"),
            "subagents are never enabled"
        );
    }

    #[test]
    fn args_new_and_resume() {
        let s = SessionSpec {
            model: Some("haiku".into()),
            effort: Some(Effort::Low),
            ..spec()
        };
        let a = build_args(
            &s,
            &Launch {
                session_id: "sid",
                ..Launch::default()
            },
        );
        for f in [
            "--restricted",
            "--strict-mcp-config",
            "--verbose",
            "--include-partial-messages",
        ] {
            assert!(a.iter().any(|x| x == f), "{f} missing");
        }
        assert_eq!(
            flag(&a, "--permission-prompt-tool").as_deref(),
            Some("stdio")
        );
        assert_eq!(
            flag(&a, "--disallowedTools").as_deref(),
            Some("AskUserQuestion")
        );
        assert_eq!(flag(&a, "--session-id").as_deref(), Some("sid"));
        assert_eq!(flag(&a, "--model").as_deref(), Some("haiku"));
        assert_eq!(flag(&a, "--effort").as_deref(), Some("low"));
        assert!(flag(&a, "--resume").is_none() && flag(&a, "--json-schema").is_none());
        let schema = json!({"type": "object"});
        let r = build_args(
            &spec(),
            &Launch {
                session_id: "sid",
                resume: true,
                schema: Some(&schema),
                mcp_config: Some("/t/mcp.json"),
            },
        );
        assert_eq!(flag(&r, "--resume").as_deref(), Some("sid"));
        assert!(flag(&r, "--session-id").is_none());
        assert_eq!(
            flag(&r, "--json-schema").as_deref(),
            Some(r#"{"type":"object"}"#)
        );
        assert_eq!(flag(&r, "--mcp-config").as_deref(), Some("/t/mcp.json"));
        assert!(
            flag(&r, "--model").is_none() && flag(&r, "--effort").is_none(),
            "defaults are left to Claude"
        );
    }

    #[test]
    fn settings_carry_rules_and_ask_user() {
        let s = SessionSpec {
            deny_rules: vec!["Read(./.env)".into(), "Bash(git push:*)".into()],
            ask_rules: vec!["Bash(npm publish:*)".into(), "Bash(npm publish:*)".into()],
            ask_user: Some(McpServer {
                name: "openchef".into(),
                command: "openchef".into(),
                args: vec!["mcp".into()],
                env: vec![("A".into(), "1".into())],
                tool_timeout: None,
            }),
            ..spec()
        };
        let v: Value = serde_json::from_str(&settings_json(&s)).unwrap();
        let deny: Vec<&str> = v["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        for rule in INVARIANT_DENY_RULES {
            assert!(deny.contains(rule), "invariant {rule} missing");
        }
        assert!(deny.contains(&"Read(./.env)"));
        assert_eq!(
            deny.iter().filter(|r| **r == "Bash(git push:*)").count(),
            1,
            "deduplicated"
        );
        assert_eq!(v["permissions"]["ask"], json!(["Bash(npm publish:*)"]));
        assert_eq!(
            v["permissions"]["allow"],
            json!(["mcp__openchef__ask_user"])
        );
        let mcp: Value =
            serde_json::from_str(&mcp_config_json(s.ask_user.as_ref().unwrap())).unwrap();
        assert_eq!(mcp["mcpServers"]["openchef"]["command"], "openchef");
        assert_eq!(mcp["mcpServers"]["openchef"]["env"]["A"], "1");
        let plain: Value = serde_json::from_str(&settings_json(&spec())).unwrap();
        assert!(
            plain["permissions"].get("ask").is_none()
                && plain["permissions"].get("allow").is_none()
        );
    }
}

//! Wire types of `agy acp` (Agent Client Protocol), as recorded with agy 1.5.16
//! (m1-findings §11, `testdata/agy`). Unknown fields are ignored.

use openchef_core::Effort;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ApprovalRequest, FailureKind, McpServer, RequestKind, SessionRef, ToolResult, Usage,
    nearest_effort,
};

const MAX_TOOL_OUTPUT: usize = 4 << 10;

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct OptionValue {
    #[serde(default)]
    pub(crate) value: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ConfigOption {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, rename = "currentValue")]
    pub(crate) current_value: String,
    #[serde(default)]
    pub(crate) options: Vec<OptionValue>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SessionResult {
    #[serde(default, rename = "sessionId")]
    pub(crate) session_id: String,
    #[serde(default, rename = "configOptions")]
    pub(crate) config_options: Vec<ConfigOption>,
}

pub(crate) fn find_option<'a>(opts: &'a [ConfigOption], id: &str) -> Option<&'a ConfigOption> {
    opts.iter().find(|o| o.id == id)
}

/// agy's thinking levels on the normalized scale; `xhigh` is agy's highest.
fn thought_effort(value: &str) -> Option<Effort> {
    match value {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" | "max" => Some(Effort::Max),
        _ => None,
    }
}

/// The `thought_level` for `effort` among the values the current model offers; `None`
/// leaves agy's default.
pub(crate) fn thought_level(effort: Option<Effort>, opts: &[ConfigOption]) -> Option<String> {
    let option = find_option(opts, "thought_level")?;
    effort?;
    let mut offered: Vec<(Effort, &str)> = Vec::new();
    for v in &option.options {
        if let Some(e) = thought_effort(&v.value) {
            match offered.iter_mut().find(|(x, _)| *x == e) {
                Some(slot) => slot.1 = &v.value, // the last spelling wins, as in Go
                None => offered.push((e, &v.value)),
            }
        }
    }
    let supported: Vec<Effort> = offered.iter().map(|(e, _)| *e).collect();
    let chosen = nearest_effort(effort, &supported)?;
    offered
        .iter()
        .find(|(e, _)| *e == chosen)
        .map(|(_, v)| (*v).to_owned())
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[allow(clippy::struct_field_names, reason = "the wire names")]
pub(crate) struct PromptUsage {
    #[serde(default, rename = "inputTokens")]
    input_tokens: u64,
    #[serde(default, rename = "outputTokens")]
    output_tokens: u64,
    #[serde(default, rename = "cachedReadTokens")]
    cached_read_tokens: u64,
    #[serde(default, rename = "cachedWriteTokens")]
    cached_write_tokens: u64,
    #[serde(default, rename = "thoughtTokens")]
    thought_tokens: u64,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct PromptResult {
    #[serde(default, rename = "stopReason")]
    pub(crate) stop_reason: String,
    #[serde(default)]
    usage: Option<PromptUsage>,
}

impl PromptResult {
    /// Per turn (m1-findings §11). Thought tokens are billed as output.
    pub(crate) fn usage(&self) -> Usage {
        self.usage.map_or_else(Usage::default, |u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens + u.thought_tokens,
            cache_read_tokens: u.cached_read_tokens,
            cache_write_tokens: u.cached_write_tokens,
            cost_usd: None,
        })
    }
}

pub(crate) fn add_usage(a: &Usage, b: &Usage) -> Usage {
    let cost = match (a.cost_usd, b.cost_usd) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0.0) + y.unwrap_or(0.0)),
    };
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read_tokens: a.cache_read_tokens + b.cache_read_tokens,
        cache_write_tokens: a.cache_write_tokens + b.cache_write_tokens,
        cost_usd: cost,
    }
}

/// Maps a stop reason; `None` for `end_turn`. OpenChef's own reason for stopping wins
/// over a generic `cancelled`.
pub(crate) fn stop_failure(reason: &str, stop: Option<FailureKind>) -> Option<FailureKind> {
    match reason {
        "end_turn" => None,
        "cancelled" => Some(stop.unwrap_or(FailureKind::Interrupted)),
        _ => Some(FailureKind::Unknown), // max_tokens, max_turn_requests, refusal, unknown
    }
}

/// Classifies a failed `session/prompt` by message; agy's quota error shape is not
/// recorded yet (m1-findings §8).
pub(crate) fn classify_error(message: &str) -> FailureKind {
    let low = message.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| low.contains(w));
    if has(&["resource_exhausted", "quota", "usage limit", "credits"]) {
        FailureKind::QuotaExhausted
    } else if has(&["rate limit", "429", "overloaded", "unavailable"]) {
        FailureKind::RateLimited
    } else if has(&[
        "unauthenticated",
        "login",
        "credential",
        "permission_denied",
        "auth",
    ]) {
        FailureKind::Auth
    } else if has(&["context window", "too long", "token limit"]) {
        FailureKind::ContextOverflow
    } else {
        FailureKind::Unknown
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct Location {
    #[serde(default)]
    pub(crate) path: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct Update {
    #[serde(default, rename = "sessionUpdate")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) content: Value,
    #[serde(default, rename = "toolCallId")]
    pub(crate) tool_call_id: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default, rename = "kind")]
    pub(crate) tool_kind: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default, rename = "rawInput")]
    pub(crate) raw_input: Value,
    #[serde(default, rename = "rawOutput")]
    pub(crate) raw_output: Value,
    #[serde(default)]
    pub(crate) locations: Option<Vec<Location>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UpdateParams {
    #[serde(default, rename = "sessionId")]
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) update: Update,
}

impl Update {
    /// The text of an `agent_message_chunk`.
    pub(crate) fn chunk_text(&self) -> &str {
        if self.content["type"] == "text" {
            self.content["text"].as_str().unwrap_or("")
        } else {
            ""
        }
    }

    /// A finished tool call; a command that exited non-zero is an error.
    pub(crate) fn result(&self) -> ToolResult {
        let exit_code = self.raw_output["exit_code"].as_i64();
        let mut text = match &self.raw_output {
            Value::Object(o)
                if o.get("output")
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                    || exit_code.is_some() =>
            {
                self.raw_output["output"].as_str().unwrap_or("").to_owned()
            }
            Value::String(s) => s.clone(),
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_) => String::new(),
        };
        if text.is_empty() {
            text = content_text(&self.content);
        }
        ToolResult {
            id: self.tool_call_id.clone(),
            is_error: self.status == "failed" || exit_code.is_some_and(|c| c != 0),
            output: truncate(&text),
        }
    }
}

fn content_text(raw: &Value) -> String {
    let Some(blocks) = raw.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for b in blocks {
        if let Some(t) = b["content"]["text"].as_str().filter(|t| !t.is_empty()) {
            parts.push(t.to_owned());
        } else if b["type"] == "diff"
            && let Some(p) = b["path"].as_str().filter(|p| !p.is_empty())
        {
            parts.push(format!("edited {p}"));
        }
    }
    parts.join("\n")
}

pub(crate) fn truncate(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s.to_owned();
    }
    let half = MAX_TOOL_OUTPUT / 2;
    let head_end = (0..=half)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    let tail_start = (s.len() - half..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("{}\n…\n{}", &s[..head_end], &s[tail_start..])
}

/// agy tool names in Claude Code vocabulary, so permission rules apply uniformly (§9.1).
fn known_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "run_command" => "Bash",
        "write_file" | "write_to_file" => "Write",
        "replace_file_content" | "multi_replace_file_content" | "edit_file" => "Edit",
        "view_file" | "read_file" => "Read",
        "list_dir" | "find_by_name" => "Glob",
        "grep_search" => "Grep",
        "read_url_content" => "WebFetch",
        "search_web" => "WebSearch",
        _ => return None,
    })
}

/// Maps an agy tool to OpenChef's vocabulary; `ask_user` names OpenChef's MCP server.
/// MCP tools appear as `mcp_<server>_<tool>`.
pub(crate) fn tool_name(name: &str, kind: &str, ask_user: Option<&str>) -> String {
    if let Some(n) = known_tool(name) {
        return n.to_owned();
    }
    if let Some(rest) = name.strip_prefix("mcp_") {
        if let Some(server) = ask_user
            && let Some(tool) = rest.strip_prefix(&format!("{server}_"))
        {
            return format!("mcp__{server}__{tool}");
        }
        return match rest.find('_') {
            Some(i) if i > 0 => format!("mcp__{}__{}", &rest[..i], &rest[i + 1..]),
            _ => format!("mcp__{rest}"),
        };
    }
    match kind {
        "execute" => "Bash".to_owned(),
        "edit" | "delete" | "move" => "Edit".to_owned(),
        "read" => "Read".to_owned(),
        "fetch" => "WebFetch".to_owned(),
        _ if !name.is_empty() => name.to_owned(),
        _ => kind.to_owned(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PermissionOption {
    #[serde(default, rename = "optionId")]
    option_id: String,
    #[serde(default)]
    kind: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PermissionParams {
    #[serde(default, rename = "toolCall")]
    pub(crate) tool_call: Update,
    #[serde(default)]
    options: Vec<PermissionOption>,
}

impl PermissionParams {
    /// The id of the first option of `kind` (D-016: ids changed between agy versions,
    /// kinds did not).
    pub(crate) fn option_id(&self, kind: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| o.option_id.as_str())
    }

    /// The normalized request.
    pub(crate) fn approval_request(
        &self,
        session: SessionRef,
        raw: &Value,
        ask_user: Option<&str>,
    ) -> ApprovalRequest {
        let tc = &self.tool_call;
        let tool = tool_name(&tc.name, &tc.tool_kind, ask_user);
        let input = &tc.raw_input;
        let field = |k: &str| input[k].as_str().unwrap_or("").to_owned();
        let mut description = field("toolSummary");
        let (kind, command) = if tc.tool_kind == "execute" || tool == "Bash" {
            let mut command = field("CommandLine");
            if command.is_empty() {
                command.clone_from(&tc.title);
            }
            if input["BypassSandbox"] == true {
                // D-015: the command wants to leave the read-only, offline sandbox.
                format!("{description} (runs outside the agy sandbox)")
                    .trim()
                    .clone_into(&mut description);
            }
            (RequestKind::Command, Some(command))
        } else if tool == "Write" || tool == "Edit" {
            (RequestKind::FileChange, None)
        } else {
            (RequestKind::Tool, None)
        };
        let mut paths: Vec<String> = Vec::new();
        let locations = tc.locations.iter().flatten().map(|l| l.path.clone());
        for p in [field("Path"), field("TargetFile"), field("AbsolutePath")]
            .into_iter()
            .chain(locations)
        {
            if !p.is_empty() && !paths.contains(&p) {
                paths.push(p);
            }
        }
        ApprovalRequest {
            session,
            kind,
            tool,
            command,
            paths,
            description,
            input: input.clone(),
            raw: raw.clone(),
        }
    }
}

pub(crate) fn selected(id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": id } })
}

pub(crate) fn cancelled() -> Value {
    json!({ "outcome": { "outcome": "cancelled" } })
}

/// A tool call's identity for loop detection, without prose that changes between
/// identical calls.
pub(crate) fn signature(name: &str, input: &Value) -> String {
    let mut v = input.clone();
    if let Value::Object(m) = &mut v {
        drop(m.remove("toolSummary"));
        drop(m.remove("WaitMsBeforeAsync"));
    }
    // serde_json's map keeps keys sorted, so re-encoding canonicalizes.
    format!("{name}:{v}")
}

/// The `session/new` and `session/load` parameter (ACP stdio servers; env as
/// name/value pairs).
pub(crate) fn mcp_servers(m: Option<&McpServer>) -> Value {
    let Some(m) = m else { return json!([]) };
    let mut env: Vec<&(String, String)> = m.env.iter().collect();
    env.sort_by(|a, b| a.0.cmp(&b.0));
    let env: Vec<Value> = env
        .into_iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();
    json!([{ "name": m.name, "command": m.command.to_string_lossy(), "args": m.args, "env": env }])
}

#[cfg(test)]
mod tests {
    use openchef_core::Effort;
    use serde_json::{Value, json};

    use super::{
        ConfigOption, OptionValue, PermissionParams, PromptResult, Update, add_usage,
        classify_error, mcp_servers, signature, stop_failure, thought_level, tool_name, truncate,
    };
    use crate::{FailureKind, McpServer, RequestKind, SessionRef, Usage};

    fn opts(levels: &[&str]) -> Vec<ConfigOption> {
        let levels = levels
            .iter()
            .map(|l| OptionValue {
                value: (*l).to_owned(),
            })
            .collect();
        vec![
            ConfigOption {
                id: "model".into(),
                current_value: "gemini-3.8-flash".into(),
                options: Vec::new(),
            },
            ConfigOption {
                id: "thought_level".into(),
                current_value: "high".into(),
                options: levels,
            },
        ]
    }

    #[test]
    fn thought_levels() {
        let full = opts(&["low", "medium", "high", "xhigh"]);
        let flash = opts(&["low", "medium", "high"]);
        let cases = [
            (Some(Effort::Low), &full, Some("low")),
            (Some(Effort::Medium), &full, Some("medium")),
            (Some(Effort::High), &full, Some("high")),
            (Some(Effort::Max), &full, Some("xhigh")),
            (Some(Effort::Max), &flash, Some("high")),
            (None, &full, None),
        ];
        for (e, o, want) in cases {
            assert_eq!(thought_level(e, o).as_deref(), want, "{e:?}");
        }
        let model_only = vec![ConfigOption {
            id: "model".into(),
            ..ConfigOption::default()
        }];
        assert_eq!(
            thought_level(Some(Effort::Low), &model_only),
            None,
            "no thinking option"
        );
        assert_eq!(
            thought_level(Some(Effort::Low), &opts(&["medium", "high"])).as_deref(),
            Some("medium"),
            "lowest offered"
        );
    }

    #[test]
    fn usage_and_stop() {
        let r: PromptResult = serde_json::from_value(json!({"stopReason": "end_turn", "usage": {"inputTokens": 2271,
            "outputTokens": 3, "cachedReadTokens": 17962, "cachedWriteTokens": 0, "thoughtTokens": 7, "totalTokens": 20243}}))
        .unwrap();
        let u = r.usage();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (2271, 10, 17962)
        );
        assert_eq!(PromptResult::default().usage(), Usage::default());
        let sum = add_usage(&u, &u);
        assert_eq!(
            (sum.input_tokens, sum.output_tokens, sum.cache_read_tokens),
            (4542, 20, 35924)
        );
        for (reason, want) in [
            ("cancelled", FailureKind::Interrupted),
            ("max_tokens", FailureKind::Unknown),
            ("refusal", FailureKind::Unknown),
            ("???", FailureKind::Unknown),
        ] {
            assert_eq!(stop_failure(reason, None), Some(want), "{reason}");
        }
        assert_eq!(stop_failure("end_turn", None), None);
        assert_eq!(
            stop_failure("cancelled", Some(FailureKind::Timeout)),
            Some(FailureKind::Timeout),
            "our reason wins"
        );
    }

    #[test]
    fn classify_errors() {
        let cases = [
            (
                "json-rpc error -32603: RESOURCE_EXHAUSTED: quota exceeded",
                FailureKind::QuotaExhausted,
            ),
            ("You have no remaining credits", FailureKind::QuotaExhausted),
            ("429 Too Many Requests", FailureKind::RateLimited),
            ("model is overloaded", FailureKind::RateLimited),
            ("UNAUTHENTICATED: please login", FailureKind::Auth),
            (
                "prompt is too long for the context window",
                FailureKind::ContextOverflow,
            ),
            ("something else", FailureKind::Unknown),
        ];
        for (msg, want) in cases {
            assert_eq!(classify_error(msg), want, "{msg}");
        }
    }

    #[test]
    fn tool_names() {
        let cases = [
            ("run_command", "execute", None, "Bash"),
            ("write_file", "edit", None, "Write"),
            ("replace_file_content", "edit", None, "Edit"),
            ("view_file", "read", None, "Read"),
            ("read_url_content", "fetch", None, "WebFetch"),
            (
                "mcp_openchef_ask_user",
                "other",
                Some("openchef"),
                "mcp__openchef__ask_user",
            ),
            (
                "mcp_open_chef_ask_user",
                "other",
                Some("open_chef"),
                "mcp__open_chef__ask_user",
            ),
            (
                "mcp_github_list_prs",
                "other",
                Some("openchef"),
                "mcp__github__list_prs",
            ),
            ("brand_new_exec", "execute", None, "Bash"),
            ("brand_new", "other", None, "brand_new"),
            ("", "think", None, "think"),
        ];
        for (name, kind, ask, want) in cases {
            assert_eq!(tool_name(name, kind, ask), want, "{name}/{kind}");
        }
    }

    #[test]
    fn approval_request_normalization() {
        let parse = |v: Value| serde_json::from_value::<PermissionParams>(v).unwrap();
        let session = || SessionRef {
            runtime: "agy".into(),
            id: "s".into(),
        };
        let bypass = parse(
            json!({"sessionId": "s", "toolCall": {"toolCallId": "c1", "name": "run_command", "kind": "execute",
            "title": "echo a > cmd.txt", "rawInput": {"BypassSandbox": true, "CommandLine": "echo a > cmd.txt", "toolSummary": "Write a to cmd.txt"}},
            "options": [{"kind": "allow_once", "optionId": "allow"}, {"kind": "reject_once", "optionId": "deny"}]}),
        );
        let r = bypass.approval_request(session(), &json!({}), Some("openchef"));
        assert!(
            r.kind == RequestKind::Command
                && r.tool == "Bash"
                && r.command.as_deref() == Some("echo a > cmd.txt"),
            "{r:?}"
        );
        assert!(r.description.contains("outside the agy sandbox"), "{r:?}");
        assert_eq!(
            (
                bypass.option_id("allow_once"),
                bypass.option_id("reject_once"),
                bypass.option_id("allow_always")
            ),
            (Some("allow"), Some("deny"), None)
        );
        let edit = parse(
            json!({"toolCall": {"toolCallId": "c2", "name": "write_file", "kind": "edit", "title": "File edit.txt",
            "rawInput": {"Content": "x", "Path": "/w/edit.txt"}, "locations": [{"path": "/w/edit.txt"}, {"path": "/w/other.txt"}]}}),
        );
        let r = edit.approval_request(session(), &Value::Null, None);
        assert!(
            r.kind == RequestKind::FileChange
                && r.tool == "Write"
                && r.paths == ["/w/edit.txt", "/w/other.txt"],
            "{r:?}"
        );
        let mcp = parse(
            json!({"toolCall": {"name": "mcp_openchef_ask_user", "kind": "other", "title": "Run", "rawInput": {"question": "?"}}}),
        );
        let r = mcp.approval_request(session(), &Value::Null, Some("openchef"));
        assert!(
            r.kind == RequestKind::Tool && r.tool == "mcp__openchef__ask_user",
            "{r:?}"
        );
        let plain = parse(
            json!({"toolCall": {"name": "run_command", "kind": "execute", "title": "ls", "rawInput": {}}}),
        );
        let r = plain.approval_request(session(), &Value::Null, None);
        assert!(
            r.command.as_deref() == Some("ls") && !r.description.contains("sandbox"),
            "{r:?}"
        );
    }

    #[test]
    fn update_parsing() {
        let u = |v: Value| serde_json::from_value::<Update>(v).unwrap();
        assert_eq!(u(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "OK"}})).chunk_text(), "OK");
        assert_eq!(u(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "..."}})).chunk_text(), "");
        let cases = [
            (
                json!({"toolCallId": "c", "status": "completed", "rawOutput": {"exit_code": 0, "output": "fine"}}),
                false,
                "fine",
            ),
            (
                json!({"toolCallId": "c", "status": "completed", "rawOutput": {"exit_code": 1, "output": "zsh: operation not permitted"}}),
                true,
                "zsh: operation not permitted",
            ),
            (
                json!({"toolCallId": "c", "status": "failed", "rawOutput": "permission check failed"}),
                true,
                "permission check failed",
            ),
            (
                json!({"toolCallId": "c", "status": "completed", "rawOutput": {}, "content": [{"type": "diff", "path": "/w/a", "newText": "x"}]}),
                false,
                "edited /w/a",
            ),
            (
                json!({"toolCallId": "c", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "from content"}}]}),
                false,
                "from content",
            ),
        ];
        for (raw, is_error, out) in cases {
            let r = u(raw.clone()).result();
            assert!(
                r.is_error == is_error && r.output == out && r.id == "c",
                "{raw}: {r:?}"
            );
        }
    }

    #[test]
    fn signature_ignores_prose() {
        let a = signature(
            "Bash",
            &json!({"CommandLine": "ls", "toolSummary": "List files", "WaitMsBeforeAsync": 5000}),
        );
        let b = signature(
            "Bash",
            &json!({"toolSummary": "Show the files", "CommandLine": "ls"}),
        );
        assert!(a == b && a.contains(r#""CommandLine":"ls""#), "{a} vs {b}");
    }

    #[test]
    fn mcp_servers_param() {
        assert_eq!(mcp_servers(None), json!([]));
        let m = McpServer {
            name: "openchef".into(),
            command: "/bin/oc".into(),
            args: Vec::new(),
            env: vec![("B".into(), "2".into()), ("A".into(), "1".into())],
            tool_timeout: None,
        };
        assert_eq!(
            mcp_servers(Some(&m)),
            json!([{"args": [], "command": "/bin/oc", "env": [{"name": "A", "value": "1"}, {"name": "B", "value": "2"}], "name": "openchef"}])
        );
    }

    #[test]
    fn truncates() {
        let got = truncate(&"x".repeat(3 * (4 << 10)));
        assert!(
            got.len() <= (4 << 10) + 8 && got.contains('…'),
            "{}",
            got.len()
        );
    }
}

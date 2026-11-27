//! Wire types of the `codex app-server` protocol subset OpenChef uses (verified against
//! codex-cli 0.172.0, m1-findings §1). Unknown fields are ignored; fields OpenChef
//! depends on are checked where they are read.

use std::time::Duration;

use openchef_core::Effort;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{Failure, FailureKind, QuotaSignal, QuotaWindow, ToolCall, ToolResult, Usage};

const MAX_TOOL_OUTPUT: usize = 4 << 10;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Id {
    #[serde(default)]
    pub(crate) id: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ThreadResponse {
    #[serde(default)]
    pub(crate) thread: Id,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnStartResponse {
    #[serde(default)]
    pub(crate) turn: Id,
}

#[derive(Debug, Default, Deserialize)]
struct EffortEntry {
    #[serde(default, rename = "reasoningEffort")]
    reasoning_effort: String,
}

#[derive(Debug, Default, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default, rename = "isDefault")]
    is_default: bool,
    #[serde(default, rename = "supportedReasoningEfforts")]
    efforts: Vec<EffortEntry>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

impl ModelList {
    /// The normalized efforts the model accepts; `None` selects the default model.
    /// Codex levels without a normalized counterpart (`minimal`, `xhigh`) are skipped.
    pub(crate) fn supported_efforts(&self, model: Option<&str>) -> Vec<Effort> {
        let selected = self.data.iter().find(|m| match model {
            None => m.is_default,
            Some(name) => m.id == name || m.model == name,
        });
        selected.map_or_else(Vec::new, |m| {
            m.efforts
                .iter()
                .filter_map(|e| e.reasoning_effort.parse().ok())
                .collect()
        })
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct FileChange {
    #[serde(default)]
    pub(crate) path: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ThreadItem {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) phase: String,
    #[serde(default)]
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default, rename = "exitCode")]
    pub(crate) exit_code: Option<i64>,
    #[serde(default, rename = "aggregatedOutput")]
    pub(crate) aggregated_output: Option<String>,
    #[serde(default)]
    pub(crate) changes: Vec<FileChange>,
    #[serde(default)]
    pub(crate) server: String,
    #[serde(default)]
    pub(crate) tool: String,
    #[serde(default)]
    pub(crate) arguments: Value,
    #[serde(default)]
    pub(crate) error: Value,
}

impl ThreadItem {
    pub(crate) fn paths(&self) -> Vec<String> {
        self.changes.iter().map(|c| c.path.clone()).collect()
    }

    /// The tool call this item starts, in Claude Code vocabulary.
    pub(crate) fn tool_call(&self) -> Option<ToolCall> {
        let (name, input, signature) = match self.kind.as_str() {
            "commandExecution" => (
                "Bash".to_owned(),
                json!({ "command": self.command }),
                format!("Bash:{}", self.command),
            ),
            "fileChange" => {
                let paths = self.paths();
                let signature = format!("Edit:{}", paths.join(","));
                ("Edit".to_owned(), json!({ "paths": paths }), signature)
            }
            "mcpToolCall" => {
                let name = format!("mcp__{}__{}", self.server, self.tool);
                let signature = format!("{name}:{}", self.arguments);
                (name, self.arguments.clone(), signature)
            }
            _ => return None,
        };
        Some(ToolCall {
            id: self.id.clone(),
            name,
            input,
            signature,
        })
    }

    /// The tool result this item completes.
    pub(crate) fn tool_result(&self) -> Option<ToolResult> {
        let not_completed = self.status != "completed";
        match self.kind.as_str() {
            "commandExecution" => Some(ToolResult {
                id: self.id.clone(),
                is_error: not_completed || self.exit_code.is_some_and(|c| c != 0),
                output: truncate(self.aggregated_output.as_deref().unwrap_or("")),
            }),
            "fileChange" | "mcpToolCall" => Some(ToolResult {
                id: self.id.clone(),
                is_error: not_completed || !self.error.is_null(),
                output: String::new(),
            }),
            _ => None,
        }
    }
}

fn truncate(s: &str) -> String {
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

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ItemNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default, rename = "turnId")]
    pub(crate) turn_id: String,
    #[serde(default)]
    pub(crate) item: ThreadItem,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeltaNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default, rename = "turnId")]
    pub(crate) turn_id: String,
    #[serde(default)]
    pub(crate) delta: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_field_names, reason = "the wire names")]
pub(crate) struct TokenBreakdown {
    #[serde(default, rename = "inputTokens")]
    input_tokens: i64,
    #[serde(default, rename = "cachedInputTokens")]
    cached_input_tokens: i64,
    #[serde(default, rename = "cacheWriteInputTokens")]
    cache_write_input_tokens: i64,
    #[serde(default, rename = "outputTokens")]
    output_tokens: i64,
}

impl TokenBreakdown {
    pub(crate) fn minus(self, o: Self) -> Self {
        Self {
            input_tokens: self.input_tokens - o.input_tokens,
            cached_input_tokens: self.cached_input_tokens - o.cached_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens - o.cache_write_input_tokens,
            output_tokens: self.output_tokens - o.output_tokens,
        }
    }

    /// Codex counts cached tokens inside `inputTokens`; OpenChef keeps them apart so
    /// the fields are additive.
    pub(crate) fn usage(self) -> Usage {
        let n = |v: i64| u64::try_from(v.max(0)).unwrap_or(0);
        Usage {
            input_tokens: n(self.input_tokens - self.cached_input_tokens),
            output_tokens: n(self.output_tokens),
            cache_read_tokens: n(self.cached_input_tokens),
            cache_write_tokens: n(self.cache_write_input_tokens),
            cost_usd: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenUsage {
    #[serde(default)]
    pub(crate) last: TokenBreakdown,
    #[serde(default)]
    pub(crate) total: TokenBreakdown,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenUsageNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default, rename = "turnId")]
    pub(crate) turn_id: String,
    #[serde(default, rename = "tokenUsage")]
    pub(crate) token_usage: TokenUsage,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct TurnError {
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default, rename = "additionalDetails")]
    additional_details: Option<String>,
    #[serde(default, rename = "codexErrorInfo")]
    codex_error_info: Value,
}

impl TurnError {
    pub(crate) fn failure(&self) -> Failure {
        let mut message = self.message.clone();
        if let Some(details) = self.additional_details.as_deref().filter(|d| !d.is_empty()) {
            message = format!("{message}: {details}");
        }
        let name = error_info_name(&self.codex_error_info);
        if !name.is_empty() {
            message = format!("{message} [{name}]");
        }
        Failure::new(failure_kind(&self.codex_error_info), message)
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CompletedTurn {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) error: Option<TurnError>,
    #[serde(default, rename = "durationMs")]
    pub(crate) duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnCompletedNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default)]
    pub(crate) turn: CompletedTurn,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnStartedNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default)]
    pub(crate) turn: Id,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ErrorNotification {
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default, rename = "turnId")]
    pub(crate) turn_id: String,
    #[serde(default)]
    pub(crate) error: TurnError,
    #[serde(default, rename = "willRetry")]
    pub(crate) will_retry: bool,
}

/// The name of a `codexErrorInfo`: a string enum, or an object for parameterized
/// variants such as `{"httpConnectionFailed": {...}}`.
fn error_info_name(info: &Value) -> String {
    match info {
        Value::String(s) => s.clone(),
        Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => String::new(),
    }
}

pub(crate) fn failure_kind(info: &Value) -> FailureKind {
    match error_info_name(info).as_str() {
        "usageLimitExceeded"
        | "sessionBudgetExceeded"
        | "apiKeySpendLimitExceeded"
        | "projectSpendLimitExceeded"
        | "organizationSpendLimitExceeded" => FailureKind::QuotaExhausted,
        "rateLimitExceeded"
        | "serverOverloaded"
        | "httpConnectionFailed"
        | "flexUnavailable"
        | "policyCapacityUnavailable"
        | "internalServerError" => FailureKind::RateLimited,
        "unauthorized" => FailureKind::Auth,
        "contextWindowExceeded" => FailureKind::ContextOverflow,
        _ => FailureKind::Unknown,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct RateLimitWindow {
    #[serde(default, rename = "usedPercent")]
    used_percent: f64,
    #[serde(default, rename = "windowDurationMins")]
    window_duration_mins: Option<u64>,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
pub(crate) struct RateLimitSnapshot {
    #[serde(default)]
    primary: Option<RateLimitWindow>,
    #[serde(default)]
    secondary: Option<RateLimitWindow>,
    #[serde(default, rename = "rateLimitReachedType")]
    reached: Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimits {
    #[serde(default, rename = "rateLimits")]
    pub(crate) rate_limits: RateLimitSnapshot,
}

impl RateLimitSnapshot {
    /// Applies a sparse update: missing windows keep their previous value.
    pub(crate) fn merge(&mut self, update: Self) {
        if update.primary.is_some() {
            self.primary = update.primary;
        }
        if update.secondary.is_some() {
            self.secondary = update.secondary;
        }
        if !update.reached.is_null() {
            self.reached = update.reached;
        }
    }

    pub(crate) fn signal(&self) -> Option<QuotaSignal> {
        let mut exhausted = match &self.reached {
            Value::Null => false,
            Value::String(s) => !s.trim().is_empty(),
            Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => true,
        };
        let mut windows = Vec::new();
        for w in [self.primary, self.secondary].into_iter().flatten() {
            let utilization = (w.used_percent / 100.0).clamp(0.0, 1.0);
            let duration = w.window_duration_mins.map(|m| Duration::from_secs(m * 60));
            exhausted |= utilization >= 1.0;
            windows.push(QuotaWindow {
                name: window_name(duration),
                utilization,
                resets_at: w
                    .resets_at
                    .filter(|r| *r > 0)
                    .and_then(|r| OffsetDateTime::from_unix_timestamp(r).ok()),
                duration,
            });
        }
        (!windows.is_empty() || exhausted).then_some(QuotaSignal { windows, exhausted })
    }

    /// The earliest reset of a full window.
    pub(crate) fn earliest_reset(&self) -> Option<OffsetDateTime> {
        let signal = self.signal()?;
        signal
            .windows
            .iter()
            .filter(|w| w.utilization >= 1.0)
            .filter_map(|w| w.resets_at)
            .min()
    }
}

fn window_name(d: Option<Duration>) -> String {
    match d.map(|d| d.as_secs() / 60) {
        None | Some(0) => "unknown".to_owned(),
        Some(300) => "five_hour".to_owned(),
        Some(10_080) => "weekly".to_owned(),
        Some(m) => format!("window_{m}m"),
    }
}

// --- server-request parameter shapes -------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CommandApproval {
    #[allow(dead_code, reason = "checked by the replay tests")]
    #[serde(default, rename = "threadId")]
    pub(crate) thread_id: String,
    #[serde(default)]
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileChangeApproval {
    #[serde(default, rename = "itemId")]
    pub(crate) item_id: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default, rename = "grantRoot")]
    pub(crate) grant_root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ElicitationMeta {
    #[serde(default, rename = "codex_approval_kind")]
    pub(crate) approval_kind: String,
    #[serde(default)]
    pub(crate) tool_name: String,
    #[serde(default)]
    pub(crate) tool_params: Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Elicitation {
    #[serde(default, rename = "serverName")]
    pub(crate) server_name: String,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default, rename = "_meta")]
    pub(crate) meta: ElicitationMeta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Label {
    #[serde(default)]
    pub(crate) label: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UserQuestion {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) question: String,
    #[serde(default)]
    pub(crate) options: Option<Vec<Label>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UserInput {
    #[serde(default)]
    pub(crate) questions: Vec<UserQuestion>,
}

/// The v2 decision for an approval.
pub(crate) fn v2_decision(allow: bool, for_session: bool) -> &'static str {
    match (allow, for_session) {
        (true, true) => "acceptForSession",
        (true, false) => "accept",
        (false, _) => "decline",
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact fractions")]
mod tests {
    use openchef_core::Effort;
    use serde_json::json;

    use super::{
        ModelList, RateLimitSnapshot, ThreadItem, TokenBreakdown, failure_kind, v2_decision,
    };
    use crate::FailureKind;

    #[test]
    fn failure_kind_table() {
        let cases = [
            (json!("usageLimitExceeded"), FailureKind::QuotaExhausted),
            (json!("sessionBudgetExceeded"), FailureKind::QuotaExhausted),
            (
                json!({"httpConnectionFailed": {"httpStatusCode": 502}}),
                FailureKind::RateLimited,
            ),
            (json!("serverOverloaded"), FailureKind::RateLimited),
            (json!("unauthorized"), FailureKind::Auth),
            (json!("contextWindowExceeded"), FailureKind::ContextOverflow),
            (json!("somethingNew"), FailureKind::Unknown),
            (json!(null), FailureKind::Unknown),
        ];
        for (info, want) in cases {
            assert_eq!(failure_kind(&info), want, "{info}");
        }
    }

    #[test]
    fn rate_limit_signal() {
        let snap: RateLimitSnapshot = serde_json::from_value(json!({
            "primary": {"usedPercent": 12.5, "windowDurationMins": 300, "resetsAt": 1_900_000_000},
            "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1_900_500_000},
        }))
        .unwrap();
        let sig = snap.signal().unwrap();
        assert!(sig.exhausted);
        assert_eq!(
            sig.windows
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["five_hour", "weekly"]
        );
        assert_eq!(sig.windows[0].utilization, 0.125);
        assert_eq!(
            snap.earliest_reset()
                .map(time::OffsetDateTime::unix_timestamp),
            Some(1_900_500_000)
        );
        let reached: RateLimitSnapshot =
            serde_json::from_value(json!({"rateLimitReachedType": "workspace"})).unwrap();
        assert!(
            reached
                .signal()
                .is_some_and(|s| s.exhausted && s.windows.is_empty())
        );
        assert!(RateLimitSnapshot::default().signal().is_none());
        let odd: RateLimitSnapshot = serde_json::from_value(
            json!({"primary": {"usedPercent": 1, "windowDurationMins": 60}}),
        )
        .unwrap();
        assert_eq!(odd.signal().unwrap().windows[0].name, "window_60m");
    }

    #[test]
    fn rate_limit_merge_keeps_missing_windows() {
        let mut snap: RateLimitSnapshot = serde_json::from_value(json!({
            "primary": {"usedPercent": 10, "windowDurationMins": 300},
            "secondary": {"usedPercent": 50, "windowDurationMins": 10080},
        }))
        .unwrap();
        snap.merge(
            serde_json::from_value(
                json!({"primary": {"usedPercent": 20, "windowDurationMins": 300}}),
            )
            .unwrap(),
        );
        let sig = snap.signal().unwrap();
        assert_eq!(
            (sig.windows[0].utilization, sig.windows[1].utilization),
            (0.2, 0.5)
        );
    }

    #[test]
    fn supported_efforts() {
        let list: ModelList = serde_json::from_value(json!({"data": [
            {"id": "a", "model": "a", "isDefault": true, "supportedReasoningEfforts": [{"reasoningEffort": "minimal"}, {"reasoningEffort": "low"}, {"reasoningEffort": "high"}, {"reasoningEffort": "xhigh"}]},
            {"id": "b", "model": "gpt-b", "supportedReasoningEfforts": [{"reasoningEffort": "medium"}]},
        ]}))
        .unwrap();
        assert_eq!(list.supported_efforts(None), [Effort::Low, Effort::High]);
        assert_eq!(list.supported_efforts(Some("gpt-b")), [Effort::Medium]);
        assert_eq!(list.supported_efforts(Some("nope")).len(), 0);
    }

    #[test]
    fn token_usage_splits_cache() {
        let b: TokenBreakdown = serde_json::from_value(
            json!({"inputTokens": 100, "cachedInputTokens": 40, "outputTokens": 10}),
        )
        .unwrap();
        let u = b.usage();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (60, 40, 10)
        );
    }

    #[test]
    fn tool_translation() {
        let item = |v| serde_json::from_value::<ThreadItem>(v).unwrap();
        let cmd = item(
            json!({"id": "c", "type": "commandExecution", "command": "ls -la", "status": "completed", "exitCode": 1, "aggregatedOutput": "x"}),
        );
        let call = cmd.tool_call().unwrap();
        assert_eq!(
            (call.name.as_str(), call.signature.as_str()),
            ("Bash", "Bash:ls -la")
        );
        assert!(
            cmd.tool_result().unwrap().is_error,
            "non-zero exit is an error"
        );
        let edit = item(
            json!({"id": "e", "type": "fileChange", "status": "completed", "changes": [{"path": "/a"}, {"path": "/b"}]}),
        );
        assert_eq!(edit.tool_call().unwrap().signature, "Edit:/a,/b");
        assert!(!edit.tool_result().unwrap().is_error);
        let mcp = item(
            json!({"id": "m", "type": "mcpToolCall", "server": "openchef", "tool": "ask_user", "status": "failed", "arguments": {"q": 1}}),
        );
        assert_eq!(
            mcp.tool_call().unwrap().signature,
            r#"mcp__openchef__ask_user:{"q":1}"#
        );
        assert!(mcp.tool_result().unwrap().is_error);
        assert!(item(json!({"type": "reasoning"})).tool_call().is_none());
        let long = item(
            json!({"id": "l", "type": "commandExecution", "status": "completed", "aggregatedOutput": "é".repeat(5000)}),
        );
        assert!(long.tool_result().unwrap().output.len() < 4200);
    }

    #[test]
    fn v2_decisions() {
        assert_eq!(v2_decision(true, true), "acceptForSession");
        assert_eq!(v2_decision(true, false), "accept");
        assert_eq!(v2_decision(false, true), "decline");
    }
}

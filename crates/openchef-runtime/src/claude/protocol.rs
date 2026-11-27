//! Wire types of `claude -p --input-format stream-json --output-format stream-json` and
//! the Agent SDK control protocol, as recorded with claude 2.1.325 (m1-findings §3, §9,
//! §10). Unknown fields are ignored.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{
    ApprovalRequest, Decision, EventBody, Failure, FailureKind, QuotaSignal, QuotaWindow,
    RequestKind, SessionRef, TurnResult, Usage,
};

/// The synthetic tool Claude uses for `--json-schema` output: plumbing, never a tool
/// call event.
pub(crate) const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";

const MAX_TOOL_OUTPUT: usize = 4 << 10;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Envelope {
    #[serde(default, rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub(crate) event: Value,
    #[serde(default)]
    pub(crate) message: Value,
    #[serde(default)]
    pub(crate) request_id: String,
    #[serde(default)]
    pub(crate) request: Value,
    #[serde(default)]
    pub(crate) response: Value,
    #[serde(default)]
    pub(crate) rate_limit_info: Value,
}

impl Envelope {
    /// Whether the message belongs to a subagent rather than the turn itself. Subagents
    /// are disabled (D-006); this is a safety net.
    pub(crate) fn is_subagent(&self) -> bool {
        self.parent_tool_use_id
            .as_deref()
            .is_some_and(|p| !p.is_empty())
    }
}

/// A streamed text delta, if the event is one.
pub(crate) fn text_delta(event: &Value) -> Option<&str> {
    (event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta")
        .then(|| event["delta"]["text"].as_str())
        .flatten()
        .filter(|t| !t.is_empty())
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ContentBlock {
    #[serde(default, rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) input: Value,
    #[serde(default)]
    pub(crate) tool_use_id: String,
    #[serde(default)]
    pub(crate) is_error: bool,
    #[serde(default)]
    pub(crate) content: Value,
}

/// The content blocks of an assistant or user message.
pub(crate) fn blocks(message: &Value) -> Vec<ContentBlock> {
    serde_json::from_value(message["content"].clone()).unwrap_or_default()
}

/// Flattens a `tool_result` content (a string or a list of text blocks), keeping the
/// head and tail of long output.
pub(crate) fn tool_result_text(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => String::new(),
    };
    truncate(text)
}

fn truncate(s: String) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s;
    }
    let half = MAX_TOOL_OUTPUT / 2;
    let head = s
        .char_indices()
        .take_while(|(i, _)| *i < half)
        .map(|(_, c)| c)
        .collect::<String>();
    let tail_start = s.len() - half;
    let tail = s
        .char_indices()
        .skip_while(|(i, _)| *i < tail_start)
        .map(|(_, c)| c)
        .collect::<String>();
    format!("{head}\n…\n{tail}")
}

/// Name plus the input with object keys sorted, for loop detection.
pub(crate) fn signature(name: &str, input: &Value) -> String {
    // serde_json's map keeps keys sorted (no preserve_order feature), so re-encoding
    // canonicalizes.
    format!("{name}:{input}")
}

#[derive(Debug, Default, Deserialize)]
#[allow(clippy::struct_field_names, reason = "the wire names")]
pub(crate) struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResultMessage {
    #[serde(default)]
    pub(crate) subtype: String,
    #[serde(default)]
    pub(crate) is_error: bool,
    #[serde(default)]
    pub(crate) result: String,
    #[serde(default)]
    structured_output: Value,
    #[serde(default)]
    terminal_reason: String,
    #[serde(default)]
    api_error_status: Option<i64>,
    #[serde(default)]
    pub(crate) result_index: Option<u32>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    permission_denials: Vec<Value>,
    #[serde(default, rename = "modelUsage")]
    model_usage: BTreeMap<String, Value>,
    #[serde(default)]
    usage: RawUsage,
}

impl ResultMessage {
    /// Usage from the result only: per-message usage would count twice (§6.2).
    pub(crate) fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cache_read_tokens: self.usage.cache_read_input_tokens,
            cache_write_tokens: self.usage.cache_creation_input_tokens,
            cost_usd: (self.total_cost_usd > 0.0).then_some(self.total_cost_usd),
        }
    }

    /// The terminal event. `stop` is why OpenChef interrupted the turn and wins over
    /// Claude's generic abort; `quota` is the latest in-stream signal.
    pub(crate) fn outcome(
        &self,
        stop: Option<FailureKind>,
        quota: Option<&QuotaSignal>,
    ) -> EventBody {
        if self.subtype == "success" && !self.is_error {
            let structured =
                (!self.structured_output.is_null()).then(|| self.structured_output.clone());
            return EventBody::TurnCompleted(TurnResult {
                final_text: self.result.clone(),
                structured,
                usage: self.usage(),
                denials: u32::try_from(self.permission_denials.len()).unwrap_or(u32::MAX),
                models: self.model_usage.keys().cloned().collect(),
                duration: Duration::from_millis(self.duration_ms),
            });
        }
        let (mut kind, message) = self.classify(quota);
        if let Some(stop) = stop
            && matches!(kind, FailureKind::Interrupted | FailureKind::Unknown)
        {
            kind = stop;
        }
        let resets_at = if matches!(kind, FailureKind::QuotaExhausted | FailureKind::RateLimited) {
            quota.and_then(earliest_full_reset)
        } else {
            None
        };
        EventBody::TurnFailed(Failure {
            kind,
            message,
            resets_at,
            usage: self.usage(),
        })
    }

    fn classify(&self, quota: Option<&QuotaSignal>) -> (FailureKind, String) {
        let mut msg = self.subtype.clone();
        if !self.terminal_reason.is_empty() {
            msg = format!("{msg} ({})", self.terminal_reason);
        }
        if !self.result.is_empty() {
            msg = format!("{msg}: {}", self.result);
        }
        match self.subtype.as_str() {
            "error_during_execution"
                if matches!(
                    self.terminal_reason.as_str(),
                    "aborted_streaming" | "aborted"
                ) =>
            {
                return (FailureKind::Interrupted, msg);
            }
            // A --max-budget-usd cap, not account quota: treating it as quota would
            // park the task in waiting_quota.
            "error_max_budget_usd" => return (FailureKind::Unknown, msg),
            "error_max_structured_output_retries" => return (FailureKind::Protocol, msg),
            _ => {}
        }
        let exhausted = quota.is_some_and(|q| q.exhausted);
        if let Some(status) = self.api_error_status {
            match status {
                429 | 529 | 500.. => {
                    return (
                        if exhausted {
                            FailureKind::QuotaExhausted
                        } else {
                            FailureKind::RateLimited
                        },
                        msg,
                    );
                }
                401 | 403 => return (FailureKind::Auth, msg),
                413 => return (FailureKind::ContextOverflow, msg),
                _ => {}
            }
        }
        let low = self.result.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| low.contains(w));
        let kind = if exhausted || has(&["usage limit", "limit reached"]) {
            FailureKind::QuotaExhausted
        } else if has(&["rate limit", "overloaded"]) {
            FailureKind::RateLimited
        } else if has(&["prompt is too long", "context window"]) {
            FailureKind::ContextOverflow
        } else if has(&["authentication", "/login", "unauthorized"]) {
            FailureKind::Auth
        } else {
            FailureKind::Unknown
        };
        (kind, msg)
    }
}

fn earliest_full_reset(q: &QuotaSignal) -> Option<OffsetDateTime> {
    q.windows
        .iter()
        .filter(|w| w.utilization >= 1.0)
        .filter_map(|w| w.resets_at)
        .min()
}

#[derive(Debug, Default, Deserialize)]
struct RateLimitWindow {
    #[serde(default)]
    status: String,
    #[serde(default)]
    utilization: f64,
    #[serde(default, rename = "resetsAt")]
    resets_at: i64,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimitInfo {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "overageStatus")]
    overage_status: String,
    #[serde(default, rename = "unifiedWindows")]
    unified_windows: BTreeMap<String, RateLimitWindow>,
}

/// Normalizes a `rate_limit_event`. Utilizations are fractions, resets epoch seconds.
/// A limit is exhausted only when the request was rejected *and* no overage absorbs it;
/// `overageStatus: rejected` alone is the normal state without overage.
pub(crate) fn quota_signal(raw: &Value) -> Option<QuotaSignal> {
    let info: RateLimitInfo = serde_json::from_value(raw.clone()).ok()?;
    let windows: Vec<QuotaWindow> = info
        .unified_windows
        .iter()
        .map(|(name, w)| QuotaWindow {
            name: if name == "seven_day" {
                "weekly".to_owned()
            } else {
                name.clone()
            },
            utilization: if w.status == "rejected" {
                1.0
            } else {
                w.utilization.clamp(0.0, 1.0)
            },
            resets_at: (w.resets_at > 0)
                .then(|| OffsetDateTime::from_unix_timestamp(w.resets_at).ok())
                .flatten(),
            duration: match name.as_str() {
                "five_hour" => Some(Duration::from_hours(5)),
                "seven_day" => Some(Duration::from_hours(7 * 24)),
                _ => None,
            },
        })
        .collect();
    let exhausted = info.status == "rejected" && info.overage_status != "allowed";
    (!windows.is_empty() || exhausted).then_some(QuotaSignal { windows, exhausted })
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CanUseTool {
    #[serde(default)]
    pub(crate) subtype: String,
    #[serde(default)]
    pub(crate) tool_name: String,
    #[serde(default)]
    pub(crate) input: Value,
    #[serde(default)]
    description: String,
    #[serde(default)]
    blocked_path: String,
}

impl CanUseTool {
    /// The normalized request.
    pub(crate) fn approval_request(&self, session: SessionRef, raw: &Value) -> ApprovalRequest {
        let kind = match self.tool_name.as_str() {
            "Bash" => RequestKind::Command,
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => RequestKind::FileChange,
            "ExitPlanMode" | "EnterPlanMode" => RequestKind::ModeChange,
            _ => RequestKind::Tool,
        };
        let mut paths: Vec<String> = Vec::new();
        for p in [
            self.blocked_path.as_str(),
            self.input["file_path"].as_str().unwrap_or(""),
            self.input["notebook_path"].as_str().unwrap_or(""),
            self.input["path"].as_str().unwrap_or(""),
        ] {
            if !p.is_empty() && !paths.iter().any(|x| x == p) {
                paths.push(p.to_owned());
            }
        }
        ApprovalRequest {
            session,
            kind,
            tool: self.tool_name.clone(),
            command: (kind == RequestKind::Command)
                .then(|| self.input["command"].as_str().unwrap_or("").to_owned()),
            paths,
            description: self.description.clone(),
            input: self.input.clone(),
            raw: raw.clone(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ControlResponse {
    #[serde(default)]
    pub(crate) subtype: String,
    #[serde(default)]
    pub(crate) request_id: String,
    #[serde(default)]
    pub(crate) error: String,
    #[serde(default)]
    pub(crate) response: Value,
}

impl ControlResponse {
    /// The user messages an interrupt reported as not yet started (D-012).
    pub(crate) fn still_queued(&self) -> usize {
        self.response["still_queued"].as_array().map_or(0, Vec::len)
    }
}

pub(crate) fn permission_response(request_id: &str, input: &Value, d: &Decision) -> Value {
    let decision = if d.allow {
        let input = if input.is_null() {
            json!({})
        } else {
            input.clone()
        };
        json!({ "behavior": "allow", "updatedInput": input })
    } else {
        let msg = if d.message.is_empty() {
            "denied by OpenChef"
        } else {
            d.message.as_str()
        };
        json!({ "behavior": "deny", "message": msg })
    };
    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": decision } })
}

pub(crate) fn error_response(request_id: &str, msg: &str) -> Value {
    json!({ "type": "control_response", "response": { "subtype": "error", "request_id": request_id, "error": msg } })
}

pub(crate) fn control_request(request_id: &str, subtype: &str) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": subtype } })
}

pub(crate) fn user_message(text: &str) -> Value {
    json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "text", "text": text }] } })
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm, reason = "test failure output")]
mod tests {
    use serde_json::json;

    use super::{ResultMessage, quota_signal, signature, tool_result_text};
    use crate::{EventBody, FailureKind};

    fn result(v: serde_json::Value) -> ResultMessage {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn classify_failures() {
        let cases = [
            (
                json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_streaming"}),
                FailureKind::Interrupted,
            ),
            (
                json!({"subtype": "error_max_budget_usd", "is_error": true}),
                FailureKind::Unknown,
            ),
            (
                json!({"subtype": "error_max_structured_output_retries", "is_error": true}),
                FailureKind::Protocol,
            ),
            (
                json!({"subtype": "success", "is_error": true, "api_error_status": 429}),
                FailureKind::RateLimited,
            ),
            (
                json!({"subtype": "success", "is_error": true, "api_error_status": 401}),
                FailureKind::Auth,
            ),
            (
                json!({"subtype": "success", "is_error": true, "api_error_status": 413}),
                FailureKind::ContextOverflow,
            ),
            (
                json!({"subtype": "success", "is_error": true, "result": "Claude AI usage limit reached"}),
                FailureKind::QuotaExhausted,
            ),
            (
                json!({"subtype": "success", "is_error": true, "result": "Prompt is too long"}),
                FailureKind::ContextOverflow,
            ),
            (
                json!({"subtype": "success", "is_error": true, "result": "Please run /login"}),
                FailureKind::Auth,
            ),
            (
                json!({"subtype": "success", "is_error": true, "result": "weird"}),
                FailureKind::Unknown,
            ),
        ];
        for (raw, want) in cases {
            match result(raw.clone()).outcome(None, None) {
                EventBody::TurnFailed(f) => assert_eq!(f.kind, want, "{raw}"),
                other => panic!("{raw}: {other:?}"),
            }
        }
        // OpenChef's reason for stopping wins over Claude's generic abort.
        let abort = result(
            json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted"}),
        );
        assert!(
            matches!(abort.outcome(Some(FailureKind::Timeout), None), EventBody::TurnFailed(f) if f.kind == FailureKind::Timeout)
        );
    }

    #[test]
    fn quota_signal_and_exhaustion() {
        let q = quota_signal(
            &json!({"status": "rejected", "overageStatus": "rejected", "unifiedWindows": {
            "five_hour": {"status": "rejected", "utilization": 0.9, "resetsAt": 1_900_000_000},
            "seven_day": {"status": "allowed", "utilization": 0.3, "resetsAt": 1_900_500_000}}}),
        )
        .unwrap();
        assert!(q.exhausted);
        assert_eq!(
            q.windows
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["five_hour", "weekly"]
        );
        assert!(
            (q.windows[0].utilization - 1.0).abs() < f64::EPSILON,
            "a rejected window is full"
        );
        let allowed = quota_signal(&json!({"status": "allowed", "overageStatus": "rejected", "unifiedWindows": {"five_hour": {"status": "allowed", "utilization": 0.2}}})).unwrap();
        assert!(
            !allowed.exhausted,
            "overageStatus rejected alone is not exhaustion"
        );
        assert!(quota_signal(&json!({"status": "allowed"})).is_none());
        let failed =
            result(json!({"subtype": "success", "is_error": true, "api_error_status": 429}))
                .outcome(None, Some(&q));
        assert!(
            matches!(failed, EventBody::TurnFailed(f) if f.kind == FailureKind::QuotaExhausted && f.resets_at.is_some())
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(
            signature("Read", &json!({"b": 1, "a": 2})),
            r#"Read:{"a":2,"b":1}"#
        );
        assert_eq!(
            tool_result_text(
                &json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            "a\nb"
        );
        let long = "x".repeat(10_000);
        assert!(tool_result_text(&json!(long)).len() < 5000);
    }
}

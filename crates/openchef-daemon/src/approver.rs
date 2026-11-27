//! One session's permission decisions (§9.2), ported from Go `internal/app/approver.go`:
//! the evaluator answers allow and deny at once; what needs asking, and every
//! question, goes to the user through the approval queue (D-022).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use openchef_core::{Approval, ApprovalKind, ApprovalStatus, TaskId};
use openchef_engine::service::{AskRequest, Error as ServiceError, Service};
use openchef_permissions::{Decision as Verdict, Evaluator, Request};
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Question, RequestKind,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// What an agent is told when nobody answers its question.
pub(crate) const NO_ANSWER: &str = "No one is available to answer right now. Make the most \
    reasonable choice, say which one you made in your final answer, and continue.";

/// Queues a request for the user and waits for the answer ([`Service::ask`]).
pub(crate) trait Queue: Send + Sync + 'static {
    /// Asks; ends when the user, a timeout or `cancel` settles the request.
    fn ask(
        &self,
        req: AskRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Approval, ServiceError>>;
}

/// Lets session specs reach the service, which is built after the engine
/// configuration that needs them. Set once, read many times.
#[derive(Debug, Clone, Default)]
pub(crate) struct LateQueue {
    service: Arc<OnceLock<Service>>,
}

impl LateQueue {
    /// Connects the queue; a second call keeps the first service.
    pub(crate) fn set(&self, service: Service) {
        // Only the composition root calls this, once; a second service is ignored.
        let _first = self.service.set(service);
    }
}

impl Queue for LateQueue {
    fn ask(
        &self,
        req: AskRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Approval, ServiceError>> {
        let service = self.service.get().cloned();
        Box::pin(async move {
            match service {
                Some(s) => s.ask(req, &cancel).await,
                None => Err(ServiceError::Invalid(
                    "the approval queue is not ready".into(),
                )),
            }
        })
    }
}

/// Decides for one session of a task.
#[derive(Clone)]
pub(crate) struct PolicyApprover {
    /// Where requests that need a person go.
    pub(crate) queue: Arc<dyn Queue>,
    /// The session's policy.
    pub(crate) eval: Arc<Evaluator>,
    /// The task.
    pub(crate) task: TaskId,
    /// The agent working on it.
    pub(crate) agent: String,
}

impl std::fmt::Debug for PolicyApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyApprover")
            .field("task", &self.task)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

/// What the queue keeps for audit: the normalized request and why it was asked.
#[derive(Serialize)]
struct Audit<'a> {
    request: &'a ApprovalRequest,
    #[serde(skip_serializing_if = "str::is_empty")]
    reason: &'a str,
}

impl PolicyApprover {
    /// The evaluator's immediate decision, or `Err(reason)` when a person decides.
    /// A mode change is always the user's call.
    fn evaluate(&self, req: &ApprovalRequest) -> Result<Decision, String> {
        if req.kind == RequestKind::ModeChange {
            return Err(String::new());
        }
        let v = self.eval.evaluate(&permission_request(req));
        match v.decision {
            Verdict::Allow => Ok(Decision::allow()),
            Verdict::Deny => {
                let rule = v.rule.map_or_else(String::new, |r| format!(" ({r})"));
                Ok(Decision::deny(format!(
                    "OpenChef refused this: {}{rule}.",
                    v.reason
                )))
            }
            Verdict::Ask => Err(v.reason),
        }
    }

    fn ask_request(&self, req: &ApprovalRequest, reason: &str) -> AskRequest {
        // Serializing plain data cannot fail; without the audit copy the user still
        // sees the summary they decide on.
        let raw = serde_json::to_string(&Audit {
            request: req,
            reason,
        })
        .ok();
        AskRequest {
            task: self.task.clone(),
            agent: self.agent.clone(),
            kind: req.kind.into(),
            tool: Some(req.tool.clone()).filter(|t| !t.is_empty()),
            summary: summary(req),
            options: Vec::new(),
            raw,
        }
    }
}

/// The decision the user's answer stands for.
fn decision(got: &Approval) -> Decision {
    let answer = got.answer.as_deref().unwrap_or_default();
    match got.status {
        ApprovalStatus::Allowed => Decision::allow(),
        ApprovalStatus::Denied if answer.is_empty() => Decision::deny("The user denied this."),
        ApprovalStatus::Denied => {
            Decision::deny(format!("The user denied this. They said: {answer}"))
        }
        ApprovalStatus::Pending
        | ApprovalStatus::Answered
        | ApprovalStatus::Expired
        | ApprovalStatus::Withdrawn => Decision::deny(
            format!("{answer} OpenChef denied the request.")
                .trim_start()
                .to_owned(),
        ),
    }
}

/// A failed ask: an error while the agent stopped waiting or the daemon stops (the
/// adapter then denies), otherwise a denial the agent can read and work around.
fn ask_failed(e: &ServiceError, cancel: &CancellationToken) -> Result<Decision, ApproverError> {
    let stopped = match e {
        ServiceError::Withdrawn(_) | ServiceError::Stopping => true,
        ServiceError::Invalid(_)
        | ServiceError::Store(_)
        | ServiceError::Engine(_)
        | ServiceError::EventLog(_)
        | ServiceError::StopTimeout
        | ServiceError::Settled { .. } => cancel.is_cancelled(),
    };
    if stopped {
        return Err(ApproverError(e.to_string()));
    }
    Ok(Decision::deny(format!(
        "OpenChef could not ask the user, so it refused this: {e}"
    )))
}

impl Approver for PolicyApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let reason = match self.evaluate(&request) {
            Ok(d) => return Box::pin(async move { Ok(d) }),
            Err(reason) => reason,
        };
        let asked = self
            .queue
            .ask(self.ask_request(&request, &reason), cancel.clone());
        Box::pin(async move {
            match asked.await {
                Ok(got) => Ok(decision(&got)),
                Err(e) => ask_failed(&e, &cancel),
            }
        })
    }

    fn answer(
        &self,
        question: Question,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        let asked = self.queue.ask(
            AskRequest {
                task: self.task.clone(),
                agent: self.agent.clone(),
                kind: ApprovalKind::Question,
                tool: None,
                summary: question.text,
                options: question.options,
                raw: None,
            },
            cancel.clone(),
        );
        Box::pin(async move {
            match asked.await {
                Ok(got) if got.status == ApprovalStatus::Answered => {
                    Ok(got.answer.unwrap_or_default())
                }
                Ok(_) => Ok(NO_ANSWER.to_owned()),
                Err(e) => match ask_failed(&e, &cancel) {
                    Err(stop) => Err(stop),
                    // A question nobody can answer is not an error for the agent.
                    Ok(_) => Ok(NO_ANSWER.to_owned()),
                },
            }
        })
    }
}

/// What the evaluator decides on. Tool names follow Claude Code (every adapter
/// normalizes to them); a request without one is judged by its kind.
fn permission_request(req: &ApprovalRequest) -> Request {
    let tool = match (req.tool.as_str(), req.kind) {
        ("", RequestKind::Command) => "Bash",
        ("", RequestKind::FileChange) => "Edit",
        (tool, _) => tool,
    };
    let url = (tool == "WebFetch")
        .then(|| req.input.get("url").and_then(serde_json::Value::as_str))
        .flatten() // no URL: the evaluator asks
        .map(str::to_owned);
    Request {
        tool: tool.to_owned(),
        command: req.command.clone(),
        // A command's paths are where it runs, not what it touches.
        paths: if tool == "Bash" {
            Vec::new()
        } else {
            req.paths.iter().map(PathBuf::from).collect()
        },
        url,
    }
}

/// What the user decides on: the whole command, the paths, or the tool's input. It
/// is never shortened.
fn summary(req: &ApprovalRequest) -> String {
    if let Some(c) = req.command.as_deref().filter(|c| !c.is_empty()) {
        return c.to_owned();
    }
    if !req.paths.is_empty() {
        return req.paths.join(", ");
    }
    if !req.input.is_null() {
        return req.input.to_string();
    }
    if !req.description.is_empty() {
        return req.description.clone();
    }
    "(no details given)".to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use openchef_core::{Approval, ApprovalKind, ApprovalStatus, PermissionMode, TaskId};
    use openchef_engine::service::{AskRequest, Error as ServiceError};
    use openchef_runtime::{
        ApprovalRequest, Approver as _, BoxFuture, Question, RequestKind, SessionRef,
    };
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use tokio_util::sync::CancellationToken;

    use super::{NO_ANSWER, PolicyApprover, Queue};
    use crate::policy::tests::{test_policies, test_task};

    /// Answers every request with its status and answer and records what it was asked.
    #[derive(Default)]
    struct FakeQueue {
        status: Option<ApprovalStatus>,
        answer: Option<String>,
        fail: Option<fn() -> ServiceError>,
        asked: Mutex<Vec<AskRequest>>,
    }

    impl FakeQueue {
        fn with(status: ApprovalStatus, answer: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                status: Some(status),
                answer: answer.map(str::to_owned),
                ..Self::default()
            })
        }

        fn asked(&self) -> Vec<AskRequest> {
            self.asked.lock().unwrap_or_else(|e| panic!("{e}")).clone()
        }
    }

    impl Queue for FakeQueue {
        fn ask(
            &self,
            req: AskRequest,
            _cancel: CancellationToken,
        ) -> BoxFuture<'static, Result<Approval, ServiceError>> {
            let got = match self.fail {
                Some(fail) => Err(fail()),
                None => Ok(Approval {
                    id: 1,
                    task_id: req.task.clone(),
                    agent: req.agent.clone(),
                    kind: req.kind,
                    status: self.status.unwrap_or(ApprovalStatus::Pending),
                    tool: req.tool.clone(),
                    summary: req.summary.clone(),
                    options: req.options.clone(),
                    answer: self.answer.clone(),
                    decided_by: None,
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    expires_at: OffsetDateTime::UNIX_EPOCH,
                    decided_at: None,
                }),
            };
            self.asked
                .lock()
                .unwrap_or_else(|e| panic!("{e}"))
                .push(req);
            Box::pin(async move { got })
        }
    }

    fn req(
        kind: RequestKind,
        tool: &str,
        command: Option<&str>,
        paths: &[&str],
    ) -> ApprovalRequest {
        ApprovalRequest {
            session: SessionRef {
                runtime: "fake".into(),
                id: "s".into(),
            },
            kind,
            tool: tool.into(),
            command: command.map(str::to_owned),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            description: String::new(),
            input: Value::Null,
            raw: Value::Null,
        }
    }

    fn approver(
        queue: Arc<FakeQueue>,
        eval: &Arc<openchef_permissions::Evaluator>,
        agent: &str,
    ) -> PolicyApprover {
        PolicyApprover {
            queue,
            eval: Arc::clone(eval),
            task: TaskId::generate().unwrap_or_else(|e| panic!("{e}")),
            agent: agent.into(),
        }
    }

    /// The requests of the §9.3 matrix, by what they do, as each adapter sends them.
    fn matrix_shapes<'a>(
        inside: &str,
        outside: &str,
        wsp: &str,
    ) -> BTreeMap<&'a str, Vec<(&'a str, ApprovalRequest)>> {
        BTreeMap::from([
            (
                "edit inside",
                vec![
                    (
                        "claude",
                        req(RequestKind::FileChange, "Edit", None, &[inside]),
                    ),
                    (
                        "codex",
                        req(RequestKind::FileChange, "Edit", None, &[inside]),
                    ),
                    (
                        "agy",
                        req(RequestKind::FileChange, "Write", None, &[inside]),
                    ),
                ],
            ),
            (
                "edit outside",
                vec![
                    (
                        "claude",
                        req(RequestKind::FileChange, "Write", None, &[outside]),
                    ),
                    (
                        "codex",
                        req(RequestKind::FileChange, "Edit", None, &[inside, outside]),
                    ),
                    (
                        "agy",
                        req(RequestKind::FileChange, "Edit", None, &[outside]),
                    ),
                ],
            ),
            (
                "allowed command",
                vec![
                    (
                        "claude",
                        req(RequestKind::Command, "Bash", Some("go test ./..."), &[]),
                    ),
                    (
                        "codex",
                        req(RequestKind::Command, "Bash", Some("go test ./..."), &[wsp]),
                    ),
                ],
            ),
            (
                "unknown command",
                vec![("agy", req(RequestKind::Command, "Bash", Some("make"), &[]))],
            ),
            (
                "denied command",
                vec![(
                    "codex",
                    req(
                        RequestKind::Command,
                        "Bash",
                        Some("go test ./... && rm -rf ~"),
                        &[],
                    ),
                )],
            ),
        ])
    }

    /// The §9.3 matrix as each adapter normalizes requests (Claude, Codex and agy tool
    /// names and shapes), for every mode: what the approver decides at once and what
    /// it asks.
    #[tokio::test]
    async fn approver_mode_matrix() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (pols, ws) = test_policies(
            dir.path(),
            "[permissions]\nallow = [\"Bash(go test:*)\"]\ndeny = [\"Bash(rm:*)\"]\n",
        );
        let inside = ws.join("main.go").to_string_lossy().into_owned();
        let outside = dir
            .path()
            .join("elsewhere.go")
            .to_string_lossy()
            .into_owned();
        let wsp = ws.to_string_lossy().into_owned();
        let shapes = matrix_shapes(&inside, &outside, &wsp);
        let (allow, ask, deny) = ("allow", "ask", "deny");
        let want = [
            (PermissionMode::Manual, [ask, deny, allow, ask, deny]),
            (PermissionMode::AcceptEdits, [allow, deny, allow, ask, deny]),
            (PermissionMode::Auto, [allow, deny, allow, ask, deny]),
            (PermissionMode::Plan, [deny, deny, deny, deny, deny]),
            (PermissionMode::DontAsk, [deny, deny, allow, deny, deny]),
            (
                PermissionMode::BypassPermissions,
                [allow, deny, allow, allow, deny],
            ),
        ];
        let order = [
            "edit inside",
            "edit outside",
            "allowed command",
            "unknown command",
            "denied command",
        ];
        for (mode, expects) in want {
            let ps = pols
                .session(&test_task(&ws, mode))
                .unwrap_or_else(|e| panic!("{mode}: {e}"));
            for (what, expect) in order.iter().zip(expects) {
                for (agent, r) in &shapes[what] {
                    let q = FakeQueue::with(ApprovalStatus::Allowed, None);
                    let got = approver(Arc::clone(&q), &ps.eval, agent)
                        .approve(r.clone(), CancellationToken::new())
                        .await;
                    let (verdict, message) = match &got {
                        Err(e) => (format!("error: {e}"), String::new()),
                        Ok(_) if !q.asked().is_empty() => (ask.to_owned(), String::new()),
                        Ok(d) if d.allow => (allow.to_owned(), String::new()),
                        Ok(d) => (deny.to_owned(), d.message.clone()),
                    };
                    assert_eq!(verdict, expect, "{mode}, {what} from {agent}: {message}");
                    if verdict == deny {
                        assert!(message.starts_with("OpenChef refused this: "), "{message}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn approver_asks_the_user() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (pols, ws) = test_policies(dir.path(), "");
        let ps = pols
            .session(&test_task(&ws, PermissionMode::AcceptEdits))
            .unwrap_or_else(|e| panic!("{e}"));
        let command = "go test ./...\nrm -rf /tmp/x";
        let cmd = req(RequestKind::Command, "Bash", Some(command), &[]);
        for (status, answer, allow, msg) in [
            (ApprovalStatus::Allowed, None, true, ""),
            (
                ApprovalStatus::Denied,
                Some("use make test"),
                false,
                "They said: use make test",
            ),
            (ApprovalStatus::Denied, None, false, "The user denied this."),
            (
                ApprovalStatus::Expired,
                Some("No one answered within 15m."),
                false,
                "No one answered",
            ),
        ] {
            let q = FakeQueue::with(status, answer);
            let d = approver(Arc::clone(&q), &ps.eval, "opus")
                .approve(cmd.clone(), CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{status}: {e}"));
            assert_eq!(d.allow, allow, "{status}: {d:?}");
            assert!(d.message.contains(msg), "{status}: {d:?}");
            let asked = q.asked();
            assert_eq!(asked.len(), 1, "{status}");
            assert_eq!(asked[0].summary, command, "the summary is never shortened");
            assert_eq!(asked[0].kind, ApprovalKind::Command);
            assert_eq!(asked[0].agent, "opus");
            assert_eq!(asked[0].tool.as_deref(), Some("Bash"));
            let raw: Value = serde_json::from_str(asked[0].raw.as_deref().unwrap_or("null"))
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(raw["request"]["command"], json!(command));
            assert!(
                raw["reason"].is_string(),
                "the evaluator's reason is kept: {raw}"
            );
        }

        // A mode change is always the user's call.
        let q = FakeQueue::with(ApprovalStatus::Denied, None);
        let d = approver(Arc::clone(&q), &ps.eval, "opus")
            .approve(
                req(RequestKind::ModeChange, "ExitPlanMode", None, &[]),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!d.allow);
        assert_eq!(q.asked().len(), 1);
        assert_eq!(q.asked()[0].kind, ApprovalKind::ModeChange);

        let question = Question {
            session: SessionRef {
                runtime: "fake".into(),
                id: "s".into(),
            },
            text: "Which port?".into(),
            options: vec!["8080".into()],
        };
        let q = FakeQueue::with(ApprovalStatus::Answered, Some("8080"));
        let got = approver(Arc::clone(&q), &ps.eval, "opus")
            .answer(question.clone(), CancellationToken::new())
            .await;
        assert_eq!(got, Ok("8080".to_owned()));
        assert_eq!(q.asked()[0].kind, ApprovalKind::Question);
        assert_eq!(q.asked()[0].options, ["8080"]);
        let q = FakeQueue::with(ApprovalStatus::Expired, Some("No one answered."));
        let got = approver(q, &ps.eval, "opus")
            .answer(question, CancellationToken::new())
            .await;
        assert_eq!(got, Ok(NO_ANSWER.to_owned()), "expired question");
    }

    #[tokio::test]
    async fn approver_queue_failures() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (pols, ws) = test_policies(dir.path(), "");
        let ps = pols
            .session(&test_task(&ws, PermissionMode::Manual))
            .unwrap_or_else(|e| panic!("{e}"));
        let make = req(RequestKind::Command, "Bash", Some("make"), &[]);
        let failing = |fail: fn() -> ServiceError| {
            Arc::new(FakeQueue {
                fail: Some(fail),
                ..FakeQueue::default()
            })
        };
        // The store failed: the agent is told and the turn goes on.
        let d = approver(
            failing(|| ServiceError::Invalid("disk full".into())),
            &ps.eval,
            "a",
        )
        .approve(make.clone(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            !d.allow && d.message.contains("could not ask the user"),
            "{d:?}"
        );
        // The agent stopped waiting, or the daemon stops: an error, the adapter denies.
        for fail in [
            (|| ServiceError::Withdrawn(1)) as fn() -> ServiceError,
            || ServiceError::Stopping,
        ] {
            let got = approver(failing(fail), &ps.eval, "a")
                .approve(make.clone(), CancellationToken::new())
                .await;
            assert!(got.is_err(), "{got:?}");
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        let got = approver(failing(|| ServiceError::Invalid("x".into())), &ps.eval, "a")
            .approve(make, cancel)
            .await;
        assert!(got.is_err(), "cancelled: {got:?}");
    }

    #[test]
    fn permission_request_shapes() {
        let mut fetch = req(RequestKind::Tool, "WebFetch", None, &[]);
        fetch.input = json!({"url": "https://docs.rs"});
        assert_eq!(
            super::permission_request(&fetch).url.as_deref(),
            Some("https://docs.rs")
        );
        let bare = super::permission_request(&req(RequestKind::Command, "", Some("ls"), &["/w"]));
        assert_eq!(bare.tool, "Bash");
        assert!(bare.paths.is_empty(), "a command's paths are its directory");
        let edit = super::permission_request(&req(RequestKind::FileChange, "", None, &["/w/a"]));
        assert_eq!(edit.tool, "Edit");
        assert_eq!(edit.paths.len(), 1);
        assert_eq!(
            super::summary(&req(RequestKind::Tool, "mcp__x__y", None, &[])),
            "(no details given)"
        );
    }
}

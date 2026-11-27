//! The approval queue API (§9.2), ported from Go `internal/daemon/approvals.go`.

use std::collections::HashMap;
use std::sync::Arc;

use openchef_api::{
    ApprovalListParams, AskParams, AskResult, CODE_INVALID_PARAMS, CODE_TASK_INVALID, DecideParams,
    METHOD_APPROVAL_ASK, METHOD_APPROVAL_DECIDE, METHOD_APPROVAL_LIST, Verdict, rpc_error,
};
use openchef_core::{ApprovalKind, ApprovalStatus};
use openchef_engine::service::{AskRequest, Error as ServiceError, Service};
use openchef_runtime::linerpc::RpcError;
use openchef_store::ApprovalFilter;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::Method;
use crate::tasks::{decode, decode_required, encode, rpc_err, task_id};

/// The approval methods, served by `service`.
#[must_use]
pub fn approval_methods(service: Service) -> HashMap<String, Method> {
    let service = Arc::new(service);
    let mut methods: HashMap<String, Method> = HashMap::new();
    let s = Arc::clone(&service);
    methods.insert(
        METHOD_APPROVAL_LIST.into(),
        Arc::new(move |_conn, params, _cancel| {
            let s = Arc::clone(&s);
            Box::pin(async move { list(&s, params).await })
        }),
    );
    let s = Arc::clone(&service);
    methods.insert(
        METHOD_APPROVAL_DECIDE.into(),
        Arc::new(move |_conn, params, _cancel| {
            let s = Arc::clone(&s);
            Box::pin(async move { decide(&s, params).await })
        }),
    );
    let s = service;
    methods.insert(
        METHOD_APPROVAL_ASK.into(),
        Arc::new(move |_conn, params, cancel| {
            let s = Arc::clone(&s);
            Box::pin(async move { ask(&s, params, cancel).await })
        }),
    );
    methods
}

async fn list(s: &Service, params: Value) -> Result<Value, RpcError> {
    let p: ApprovalListParams = decode(params)?;
    let filter = ApprovalFilter {
        task: p.task.as_deref().map(task_id).transpose()?,
        pending: !p.all,
        limit: p.limit,
    };
    encode(&s.approvals(filter).await.map_err(|e| rpc_err(&e))?)
}

async fn decide(s: &Service, params: Value) -> Result<Value, RpcError> {
    let p: DecideParams = decode_required(params)?;
    let status = match p.verdict {
        Verdict::Allow => ApprovalStatus::Allowed,
        Verdict::Deny => ApprovalStatus::Denied,
        Verdict::Answer => ApprovalStatus::Answered,
    };
    let text = p.text.filter(|t| !t.is_empty());
    match s.decide(p.id, status, text).await {
        Ok(a) => encode(&a),
        // Users settle "requests" (`openchef approve 12`), whatever the store calls them.
        Err(ServiceError::Store(e)) if e.is_not_found() => Err(rpc_error(
            CODE_TASK_INVALID,
            format!("no such request {}", p.id),
        )),
        Err(e) => Err(rpc_err(&e)),
    }
}

/// Waits for the answer; the client going away withdraws the question.
async fn ask(s: &Service, params: Value, cancel: CancellationToken) -> Result<Value, RpcError> {
    let p: AskParams = decode_required(params)?;
    if p.agent.is_empty() || p.question.is_empty() {
        return Err(rpc_error(
            CODE_INVALID_PARAMS,
            "agent and question are required",
        ));
    }
    let req = AskRequest {
        task: task_id(&p.task)?,
        agent: p.agent,
        kind: ApprovalKind::Question,
        tool: None,
        summary: p.question,
        options: p.options,
        raw: None,
    };
    let a = s.ask(req, &cancel).await.map_err(|e| rpc_err(&e))?;
    let answered = a.status == ApprovalStatus::Answered;
    encode(&AskResult {
        status: a.status.to_string(),
        answer: a.answer.filter(|_| answered),
    })
}

/// The whole service API: tasks and approvals.
#[must_use]
pub fn service_methods(service: Service) -> HashMap<String, Method> {
    let mut methods = crate::task_methods(service.clone());
    methods.extend(approval_methods(service));
    methods
}

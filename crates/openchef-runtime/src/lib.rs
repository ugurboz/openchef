//! The contract between the engine and the agent CLIs it drives (`docs/architecture.md`
//! §6). Adapters implement [`Runtime`] and [`Session`]; the engine only sees the
//! normalized types here. Shared plumbing: [`rawlog`] (every protocol line, raw first),
//! [`linerpc`] (JSON-RPC over newline-delimited JSON) and [`TurnSender`] (a turn's event
//! stream that never blocks its producer).

mod approval;
mod error;
mod event;
pub mod linerpc;
pub mod rawlog;
mod spec;
mod turn;

pub use approval::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Question, RequestKind,
};
pub use error::Error;
pub use event::{
    Event, EventBody, Failure, FailureKind, QuotaSignal, QuotaWindow, ToolCall, ToolResult,
    TurnResult, Usage,
};
pub use spec::{
    INVARIANT_DENY_RULES, McpServer, OnSpawn, Runtime, Session, SessionRef, SessionSpec,
    TurnEvents, TurnInput, nearest_effort, parse_mode,
};
pub use turn::TurnSender;

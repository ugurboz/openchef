//! OpenChef's domain model (`docs/architecture.md` §4, §7.1): tasks, their steps, the
//! attempts agents make at them, the CLI sessions those attempts run in, the turns sent to
//! each session and the approvals they ask for, together with the state machines that
//! govern them.
//!
//! The crate does no I/O. Storage, runtimes and the engine depend on it, never the other
//! way round. Every closed vocabulary (statuses, kinds, modes) is an `enum` with a fixed
//! string form: that string is what the store persists, the API sends and the CLI shows,
//! so it must never change (D-018).

#[macro_use]
mod macros;

#[doc(hidden)]
pub use serde as __serde;

mod approval;
mod error;
mod id;
mod machine;
mod status;
mod task;
mod vocab;

pub use approval::{APPROVAL_TIMEOUT, Approval, ApprovalKind, ApprovalStatus, DecidedBy, Decision};
pub use error::Error;
pub use id::TaskId;
pub use machine::{Entity, StateMachine};
pub use status::{AttemptStatus, SessionState, StepStatus, TaskStatus, TurnStatus};
pub use task::{Attempt, MAX_STEPS, Session, Step, Task, Turn, validate_plan};
pub use vocab::{Backend, Effort, Importance, Kind, Mode, PermissionMode, Tier, TurnKind};

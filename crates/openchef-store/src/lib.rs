//! OpenChef's durable state: one SQLite database in WAL mode with versioned,
//! forward-only migrations (`docs/architecture.md` §11, D-018).
//!
//! Every state change is a compare-and-set on the state the caller believes the row is
//! in, checked against the `openchef-core` state machines, and committed before the
//! caller performs the side effect it authorizes (write-ahead rule).

mod approvals;
mod attempts;
mod db;
mod debate;
mod error;
mod findings;
mod migrate;
mod quota;
mod sql;
mod tasks;

pub use approvals::{ApprovalFilter, NewApproval};
pub use attempts::{NewAttempt, NewSession};
pub use db::{Clock, OpenOptions, Store};
pub use error::Error;
pub use findings::FindingRow;
pub use migrate::Migration;
pub use quota::{ObservationRow, Spend};
pub use tasks::{StepChange, TaskChange, TaskFilter};

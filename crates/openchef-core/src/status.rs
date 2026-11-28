//! Status enums and their state machines (§7.1, D-018).

use crate::machine::{Entity, StateMachine};

string_enum! {
    /// The state of a task (§7.1).
    pub enum TaskStatus: "task status" {
        /// The planner is producing the plan.
        Planning => "planning",
        /// The plan waits for the user.
        AwaitingApproval => "awaiting_approval",
        /// Steps are being implemented.
        Running => "running",
        /// The result is being reviewed.
        Reviewing => "reviewing",
        /// Every eligible pool is exhausted; resumes at the earliest reset.
        WaitingQuota => "waiting_quota",
        /// Retries ran out or something needs the user.
        NeedsAttention => "needs_attention",
        /// Finished and verified.
        Done => "done",
        /// Failed for good.
        Failed => "failed",
        /// Cancelled by the user.
        Cancelled => "cancelled",
    }
}

impl TaskStatus {
    /// Whether an engine loop drives a task in this state: its steps may start attempts,
    /// sessions and turns only then.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Planning | Self::Running | Self::Reviewing)
    }

    /// The final task states, sorted by their string form.
    #[must_use]
    pub fn terminal_statuses() -> Vec<Self> {
        let mut out: Vec<Self> = Self::ALL
            .iter()
            .copied()
            .filter(|s| s.is_terminal())
            .collect();
        out.sort_by_key(|s| s.as_str());
        out
    }
}

impl StateMachine for TaskStatus {
    const ENTITY: Entity = Entity::Task;

    fn next(self) -> &'static [Self] {
        use TaskStatus::{
            AwaitingApproval, Cancelled, Done, Failed, NeedsAttention, Planning, Reviewing,
            Running, WaitingQuota,
        };
        match self {
            // A planner can run out of quota or fail like an implementer (D-037).
            Planning => &[
                AwaitingApproval,
                WaitingQuota,
                NeedsAttention,
                Failed,
                Cancelled,
            ],
            Running => &[
                Reviewing,
                Done,
                WaitingQuota,
                NeedsAttention,
                Failed,
                Cancelled,
            ],
            // A review that requests changes sends the task back to running (§7.5).
            Reviewing => &[Running, Done, NeedsAttention, Failed, Cancelled],
            // Back to planning: a task that stopped before its plan was approved, or a
            // plan the user sent back to the planner.
            AwaitingApproval | WaitingQuota | NeedsAttention => &[Running, Planning, Cancelled],
            Done | Failed | Cancelled => &[],
        }
    }

    fn name(self) -> &'static str {
        self.as_str()
    }
}

string_enum! {
    /// The state of one plan step.
    pub enum StepStatus: "step status" {
        /// Not started, or sent back after an attempt ended without a verdict.
        Pending => "pending",
        /// An attempt is working on it.
        Running => "running",
        /// Verification passed and the step is committed.
        Verified => "verified",
        /// Retries ran out.
        Failed => "failed",
    }
}

impl StateMachine for StepStatus {
    const ENTITY: Entity = Entity::Step;

    fn next(self) -> &'static [Self] {
        use StepStatus::{Failed, Pending, Running, Verified};
        match self {
            Pending => &[Running],
            // running → pending: the attempt ended without a verdict (failover, restart).
            Running => &[Verified, Failed, Pending],
            Verified => &[],
            // failed → pending: the user retries the task.
            Failed => &[Pending],
        }
    }

    fn name(self) -> &'static str {
        self.as_str()
    }
}

string_enum! {
    /// The state of one agent's attempt at one step.
    pub enum AttemptStatus: "attempt status" {
        /// The agent is working.
        Running => "running",
        /// The step was verified.
        Succeeded => "succeeded",
        /// The pool ran out of quota and the step moved to another pool.
        FailedOver => "failed_over",
        /// The attempt failed.
        Failed => "failed",
        /// The daemon stopped during the attempt.
        Interrupted => "interrupted",
        /// The task was cancelled.
        Cancelled => "cancelled",
    }
}

impl StateMachine for AttemptStatus {
    const ENTITY: Entity = Entity::Attempt;

    fn next(self) -> &'static [Self] {
        use AttemptStatus::{Cancelled, Failed, FailedOver, Interrupted, Running, Succeeded};
        match self {
            Running => &[Succeeded, FailedOver, Failed, Interrupted, Cancelled],
            Succeeded | FailedOver | Failed | Interrupted | Cancelled => &[],
        }
    }

    fn name(self) -> &'static str {
        self.as_str()
    }
}

string_enum! {
    /// The lifecycle of one agent CLI process and its conversation. A session is recorded
    /// as starting (process identity known, native id not yet) before the first message
    /// is sent (§5.1), opened once the CLI reports its id, and closed when its process is
    /// gone.
    pub enum SessionState: "session state" {
        /// Process started, native id not known yet.
        Starting => "starting",
        /// The CLI reported its session id.
        Open => "open",
        /// The process is gone.
        Closed => "closed",
    }
}

impl StateMachine for SessionState {
    const ENTITY: Entity = Entity::Session;

    fn next(self) -> &'static [Self] {
        use SessionState::{Closed, Open, Starting};
        match self {
            Starting => &[Open, Closed],
            Open => &[Closed],
            Closed => &[],
        }
    }

    fn name(self) -> &'static str {
        self.as_str()
    }
}

string_enum! {
    /// The state of one message sent to a session. A turn is recorded as sent before the
    /// message is written to the CLI.
    pub enum TurnStatus: "turn status" {
        /// Written (or about to be written) to the CLI.
        Sent => "sent",
        /// The CLI ended the turn normally.
        Completed => "completed",
        /// The turn failed.
        Failed => "failed",
        /// The turn was interrupted.
        Interrupted => "interrupted",
    }
}

impl StateMachine for TurnStatus {
    const ENTITY: Entity = Entity::Turn;

    fn next(self) -> &'static [Self] {
        use TurnStatus::{Completed, Failed, Interrupted, Sent};
        match self {
            Sent => &[Completed, Failed, Interrupted],
            Completed | Failed | Interrupted => &[],
        }
    }

    fn name(self) -> &'static str {
        self.as_str()
    }
}

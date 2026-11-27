//! Closed vocabularies that are not state machines.

use crate::TaskStatus;

string_enum! {
    /// How a task was started.
    pub enum Kind: "task kind" {
        /// Single step, no plan; starts running.
        Run => "run",
        /// Plan → approve → implement → verify → review.
        Pipeline => "pipeline",
    }
}

impl Kind {
    /// The state a new task of this kind starts in.
    #[must_use]
    pub const fn initial_status(self) -> TaskStatus {
        match self {
            Self::Run => TaskStatus::Running,
            Self::Pipeline => TaskStatus::Planning,
        }
    }
}

string_enum! {
    /// The workspace mode (§8.1).
    pub enum Mode: "workspace mode" {
        /// One worktree, steps in sequence.
        Isolated => "isolated",
        /// Independent steps in parallel worktrees, integrated by a merge queue.
        Team => "team",
    }
}

string_enum! {
    /// Where a task's agents and verification run (§8.1, §21.3).
    pub enum Backend: "execution backend" {
        /// Directly on the host, in worktrees.
        Local => "local",
        /// Inside OpenChef's built-in OS sandbox, in the same worktrees.
        Sandbox => "sandbox",
    }
}

string_enum! {
    /// How much a mistake in a step would hurt (§7.4). Ordered lowest first.
    pub enum Importance: "importance" {
        /// Cheap to get wrong.
        Low => "low",
        /// The default.
        Normal => "normal",
        /// Needs a frontier agent.
        High => "high",
        /// Highlighted everywhere; deepest debate.
        Critical => "critical",
    }
}

impl Importance {
    /// The higher of `self` and `floor`.
    #[must_use]
    pub fn at_least(self, floor: Self) -> Self {
        self.max(floor)
    }
}

string_enum! {
    /// A permission mode in Claude Code's vocabulary (§9.1).
    pub enum PermissionMode: "permission mode" {
        /// Ask for everything not allowed by a rule (Claude Code's former `default`).
        Manual => "manual",
        /// File edits are accepted; commands still ask.
        AcceptEdits => "acceptEdits",
        /// Read-only by construction.
        Plan => "plan",
        /// The runtime's own classifier decides.
        Auto => "auto",
        /// Anything that would ask is refused.
        DontAsk => "dontAsk",
        /// Everything except deny rules and invariants is allowed.
        BypassPermissions => "bypassPermissions",
    }
}

string_enum! {
    /// Why a message was sent to a session (§7.2, §5.3).
    pub enum TurnKind: "turn kind" {
        /// The step prompt, first message of an attempt.
        Prompt => "prompt",
        /// The restated step plus verification failure output.
        Retry => "retry",
        /// Failover to another pool.
        Handoff => "handoff",
        /// The daemon restarted during the previous turn.
        Recovery => "recovery",
        /// A loop or idle period was detected; interrupted and redirected.
        Nudge => "nudge",
    }
}

string_enum! {
    /// How hard a model may think (§7.4). Ordered lowest first.
    pub enum Effort: "effort" {
        /// Least thinking.
        Low => "low",
        /// Balanced.
        Medium => "medium",
        /// More thinking.
        High => "high",
        /// The most the model offers.
        Max => "max",
    }
}

string_enum! {
    /// An agent's capability tier (§7.4). Ordered weakest first.
    pub enum Tier: "tier" {
        /// Haiku, Flash, free models.
        Fast => "fast",
        /// Mid-range models.
        Standard => "standard",
        /// The strongest models of each provider.
        Frontier => "frontier",
    }
}

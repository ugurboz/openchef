//! The debate's bounds while it runs (§7.5, D-040): the task-wide budget of debate
//! turns, and the depth a debate gets, lowered once when a pool paying for it is under
//! pressure.

use std::sync::atomic::Ordering;

use openchef_core::{DebateDepth, Importance};

use crate::engine::{Error, Run};

/// The depth of one debate (a council, a step's review) as it goes: its importance
/// level, lowered at most once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Level {
    importance: Importance,
    lowered: bool,
}

impl Level {
    pub(crate) const fn new(importance: Importance) -> Self {
        Self {
            importance,
            lowered: false,
        }
    }

    /// The importance the debate runs at now.
    pub(crate) const fn importance(self) -> Importance {
        self.importance
    }
}

impl Run<'_> {
    /// The depth of `level` from the configured table.
    pub(crate) fn depth(&self, level: Level) -> DebateDepth {
        self.e.cfg.debate.depth(level.importance)
    }

    /// Lowers `level` by one when `pool` is under pressure and it was not lowered yet;
    /// says so at `step`. Returns whether it changed.
    pub(crate) fn lower_for(&self, level: &mut Level, step: u32, pool: &str) -> bool {
        if level.lowered || level.importance == Importance::Low {
            return false;
        }
        let Some(why) = self.e.cfg.router.pressure(pool) else {
            return false;
        };
        let to = level.importance.lowered();
        self.notice(
            step,
            None,
            format!("debate lowered from {} to {to}: {why}", level.importance),
        );
        *level = Level {
            importance: to,
            lowered: true,
        };
        true
    }

    /// The task's debate budget: `[debate] max_turns_per_task`, else 2 × steps + 4.
    fn budget(&self) -> u32 {
        self.e.cfg.debate.budget(self.debate_steps)
    }

    /// Whether the task may still spend a debate turn; says once when it may not.
    pub(crate) async fn debate_left(&self, step: u32) -> Result<bool, Error> {
        let left = self.store().debate_turns(&self.task.id).await? < self.budget();
        if !left {
            self.budget_used_up(step);
        }
        Ok(left)
    }

    /// Spends one debate turn before it is sent; `false` (said once) when the budget is
    /// used up and the turn must not run.
    pub(crate) async fn spend_debate_turn(&self, step: u32) -> Result<bool, Error> {
        let spent = self
            .store()
            .spend_debate_turn(&self.task.id, self.budget())
            .await?;
        if !spent {
            self.budget_used_up(step);
        }
        Ok(spent)
    }

    /// Why a debate turn was not run, for notices and skipped events.
    pub(crate) fn budget_reason(&self) -> String {
        format!("debate budget used up ({} turns)", self.budget())
    }

    fn budget_used_up(&self, step: u32) {
        if !self.budget_noted.swap(true, Ordering::Relaxed) {
            self.notice(
                step,
                None,
                format!(
                    "{}; further reviews, critiques and fixes are skipped",
                    self.budget_reason()
                ),
            );
        }
    }
}

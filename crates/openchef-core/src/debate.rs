//! How much debate a task gets (§7.5, D-040): the depth table by importance and the
//! task-wide budget of debate turns. `[debate]` in `config.toml` overrides both.

use crate::Importance;

/// Critics a council may have at most.
pub const MAX_CRITICS: u32 = 3;
/// Council or review rounds at most.
pub const MAX_ROUNDS: u32 = 5;

/// The debate of one importance level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebateDepth {
    /// Plan critics, each of another provider.
    pub council_critics: u32,
    /// Critique rounds of the council at most.
    pub council_rounds: u32,
    /// Review–fix rounds of a step at most.
    pub review_rounds: u32,
}

impl DebateDepth {
    /// The §7.5 table, frugal because most users are on entry plans.
    #[must_use]
    pub const fn table(importance: Importance) -> Self {
        let (council_critics, council_rounds, review_rounds) = match importance {
            Importance::Low => (0, 0, 0),
            Importance::Normal => (0, 0, 1),
            Importance::High => (1, 2, 2),
            Importance::Critical => (2, 3, 3),
        };
        Self {
            council_critics,
            council_rounds,
            review_rounds,
        }
    }

    /// What is wrong with this depth, if anything: beyond the caps, or critics without
    /// rounds (or rounds without critics).
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.council_critics > MAX_CRITICS {
            out.push(format!("council_critics: at most {MAX_CRITICS}"));
        }
        if self.council_rounds > MAX_ROUNDS {
            out.push(format!("council_rounds: at most {MAX_ROUNDS}"));
        }
        if self.review_rounds > MAX_ROUNDS {
            out.push(format!("review_rounds: at most {MAX_ROUNDS}"));
        }
        if (self.council_critics == 0) != (self.council_rounds == 0) {
            out.push("council_critics and council_rounds are both 0 or both positive".into());
        }
        out
    }
}

/// The debate settings of a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Debate {
    /// Debate turns one task may spend; `None` is 2 × steps + 4.
    pub max_turns_per_task: Option<u32>,
    /// By importance, lowest first.
    depth: [DebateDepth; 4],
}

impl Default for Debate {
    fn default() -> Self {
        Self {
            max_turns_per_task: None,
            depth: [
                Importance::Low,
                Importance::Normal,
                Importance::High,
                Importance::Critical,
            ]
            .map(DebateDepth::table),
        }
    }
}

impl Debate {
    /// The depth of `importance`.
    #[must_use]
    pub const fn depth(&self, importance: Importance) -> DebateDepth {
        self.depth[slot(importance)]
    }

    /// Replaces the depth of `importance`.
    pub const fn set_depth(&mut self, importance: Importance, depth: DebateDepth) {
        self.depth[slot(importance)] = depth;
    }

    /// The debate turns a task of `steps` steps may spend.
    #[must_use]
    pub fn budget(&self, steps: usize) -> u32 {
        self.max_turns_per_task.unwrap_or_else(|| {
            u32::try_from(steps)
                .unwrap_or(u32::MAX)
                .saturating_mul(2)
                .saturating_add(4)
        })
    }
}

const fn slot(importance: Importance) -> usize {
    match importance {
        Importance::Low => 0,
        Importance::Normal => 1,
        Importance::High => 2,
        Importance::Critical => 3,
    }
}

impl Importance {
    /// One level lower (`low` stays `low`): the depth of a debate under quota pressure.
    #[must_use]
    pub const fn lowered(self) -> Self {
        match self {
            Self::Low | Self::Normal => Self::Low,
            Self::High => Self::Normal,
            Self::Critical => Self::High,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Debate, DebateDepth};
    use crate::Importance;

    #[test]
    fn table_budget_and_lowering() {
        let d = Debate::default();
        assert_eq!(d.depth(Importance::Normal).review_rounds, 1);
        assert_eq!(d.depth(Importance::Critical).council_critics, 2);
        assert_eq!(d.budget(3), 10, "2 × steps + 4");
        let mut custom = d.clone();
        custom.max_turns_per_task = Some(7);
        assert_eq!(custom.budget(3), 7);
        assert_eq!(Importance::Critical.lowered(), Importance::High);
        assert_eq!(Importance::Low.lowered(), Importance::Low);
        for i in Importance::ALL {
            assert!(DebateDepth::table(*i).problems().is_empty(), "{i}");
        }
        let bad = DebateDepth {
            council_critics: 4,
            council_rounds: 0,
            review_rounds: 6,
        };
        assert_eq!(bad.problems().len(), 3, "{:?}", bad.problems());
    }
}

//! Choosing the agent for an attempt (§7.3). The daemon routes by quota (D-035);
//! this ordered router is the engine's simple default, used by tests.

use openchef_core::{Effort, Step, Task};
use time::OffsetDateTime;

/// A routed (runtime, model, pool) triple (§4) with its resolved effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// The configured name.
    pub name: String,
    /// `claude`, `codex` or `agy`.
    pub runtime: String,
    /// The model; `None` for the runtime's default.
    pub model: Option<String>,
    /// The quota pool.
    pub pool: String,
    /// The effort; `None` leaves the runtime's default.
    pub effort: Option<Effort>,
}

/// No agent is eligible for a step right now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no eligible agent: {reason}")]
pub struct NoAgent {
    /// The earliest time one may become eligible; `None` when nothing changes by itself.
    pub resume_at: Option<OffsetDateTime>,
    /// Why.
    pub reason: String,
}

/// Picks the agent for an attempt at a step. `exclude` lists pools that must not be
/// used for this attempt (they ran out of quota on this step).
pub trait Router: Send + Sync {
    /// The agent for this attempt.
    ///
    /// # Errors
    ///
    /// [`NoAgent`] when none is eligible.
    fn route(&self, task: &Task, step: &Step, exclude: &[String]) -> Result<Agent, NoAgent>;

    /// An agent by name, so a resumed attempt gets the agent's current settings.
    fn agent_named(&self, _name: &str) -> Option<Agent> {
        None
    }

    /// An agent independent of the work it judges (reviewer, plan critic, arbiter,
    /// §7.5): its runtime is none of `avoid.runtimes` and its pool none of
    /// `avoid.pools`. A task's pin does not apply.
    ///
    /// # Errors
    ///
    /// [`NoAgent`] when no such agent can work now.
    fn route_independent(
        &self,
        _task: &Task,
        _step: &Step,
        avoid: &Avoid,
    ) -> Result<Agent, NoAgent> {
        Err(NoAgent {
            resume_at: None,
            reason: format!("no agent outside {avoid} is configured"),
        })
    }
}

/// What an independent agent must not share with the work it judges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Avoid {
    /// Runtimes (providers) to avoid.
    pub runtimes: Vec<String>,
    /// Quota pools to avoid.
    pub pools: Vec<String>,
}

impl Avoid {
    /// Avoids these runtimes.
    #[must_use]
    pub fn runtimes(runtimes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            runtimes: runtimes.into_iter().map(Into::into).collect(),
            pools: Vec::new(),
        }
    }

    /// Whether `agent` is independent of what this avoids.
    #[must_use]
    pub fn allows(&self, agent: &Agent) -> bool {
        !self.runtimes.contains(&agent.runtime) && !self.pools.contains(&agent.pool)
    }
}

impl std::fmt::Display for Avoid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.runtimes.is_empty() {
            parts.push(format!("providers {}", self.runtimes.join(", ")));
        }
        if !self.pools.is_empty() {
            parts.push(format!("pools {}", self.pools.join(", ")));
        }
        if parts.is_empty() {
            return f.write_str("nothing");
        }
        f.write_str(&parts.join(" and "))
    }
}

/// Reports until when a pool is unusable (quota reset); `None` means usable.
pub type Cooldown = std::sync::Arc<dyn Fn(&str) -> Option<OffsetDateTime> + Send + Sync>;

/// Routes to the first agent whose pool is neither excluded nor cooling down, or to the
/// task's pinned agent.
pub struct Ordered {
    /// In order of preference.
    pub agents: Vec<Agent>,
    /// Pool cooldowns.
    pub cooldown: Option<Cooldown>,
}

impl std::fmt::Debug for Ordered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ordered")
            .field("agents", &self.agents)
            .finish_non_exhaustive()
    }
}

impl Router for Ordered {
    fn route(&self, task: &Task, _step: &Step, exclude: &[String]) -> Result<Agent, NoAgent> {
        let mut earliest: Option<OffsetDateTime> = None;
        for a in &self.agents {
            if task.agent_pin.as_ref().is_some_and(|pin| *pin != a.name)
                || exclude.contains(&a.pool)
            {
                continue;
            }
            if let Some(until) = self.cooldown.as_ref().and_then(|c| c(&a.pool)) {
                earliest = Some(earliest.map_or(until, |e| e.min(until)));
                continue;
            }
            return Ok(a.clone());
        }
        let reason = match &task.agent_pin {
            Some(pin) => format!("pinned agent {pin:?} is unavailable"),
            None => "every configured agent's pool is exhausted".to_owned(),
        };
        Err(NoAgent {
            resume_at: earliest,
            reason,
        })
    }

    fn agent_named(&self, name: &str) -> Option<Agent> {
        self.agents.iter().find(|a| a.name == name).cloned()
    }

    fn route_independent(
        &self,
        _task: &Task,
        _step: &Step,
        avoid: &Avoid,
    ) -> Result<Agent, NoAgent> {
        self.agents
            .iter()
            .find(|a| {
                avoid.allows(a) && self.cooldown.as_ref().is_none_or(|c| c(&a.pool).is_none())
            })
            .cloned()
            .ok_or_else(|| NoAgent {
                resume_at: None,
                reason: format!("no agent outside {avoid} is available"),
            })
    }
}

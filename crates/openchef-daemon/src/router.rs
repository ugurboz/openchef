//! Routing by quota (§7.3, §7.4, §10.3; D-026), ported from Go `internal/app/router.go`:
//! each configured agent becomes a candidate with its pool's health, headroom and
//! reset; admission excludes pools that cannot take the step; the implement phase's
//! strategy picks; the effort is resolved for the chosen pool.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openchef_config::{Billing, Config, Phase, Strategy};
use openchef_core::{Importance, Step, Task};
use openchef_engine::{Agent, Avoid, NoAgent, Router};
use openchef_quota::{Admission, Health, admit, headroom};
use openchef_router::{Candidate, Counters, EffortInput, Request, choose, resolve_effort};
use time::OffsetDateTime;

use crate::app::agents;
use crate::quota::{Quota, View};

/// Green headroom of at least 40 % that resets within this long would go unused, so
/// auto effort may rise (§7.4).
const SURPLUS_WINDOW: Duration = Duration::from_hours(1);
/// The headroom share that counts as surplus.
const SURPLUS_SHARE: f64 = 0.4;

/// Where the router reads quota from: the monitor, or a fixed view in tests.
pub(crate) trait QuotaSource: Send + Sync {
    /// The current view.
    fn view(&self) -> Arc<View>;
    /// Now.
    fn now(&self) -> OffsetDateTime;
}

impl QuotaSource for Quota {
    fn view(&self) -> Arc<View> {
        Quota::view(self)
    }

    fn now(&self) -> OffsetDateTime {
        Quota::now(self)
    }
}

/// The engine's router once quota is tracked.
pub(crate) struct QuotaRouter {
    cfg: Config,
    /// Config order.
    agents: Vec<Agent>,
    quota: Arc<dyn QuotaSource>,
    /// Weighted shares, kept for the daemon's life. A small leaf value, never held
    /// across `.await` (D-030).
    counters: Mutex<Counters>,
    /// The review phase's weighted shares, kept apart from the implement phase's.
    review_counters: Mutex<Counters>,
}

impl std::fmt::Debug for QuotaRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaRouter")
            .field("agents", &self.agents)
            .finish_non_exhaustive()
    }
}

/// An agent the router looked at and could not use.
struct Refusal {
    reason: String,
    reset: Option<OffsetDateTime>,
}

impl QuotaRouter {
    pub(crate) fn new(cfg: &Config, quota: Arc<dyn QuotaSource>) -> Self {
        Self {
            agents: agents(cfg),
            cfg: cfg.clone(),
            quota,
            counters: Mutex::new(Counters::new()),
            review_counters: Mutex::new(Counters::new()),
        }
    }

    fn candidate(
        &self,
        a: &Agent,
        view: &View,
        now: OffsetDateTime,
        exclude: &[String],
    ) -> (Candidate, Option<Refusal>) {
        let state = view.state(&a.pool, now);
        let (health, reset) = view.health(&a.pool, now);
        let refusal = if exclude.contains(&a.pool) {
            Some("ran out on this step".to_owned())
        } else {
            match admit(health, &state, view.estimate(&a.pool)) {
                Admission::Admitted => None,
                Admission::Refused { reason } => Some(reason),
            }
        };
        let (free, reset_in) = headroom(&state, now);
        let phase = self.cfg.routing.get(&Phase::Implement);
        let cand = Candidate {
            agent: a.name.clone(),
            pool: a.pool.clone(),
            tier: self
                .cfg
                .agents
                .get(&a.name)
                .map_or(openchef_core::Tier::Standard, |c| c.tier),
            health,
            headroom: free,
            reset_in,
            failure_rate: 0.0,
            api: self
                .cfg
                .pools
                .get(&a.pool)
                .is_some_and(|p| p.billing == Billing::Api),
            excluded: refusal.is_some(),
            weight: phase
                .and_then(|p| p.weights.get(&a.name).copied())
                .unwrap_or(0),
        };
        let refusal = refusal.map(|r| Refusal {
            reason: format!("{}: {r}", a.name),
            reset,
        });
        (cand, refusal)
    }

    /// The chosen agent's effort for its pool now (§7.4).
    fn effort(&self, a: &Agent, importance: Importance, view: &View, now: OffsetDateTime) -> Agent {
        let Some(pool) = self.cfg.pools.get(&a.pool) else {
            return a.clone();
        };
        let state = view.state(&a.pool, now);
        let (health, _) = view.health(&a.pool, now);
        let (free, reset_in) = headroom(&state, now);
        let surplus = health == Health::Green
            && free >= SURPLUS_SHARE
            && reset_in.is_some_and(|r| !r.is_zero() && r < SURPLUS_WINDOW);
        let effort = resolve_effort(EffortInput {
            importance,
            profile: pool.effort_profile,
            plan: pool.plan_tier,
            health,
            surplus,
            pin: a.effort,
        });
        Agent {
            effort: Some(effort.level),
            ..a.clone()
        }
    }
}

fn no_agent(reason: String, refused: &[Refusal]) -> NoAgent {
    let resume_at = refused.iter().filter_map(|r| r.reset).min();
    let reason = if refused.is_empty() {
        reason
    } else {
        let why: Vec<&str> = refused.iter().map(|r| r.reason.as_str()).collect();
        format!("{reason} ({})", why.join("; "))
    };
    NoAgent { resume_at, reason }
}

impl Router for QuotaRouter {
    fn route(&self, task: &Task, step: &Step, exclude: &[String]) -> Result<Agent, NoAgent> {
        let importance = step.importance.at_least(task.importance);
        let now = self.quota.now();
        let view = self.quota.view();
        let mut cands = Vec::new();
        let mut refused = Vec::new();
        for a in &self.agents {
            if task.agent_pin.as_ref().is_some_and(|pin| *pin != a.name) {
                continue;
            }
            let (c, r) = self.candidate(a, &view, now, exclude);
            cands.push(c);
            refused.extend(r);
        }
        let pick = if let Some(pin) = &task.agent_pin {
            // A pin disables selection (§7.3), not admission.
            match cands.first() {
                None => {
                    return Err(no_agent(
                        format!("pinned agent {pin:?} is not configured"),
                        &refused,
                    ));
                }
                Some(c) if c.excluded => {
                    return Err(no_agent(
                        format!("pinned agent {pin:?} cannot take the step now"),
                        &refused,
                    ));
                }
                Some(c) => c.agent.clone(),
            }
        } else {
            let strategy = self
                .cfg
                .routing
                .get(&Phase::Implement)
                .map_or(Strategy::Auto, |p| p.strategy);
            let req = Request {
                strategy,
                importance,
                allow_downgrade: false,
            };
            let decided = {
                let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
                choose(req, &cands, &mut counters)
            };
            decided
                .map_err(|e| no_agent(e.to_string(), &refused))?
                .agent
        };
        let Some(a) = self.agents.iter().find(|a| a.name == pick) else {
            return Err(no_agent(format!("agent {pick:?} vanished"), &refused));
        };
        Ok(self.effort(a, importance, &view, now))
    }

    fn agent_named(&self, name: &str) -> Option<Agent> {
        self.agents.iter().find(|a| a.name == name).cloned()
    }

    fn route_independent(&self, task: &Task, step: &Step, avoid: &Avoid) -> Result<Agent, NoAgent> {
        let importance = step.importance.at_least(task.importance);
        let now = self.quota.now();
        let view = self.quota.view();
        let mut cands = Vec::new();
        let mut refused = Vec::new();
        for a in self
            .agents
            .iter()
            .filter(|a| !avoid.runtimes.contains(&a.runtime) && !avoid.pools.contains(&a.pool))
        {
            let (c, r) = self.candidate(a, &view, now, &[]);
            cands.push(c);
            refused.extend(r);
        }
        let strategy = self
            .cfg
            .routing
            .get(&Phase::Review)
            .map_or(Strategy::Auto, |p| p.strategy);
        let req = Request {
            strategy,
            importance,
            allow_downgrade: false,
        };
        let decided = {
            let mut counters = self
                .review_counters
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            choose(req, &cands, &mut counters)
        };
        let pick = decided
            .map_err(|e| no_agent(format!("no agent outside {avoid}: {e}"), &refused))?
            .agent;
        let Some(a) = self.agents.iter().find(|a| a.name == pick) else {
            return Err(no_agent(format!("agent {pick:?} vanished"), &refused));
        };
        Ok(self.effort(a, importance, &view, now))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use openchef_config::{
        Agent as AgentConfig, Billing, Config, EffortProfile, Phase, PhaseRouting, PlanTier, Pool,
        Provider, Quota as QuotaSwitches, Runtime, Strategy,
    };
    use openchef_core::{Effort, Importance, Step, StepStatus, Tier};
    use openchef_engine::Router as _;
    use openchef_quota::{Observation, Source, Window};
    use time::OffsetDateTime;
    use time::macros::datetime;

    use super::{QuotaRouter, QuotaSource};
    use crate::quota::View;

    const NOW: OffsetDateTime = datetime!(2026-11-26 12:00 UTC);

    /// Fixed pool states and estimates, replaceable between routings.
    #[derive(Default)]
    struct FakeQuota {
        used: Mutex<BTreeMap<String, (f64, Duration)>>,
        estimates: Mutex<BTreeMap<String, Vec<f64>>>,
    }

    impl FakeQuota {
        fn set(&self, pool: &str, used: f64, reset: Duration) {
            let _old = self
                .used
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(pool.into(), (used, reset));
        }

        /// An estimate of `e`: three equal step costs.
        fn estimate(&self, pool: &str, e: f64) {
            let _old = self
                .estimates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(pool.into(), vec![e; 3]);
        }
    }

    impl QuotaSource for FakeQuota {
        fn view(&self) -> Arc<View> {
            let used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
            let obs: Vec<Observation> = used
                .iter()
                .map(|(pool, (u, reset))| Observation {
                    pool: pool.clone(),
                    source: Source::Stream,
                    at: NOW,
                    windows: vec![Window {
                        name: "five_hour".into(),
                        utilization: *u,
                        resets_at: Some(NOW + *reset),
                        duration: None,
                    }],
                    exhausted: false,
                })
                .collect();
            let costs = self
                .estimates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            Arc::new(View::of(&obs, costs))
        }

        fn now(&self) -> OffsetDateTime {
            NOW
        }
    }

    fn pool(provider: Provider, plan: PlanTier) -> Pool {
        Pool {
            provider,
            billing: Billing::Subscription,
            plan_tier: Some(plan),
            max_concurrent: 1,
            effort_profile: EffortProfile::Auto,
            api_key_env: None,
            daily_budget_usd: None,
            monthly_budget_usd: None,
        }
    }

    fn agent(runtime: Runtime, pool: &str) -> AgentConfig {
        AgentConfig {
            runtime,
            model: None,
            pool: pool.into(),
            tier: Tier::Frontier,
            effort: None,
        }
    }

    fn config(strategy: Strategy) -> Config {
        Config {
            pools: BTreeMap::from([
                (
                    "claude-max".to_owned(),
                    pool(Provider::Anthropic, PlanTier::Max),
                ),
                ("chatgpt".to_owned(), pool(Provider::OpenAi, PlanTier::Plus)),
            ]),
            agents: BTreeMap::from([
                ("opus".to_owned(), agent(Runtime::Claude, "claude-max")),
                ("gpt".to_owned(), agent(Runtime::Codex, "chatgpt")),
            ]),
            routing: BTreeMap::from([(
                Phase::Implement,
                PhaseRouting {
                    strategy,
                    agents: vec!["opus".into(), "gpt".into()],
                    weights: BTreeMap::from([("opus".to_owned(), 3), ("gpt".to_owned(), 1)]),
                },
            )]),
            quota: QuotaSwitches::default(),
        }
    }

    fn route(
        r: &QuotaRouter,
        imp: Importance,
        pin: Option<&str>,
        exclude: &[&str],
    ) -> Result<openchef_engine::Agent, openchef_engine::NoAgent> {
        let mut task = crate::policy::tests::test_task(
            std::path::Path::new("/w"),
            openchef_core::PermissionMode::Manual,
        );
        task.agent_pin = pin.map(str::to_owned);
        let step = Step {
            task_id: task.id.clone(),
            index: 1,
            title: "s".into(),
            prompt: "p".into(),
            files: Vec::new(),
            verify: None,
            importance: imp,
            status: StepStatus::Pending,
            retries: 0,
            verified_commit: None,
            updated_at: NOW,
        };
        let exclude: Vec<String> = exclude.iter().map(|p| (*p).to_owned()).collect();
        r.route(&task, &step, &exclude)
    }

    const H4: Duration = Duration::from_hours(4);

    #[test]
    fn routing_follows_quota() {
        let q = Arc::new(FakeQuota::default());
        let r = QuotaRouter::new(
            &config(Strategy::Auto),
            Arc::clone(&q) as Arc<dyn QuotaSource>,
        );
        let pick = |want: &str| {
            let a = route(&r, Importance::Normal, None, &[]).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(a.name, want);
        };
        // More headroom wins.
        q.set("claude-max", 0.80, H4);
        q.set("chatgpt", 0.30, H4);
        pick("gpt");
        q.set("claude-max", 0.20, H4);
        q.set("chatgpt", 0.50, H4);
        pick("opus");
        // An exhausted pool takes nothing.
        q.set("claude-max", 1.0, Duration::from_hours(2));
        pick("gpt");
        // Admission: the step does not fit gpt's headroom with the margin; opus has room.
        q.set("claude-max", 0.50, H4);
        q.set("chatgpt", 0.75, H4);
        q.estimate("chatgpt", 0.25);
        q.estimate("claude-max", 0.25);
        pick("opus");
        // Red takes small steps only.
        q.set("claude-max", 0.95, H4);
        q.set("chatgpt", 0.90, H4);
        q.estimate("chatgpt", 0.04);
        q.estimate("claude-max", 0.10);
        pick("gpt");
        // The engine's exclusion (ran out on this step) is respected.
        assert!(
            route(&r, Importance::Normal, None, &["chatgpt"]).is_err(),
            "opus cannot take the step and gpt is excluded"
        );
        // Nothing fits: wait until the earliest reset.
        q.set("claude-max", 1.0, Duration::from_hours(3));
        q.set("chatgpt", 1.0, Duration::from_hours(2));
        let na = route(&r, Importance::Normal, None, &[]).unwrap_err();
        assert_eq!(na.resume_at, Some(NOW + Duration::from_hours(2)), "{na}");
    }

    #[test]
    fn pinned_agent_is_admitted_not_chosen() {
        let q = Arc::new(FakeQuota::default());
        q.set("claude-max", 0.9, Duration::from_hours(1));
        q.set("chatgpt", 0.1, Duration::from_hours(1));
        let r = QuotaRouter::new(
            &config(Strategy::Auto),
            Arc::clone(&q) as Arc<dyn QuotaSource>,
        );
        // A pin overrides the better-scored gpt.
        let a = route(&r, Importance::Normal, Some("opus"), &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.name, "opus");
        q.set("claude-max", 1.0, Duration::from_hours(1));
        let na = route(&r, Importance::Normal, Some("opus"), &[]).unwrap_err();
        assert_eq!(na.resume_at, Some(NOW + Duration::from_hours(1)), "{na}");
        let na = route(&r, Importance::Normal, Some("ghost"), &[]).unwrap_err();
        assert!(na.reason.contains("not configured"), "{na}");
    }

    #[test]
    fn weighted_shares_and_effort() {
        let q = Arc::new(FakeQuota::default());
        q.set("claude-max", 0.1, H4);
        q.set("chatgpt", 0.1, H4);
        let r = QuotaRouter::new(
            &config(Strategy::Weighted),
            Arc::clone(&q) as Arc<dyn QuotaSource>,
        );
        let mut opus = 0;
        for _ in 0..40 {
            let a = route(&r, Importance::Normal, None, &[]).unwrap_or_else(|e| panic!("{e}"));
            if a.name == "opus" {
                opus += 1;
            }
        }
        assert!(
            (29..=31).contains(&opus),
            "weighted 3:1 over 40 steps: {opus}"
        );

        // Effort: a Plus plan under pressure is frugal; Max with room is not.
        q.set("chatgpt", 0.7, H4);
        let r = QuotaRouter::new(
            &config(Strategy::Ordered),
            Arc::clone(&q) as Arc<dyn QuotaSource>,
        );
        let a = route(&r, Importance::High, Some("gpt"), &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.effort, Some(Effort::Low), "plus, yellow, high importance");
        let a = route(&r, Importance::High, Some("opus"), &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.effort, Some(Effort::High), "max, green, high importance");
    }

    #[test]
    fn reviewers_come_from_another_provider() {
        let q = Arc::new(FakeQuota::default());
        q.set("claude-max", 0.1, H4);
        q.set("chatgpt", 0.9, H4);
        let r = QuotaRouter::new(
            &config(Strategy::Auto),
            Arc::clone(&q) as Arc<dyn QuotaSource>,
        );
        let mut task = crate::policy::tests::test_task(
            std::path::Path::new("/w"),
            openchef_core::PermissionMode::Manual,
        );
        task.agent_pin = Some("opus".into());
        let step = Step {
            task_id: task.id.clone(),
            index: 1,
            title: "s".into(),
            prompt: "p".into(),
            files: Vec::new(),
            verify: None,
            importance: Importance::Normal,
            status: StepStatus::Running,
            retries: 0,
            verified_commit: None,
            updated_at: NOW,
        };
        // gpt has less headroom, but it is the only other provider; the pin is ignored.
        let a = r
            .route_independent(&task, &step, &openchef_engine::Avoid::runtimes(["claude"]))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.name, "gpt");
        q.set("chatgpt", 1.0, H4);
        assert!(
            r.route_independent(&task, &step, &openchef_engine::Avoid::runtimes(["claude"]))
                .is_err(),
            "never the same provider"
        );
    }
}

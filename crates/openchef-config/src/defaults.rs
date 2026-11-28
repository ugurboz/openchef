//! The configuration `openchef init` proposes for the installed runtimes.
//!
//! Model ids are deliberately not hard-coded where they change between releases: Claude
//! Code's aliases (`opus`, `sonnet`, `haiku`) are stable; Codex and agy use their own
//! default model when `model` is `None`.

use std::collections::BTreeMap;

use openchef_core::Tier;

use crate::model::{
    Agent, Billing, Config, DEFAULT_MAX_CONCURRENT, EffortProfile, Phase, PhaseRouting, PlanTier,
    Pool, Provider, Quota, Runtime, Strategy,
};

/// An agent `init` generates: name, model alias, tier.
type GeneratedAgent = (&'static str, Option<&'static str>, Tier);

/// The pool and agents generated for one runtime.
struct RuntimeDefaults {
    pool: &'static str,
    provider: Provider,
    plan_tier: PlanTier,
    agents: &'static [GeneratedAgent],
}

fn runtime_defaults(runtime: Runtime) -> RuntimeDefaults {
    match runtime {
        Runtime::Claude => RuntimeDefaults {
            pool: "claude",
            provider: Provider::Anthropic,
            plan_tier: PlanTier::Pro,
            agents: &[
                ("opus", Some("opus"), Tier::Frontier),
                ("sonnet", Some("sonnet"), Tier::Standard),
                ("haiku", Some("haiku"), Tier::Fast),
            ],
        },
        Runtime::Codex => RuntimeDefaults {
            pool: "chatgpt",
            provider: Provider::OpenAi,
            plan_tier: PlanTier::Plus,
            agents: &[("gpt", None, Tier::Frontier)],
        },
        Runtime::Agy => RuntimeDefaults {
            pool: "google",
            provider: Provider::Google,
            plan_tier: PlanTier::Pro,
            agents: &[("gemini", None, Tier::Frontier)],
        },
    }
}

/// Preferred agent order per phase; agents that were not generated are skipped.
fn phase_order(phase: Phase) -> &'static [&'static str] {
    match phase {
        Phase::Plan => &["opus", "gpt", "gemini"],
        Phase::Implement => &["opus", "gpt", "gemini", "sonnet"],
        Phase::Review => &["gpt", "gemini", "opus"],
        Phase::Helper => &["haiku", "gemini", "gpt"],
    }
}

/// Builds a configuration for `runtimes`. `plan_tiers` overrides the plan per runtime,
/// e.g. `{Claude: Max}`.
#[must_use]
pub fn default_config(runtimes: &[Runtime], plan_tiers: &BTreeMap<Runtime, PlanTier>) -> Config {
    let mut pools = BTreeMap::new();
    let mut agents = BTreeMap::new();
    for &runtime in runtimes {
        let RuntimeDefaults {
            pool: pool_name,
            provider,
            plan_tier: default_tier,
            agents: generated,
        } = runtime_defaults(runtime);
        let pool = Pool {
            provider,
            billing: Billing::Subscription,
            plan_tier: Some(plan_tiers.get(&runtime).copied().unwrap_or(default_tier)),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            effort_profile: EffortProfile::Auto,
            api_key_env: None,
            daily_budget_usd: None,
            monthly_budget_usd: None,
        };
        let _previous = pools.insert(pool_name.to_owned(), pool);
        for &(name, model, tier) in generated {
            let agent = Agent {
                runtime,
                model: model.map(str::to_owned),
                pool: pool_name.to_owned(),
                tier,
                effort: None,
            };
            let _previous = agents.insert(name.to_owned(), agent);
        }
    }
    let mut routing = BTreeMap::new();
    for &phase in Phase::ALL {
        let chosen: Vec<String> = phase_order(phase)
            .iter()
            .filter(|a| agents.contains_key(**a))
            .map(|a| (*a).to_owned())
            .collect();
        if !chosen.is_empty() {
            let _previous = routing.insert(
                phase,
                PhaseRouting {
                    strategy: Strategy::Auto,
                    agents: chosen,
                    weights: BTreeMap::new(),
                },
            );
        }
    }
    Config {
        pools,
        agents,
        routing,
        quota: Quota::default(),
        debate: openchef_core::Debate::default(),
    }
}

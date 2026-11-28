//! The file format: unknown-key detection with positions, loosely typed decoding, and
//! validation into the typed [`Config`].

use std::collections::BTreeMap;
use std::fmt::Display;
use std::str::FromStr;

use openchef_core::{Debate, DebateDepth, Effort, Importance, Tier};
use serde::{Deserialize, Serialize};
use toml::de::{DeTable, DeValue};

use crate::Error;
use crate::model::{
    Agent, Billing, Config, DEFAULT_MAX_CONCURRENT, EffortProfile, Phase, PhaseRouting, PlanTier,
    Pool, Provider, Quota, Runtime, Strategy,
};

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pools: BTreeMap<String, RawPool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agents: BTreeMap<String, RawAgent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    routing: BTreeMap<String, RawPhase>,
    #[serde(default, skip_serializing_if = "RawQuota::is_empty")]
    quota: RawQuota,
    #[serde(default, skip_serializing_if = "RawDebate::is_empty")]
    debate: RawDebate,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawPool {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    billing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_concurrent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daily_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    monthly_budget_usd: Option<f64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawAgent {
    #[serde(default)]
    runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default)]
    pool: String,
    #[serde(default)]
    tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawPhase {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strategy: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    weights: BTreeMap<String, i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probes: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    providers: BTreeMap<String, bool>,
}

impl RawQuota {
    fn is_empty(&self) -> bool {
        self.probes.is_none() && self.providers.is_empty()
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawDebate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_turns_per_task: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    low: Option<RawDepth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    normal: Option<RawDepth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    high: Option<RawDepth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    critical: Option<RawDepth>,
}

impl RawDebate {
    fn is_empty(&self) -> bool {
        self.max_turns_per_task.is_none()
            && self.low.is_none()
            && self.normal.is_none()
            && self.high.is_none()
            && self.critical.is_none()
    }

    const fn level(&self, importance: Importance) -> Option<&RawDepth> {
        match importance {
            Importance::Low => self.low.as_ref(),
            Importance::Normal => self.normal.as_ref(),
            Importance::High => self.high.as_ref(),
            Importance::Critical => self.critical.as_ref(),
        }
    }

    const fn level_mut(&mut self, importance: Importance) -> &mut Option<RawDepth> {
        match importance {
            Importance::Low => &mut self.low,
            Importance::Normal => &mut self.normal,
            Importance::High => &mut self.high,
            Importance::Critical => &mut self.critical,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawDepth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    council_critics: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    council_rounds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_rounds: Option<i64>,
}

/// Which keys a table may hold. `Free` tables take any key (names chosen by the user).
#[derive(Clone, Copy)]
enum Schema {
    Keys(&'static [(&'static str, Schema)]),
    /// A map from user-chosen names to tables of this schema.
    Named(&'static Schema),
    Free,
    Leaf,
}

const POOL: Schema = Schema::Keys(&[
    ("provider", Schema::Leaf),
    ("billing", Schema::Leaf),
    ("plan_tier", Schema::Leaf),
    ("max_concurrent", Schema::Leaf),
    ("effort_profile", Schema::Leaf),
    ("api_key_env", Schema::Leaf),
    ("daily_budget_usd", Schema::Leaf),
    ("monthly_budget_usd", Schema::Leaf),
]);
const AGENT: Schema = Schema::Keys(&[
    ("runtime", Schema::Leaf),
    ("model", Schema::Leaf),
    ("pool", Schema::Leaf),
    ("tier", Schema::Leaf),
    ("effort", Schema::Leaf),
]);
const PHASE: Schema = Schema::Keys(&[
    ("strategy", Schema::Leaf),
    ("agents", Schema::Leaf),
    ("weights", Schema::Free),
]);
const QUOTA: Schema = Schema::Keys(&[("probes", Schema::Leaf), ("providers", Schema::Free)]);
const DEPTH: Schema = Schema::Keys(&[
    ("council_critics", Schema::Leaf),
    ("council_rounds", Schema::Leaf),
    ("review_rounds", Schema::Leaf),
]);
const DEBATE: Schema = Schema::Keys(&[
    ("max_turns_per_task", Schema::Leaf),
    ("low", DEPTH),
    ("normal", DEPTH),
    ("high", DEPTH),
    ("critical", DEPTH),
]);
const ROOT: Schema = Schema::Keys(&[
    ("pools", Schema::Named(&POOL)),
    ("agents", Schema::Named(&AGENT)),
    ("routing", Schema::Named(&PHASE)),
    ("quota", QUOTA),
    ("debate", DEBATE),
]);

pub(crate) fn parse(text: &str) -> Result<Config, Error> {
    let table = DeTable::parse(text).map_err(|e| syntax(text, &e))?;
    let mut unknown = Vec::new();
    check_keys(text, table.get_ref(), ROOT, "", &mut unknown);
    if !unknown.is_empty() {
        return Err(Error::Invalid {
            path: None,
            problems: unknown,
        });
    }
    let raw: RawConfig = toml::from_str(text).map_err(|e| syntax(text, &e))?;
    validate(&raw).map_err(|problems| Error::Invalid {
        path: None,
        problems,
    })
}

fn syntax(text: &str, e: &toml::de::Error) -> Error {
    let (line, column) = line_col(text, e.span().map_or(0, |s| s.start));
    Error::Syntax {
        path: None,
        line,
        column,
        message: e.message().trim().to_owned(),
    }
}

/// 1-based line and column of byte `offset`.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before.rfind('\n').map_or(before.chars().count(), |nl| {
        before[nl + 1..].chars().count()
    }) + 1;
    (line, column)
}

/// Records every key the schema does not know, with its position.
fn check_keys(
    text: &str,
    table: &DeTable<'_>,
    schema: Schema,
    prefix: &str,
    out: &mut Vec<String>,
) {
    for (key, value) in table {
        let name = key.get_ref().as_ref();
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        };
        let child = match schema {
            Schema::Keys(keys) => {
                let Some((_, child)) = keys.iter().find(|(k, _)| *k == name) else {
                    let (line, column) = line_col(text, key.span().start);
                    out.push(format!(
                        "line {line}, column {column}: unknown key {path:?}"
                    ));
                    continue;
                };
                *child
            }
            Schema::Named(inner) => *inner,
            Schema::Free | Schema::Leaf => continue,
        };
        if let DeValue::Table(inner) = value.get_ref() {
            check_keys(text, inner, child, &path, out);
        }
    }
}

/// Collects problems with their TOML key.
struct Problems(Vec<String>);

impl Problems {
    fn add(&mut self, problem: impl Into<String>) {
        self.0.push(problem.into());
    }

    /// Parses a vocabulary value, recording `key: "value" is not one of …` on failure.
    fn one_of<T: FromStr + Display + Copy + 'static>(
        &mut self,
        key: &str,
        value: &str,
        all: &[T],
    ) -> Option<T> {
        if let Ok(v) = value.parse() {
            return Some(v);
        }
        let allowed: Vec<String> = all.iter().map(ToString::to_string).collect();
        self.add(format!(
            "{key}: {value:?} is not one of {}",
            allowed.join(", ")
        ));
        None
    }
}

fn validate(raw: &RawConfig) -> Result<Config, Vec<String>> {
    let mut p = Problems(Vec::new());
    let pools = validate_pools(&raw.pools, &mut p);
    let agents = validate_agents(&raw.agents, &raw.pools, &mut p);
    let routing = validate_routing(&raw.routing, &raw.agents, &mut p);
    let quota = validate_quota(&raw.quota, &mut p);
    let debate = validate_debate(&raw.debate, &mut p);
    if p.0.is_empty() {
        Ok(Config {
            pools,
            agents,
            routing,
            quota,
            debate,
        })
    } else {
        Err(p.0)
    }
}

fn validate_pools(raw: &BTreeMap<String, RawPool>, p: &mut Problems) -> BTreeMap<String, Pool> {
    if raw.is_empty() {
        p.add("pools: at least one pool is required");
    }
    let mut out = BTreeMap::new();
    for (name, rp) in raw {
        let key = format!("pools.{name}");
        let provider = p.one_of(&format!("{key}.provider"), &rp.provider, Provider::ALL);
        let billing = p.one_of(&format!("{key}.billing"), &rp.billing, Billing::ALL);
        let plan_tier = match &rp.plan_tier {
            Some(t) => p
                .one_of(&format!("{key}.plan_tier"), t, PlanTier::ALL)
                .map(Some),
            None => Some(None),
        };
        let effort_profile = match &rp.effort_profile {
            Some(e) => p.one_of(&format!("{key}.effort_profile"), e, EffortProfile::ALL),
            None => Some(EffortProfile::Auto),
        };
        let max_concurrent = match rp.max_concurrent {
            None => Some(DEFAULT_MAX_CONCURRENT),
            Some(n) => u32::try_from(n).ok().filter(|n| *n >= 1).or_else(|| {
                p.add(format!("{key}.max_concurrent: must be at least 1"));
                None
            }),
        };
        let is_api = billing == Some(Billing::Api);
        let api_key_env = rp.api_key_env.clone().filter(|s| !s.is_empty());
        match (is_api, &api_key_env) {
            (true, None) => p.add(format!(
                "{key}.api_key_env: required for api billing (subscription keys are never used implicitly)"
            )),
            (false, Some(_)) => p.add(format!("{key}.api_key_env: only allowed with billing = \"api\"")),
            _ => {}
        }
        let daily = rp.daily_budget_usd.unwrap_or(0.0);
        let monthly = rp.monthly_budget_usd.unwrap_or(0.0);
        if daily < 0.0 || monthly < 0.0 {
            p.add(format!("{key}: budgets cannot be negative"));
        }
        if !is_api && (daily > 0.0 || monthly > 0.0) {
            p.add(format!("{key}: USD budgets apply to billing = \"api\" pools; subscriptions have quota windows"));
        }
        if let (
            Some(provider),
            Some(billing),
            Some(plan_tier),
            Some(effort_profile),
            Some(max_concurrent),
        ) = (provider, billing, plan_tier, effort_profile, max_concurrent)
        {
            let pool = Pool {
                provider,
                billing,
                plan_tier,
                max_concurrent,
                effort_profile,
                api_key_env,
                daily_budget_usd: Some(daily).filter(|d| *d > 0.0),
                monthly_budget_usd: Some(monthly).filter(|m| *m > 0.0),
            };
            let _previous = out.insert(name.clone(), pool);
        }
    }
    out
}

fn validate_agents(
    raw: &BTreeMap<String, RawAgent>,
    pools: &BTreeMap<String, RawPool>,
    p: &mut Problems,
) -> BTreeMap<String, Agent> {
    if raw.is_empty() {
        p.add("agents: at least one agent is required");
    }
    let mut out = BTreeMap::new();
    for (name, ra) in raw {
        let key = format!("agents.{name}");
        let runtime = p.one_of(&format!("{key}.runtime"), &ra.runtime, Runtime::ALL);
        let tier = p.one_of(&format!("{key}.tier"), &ra.tier, Tier::ALL);
        let effort = match &ra.effort {
            Some(e) => p.one_of(&format!("{key}.effort"), e, Effort::ALL).map(Some),
            None => Some(None),
        };
        if !pools.contains_key(&ra.pool) {
            p.add(format!("{key}.pool: unknown pool {:?}", ra.pool));
        }
        if let (Some(runtime), Some(tier), Some(effort)) = (runtime, tier, effort) {
            let agent = Agent {
                runtime,
                model: ra.model.clone().filter(|m| !m.is_empty()),
                pool: ra.pool.clone(),
                tier,
                effort,
            };
            let _previous = out.insert(name.clone(), agent);
        }
    }
    out
}

fn validate_routing(
    raw: &BTreeMap<String, RawPhase>,
    agents: &BTreeMap<String, RawAgent>,
    p: &mut Problems,
) -> BTreeMap<Phase, PhaseRouting> {
    let mut out = BTreeMap::new();
    for (name, rp) in raw {
        let key = format!("routing.{name}");
        let phase = p.one_of(&key, name, Phase::ALL);
        let strategy = match &rp.strategy {
            Some(s) => p.one_of(&format!("{key}.strategy"), s, Strategy::ALL),
            None => Some(Strategy::Auto),
        };
        if rp.agents.is_empty() {
            p.add(format!("{key}.agents: at least one agent is required"));
        }
        let mut seen = Vec::new();
        for agent in &rp.agents {
            if !agents.contains_key(agent) {
                p.add(format!("{key}.agents: unknown agent {agent:?}"));
            }
            if seen.contains(&agent) {
                p.add(format!("{key}.agents: {agent:?} listed twice"));
            }
            seen.push(agent);
        }
        let weights = validate_weights(&key, rp, strategy, p);
        if let (Some(phase), Some(strategy)) = (phase, strategy) {
            let routing = PhaseRouting {
                strategy,
                agents: rp.agents.clone(),
                weights,
            };
            let _previous = out.insert(phase, routing);
        }
    }
    out
}

fn validate_weights(
    key: &str,
    rp: &RawPhase,
    strategy: Option<Strategy>,
    p: &mut Problems,
) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    if strategy != Some(Strategy::Weighted) {
        if !rp.weights.is_empty() {
            p.add(format!(
                "{key}.weights: only used with strategy = \"weighted\""
            ));
        }
        return out;
    }
    if rp.weights.is_empty() {
        p.add(format!(
            "{key}.weights: required with strategy = \"weighted\""
        ));
        return out;
    }
    for (agent, weight) in &rp.weights {
        if !rp.agents.contains(agent) {
            p.add(format!("{key}.weights: {agent:?} is not in {key}.agents"));
        }
        match u32::try_from(*weight).ok().filter(|w| *w > 0) {
            Some(w) => {
                let _previous = out.insert(agent.clone(), w);
            }
            None => p.add(format!("{key}.weights.{agent}: must be a positive integer")),
        }
    }
    out
}

fn validate_quota(raw: &RawQuota, p: &mut Problems) -> Quota {
    let mut providers = BTreeMap::new();
    for (name, on) in &raw.providers {
        if let Ok(provider) = name.parse::<Provider>() {
            let _previous = providers.insert(provider, *on);
        } else {
            let allowed: Vec<&str> = Provider::ALL.iter().map(|v| v.as_str()).collect();
            p.add(format!(
                "quota.providers.{name}: unknown provider (want one of {})",
                allowed.join(", ")
            ));
        }
    }
    Quota {
        probes: raw.probes,
        providers,
    }
}

/// Turns a debate setting into a count, or records why it is not one.
fn count(key: &str, value: Option<i64>, p: &mut Problems) -> Option<u32> {
    let v = value?;
    let n = u32::try_from(v).ok();
    if n.is_none() {
        p.add(format!("{key}: must be a non-negative integer"));
    }
    n
}

fn validate_debate(raw: &RawDebate, p: &mut Problems) -> Debate {
    let mut debate = Debate::default();
    if let Some(max) = count("debate.max_turns_per_task", raw.max_turns_per_task, p) {
        if max == 0 {
            p.add("debate.max_turns_per_task: must be positive");
        }
        debate.max_turns_per_task = Some(max);
    }
    for &importance in Importance::ALL {
        let Some(r) = raw.level(importance) else {
            continue;
        };
        let key = format!("debate.{importance}");
        let mut d = debate.depth(importance);
        if let Some(n) = count(&format!("{key}.council_critics"), r.council_critics, p) {
            d.council_critics = n;
        }
        if let Some(n) = count(&format!("{key}.council_rounds"), r.council_rounds, p) {
            d.council_rounds = n;
        }
        if let Some(n) = count(&format!("{key}.review_rounds"), r.review_rounds, p) {
            d.review_rounds = n;
        }
        for problem in d.problems() {
            p.add(format!("{key}: {problem}"));
        }
        debate.set_depth(importance, d);
    }
    debate
}

/// The `[debate]` table for `debate`: only what differs from the defaults.
fn encode_debate(debate: &Debate) -> RawDebate {
    let mut raw = RawDebate {
        max_turns_per_task: debate.max_turns_per_task.map(i64::from),
        ..RawDebate::default()
    };
    for &importance in Importance::ALL {
        let (d, table) = (debate.depth(importance), DebateDepth::table(importance));
        if d != table {
            *raw.level_mut(importance) = Some(RawDepth {
                council_critics: Some(i64::from(d.council_critics)),
                council_rounds: Some(i64::from(d.council_rounds)),
                review_rounds: Some(i64::from(d.review_rounds)),
            });
        }
    }
    raw
}

pub(crate) fn encode(config: &Config) -> Result<String, Error> {
    let raw = RawConfig {
        pools: config
            .pools
            .iter()
            .map(|(name, pool)| {
                let raw = RawPool {
                    provider: pool.provider.to_string(),
                    billing: pool.billing.to_string(),
                    plan_tier: pool.plan_tier.map(|t| t.to_string()),
                    max_concurrent: Some(i64::from(pool.max_concurrent)),
                    effort_profile: Some(pool.effort_profile.to_string()),
                    api_key_env: pool.api_key_env.clone(),
                    daily_budget_usd: pool.daily_budget_usd,
                    monthly_budget_usd: pool.monthly_budget_usd,
                };
                (name.clone(), raw)
            })
            .collect(),
        agents: config
            .agents
            .iter()
            .map(|(name, agent)| {
                let raw = RawAgent {
                    runtime: agent.runtime.to_string(),
                    model: agent.model.clone(),
                    pool: agent.pool.clone(),
                    tier: agent.tier.to_string(),
                    effort: agent.effort.map(|e| e.to_string()),
                };
                (name.clone(), raw)
            })
            .collect(),
        routing: config
            .routing
            .iter()
            .map(|(phase, routing)| {
                let raw = RawPhase {
                    strategy: Some(routing.strategy.to_string()),
                    agents: routing.agents.clone(),
                    weights: routing
                        .weights
                        .iter()
                        .map(|(a, w)| (a.clone(), i64::from(*w)))
                        .collect(),
                };
                (phase.to_string(), raw)
            })
            .collect(),
        quota: RawQuota {
            probes: config.quota.probes,
            providers: config
                .quota
                .providers
                .iter()
                .map(|(p, on)| (p.to_string(), *on))
                .collect(),
        },
        debate: encode_debate(&config.debate),
    };
    toml::to_string(&raw).map_err(|e| Error::Encode(e.to_string()))
}

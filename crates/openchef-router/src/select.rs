//! Agent selection (§7.3–7.4): tier filtering by importance and the auto, weighted
//! and ordered strategies.

use std::collections::BTreeMap;
use std::time::Duration;

use openchef_config::Strategy;
use openchef_core::{Importance, Tier};
use openchef_quota::Health;

/// One configured agent with the state the router needs. Candidates are given in
/// config order, which breaks ties.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The agent's name.
    pub agent: String,
    /// Its pool.
    pub pool: String,
    /// Its capability tier.
    pub tier: Tier,
    /// The pool's quota health.
    pub health: Health,
    /// The unused share of the pool's tightest window, in `[0, 1]`.
    pub headroom: f64,
    /// How soon that window resets (use it or lose it); `None` if unknown.
    pub reset_in: Option<Duration>,
    /// The share of recent attempts on this agent that failed, in `[0, 1]`.
    pub failure_rate: f64,
    /// Pay-per-use pools rank below subscriptions.
    pub api: bool,
    /// Removed for this attempt (pool exhausted on this step, not admitted, …).
    pub excluded: bool,
    /// The agent's target share for the weighted strategy.
    pub weight: u32,
}

/// One routing decision's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// How to pick among eligible agents.
    pub strategy: Strategy,
    /// The step's importance.
    pub importance: Importance,
    /// Lets a step use a lower tier than its importance permits when no permitted agent
    /// is available (`--allow-downgrade`). Never implied.
    pub allow_downgrade: bool,
}

/// The weighted strategy's running state, by agent. The caller keeps it (and may
/// persist it) so shares hold across decisions.
pub type Counters = BTreeMap<String, i64>;

/// The chosen agent and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The agent.
    pub agent: String,
    /// Why, for status and plan approval.
    pub reason: String,
    /// `allow_downgrade` let a lower tier through.
    pub downgraded: bool,
}

/// No agent passed the filters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "no eligible agent for {importance} importance (tiers {tiers}); a step is never silently downgraded"
)]
pub struct NoCandidate {
    /// The step's importance.
    pub importance: Importance,
    /// The tiers it allows, comma-separated.
    pub tiers: String,
}

/// The importance table of §7.4: the tiers a step may use.
#[must_use]
pub const fn allowed_tiers(i: Importance) -> &'static [Tier] {
    match i {
        Importance::Low => &[Tier::Fast, Tier::Standard],
        Importance::Normal => &[Tier::Standard, Tier::Frontier],
        Importance::High | Importance::Critical => &[Tier::Frontier],
    }
}

const ALL_TIERS: &[Tier] = &[Tier::Frontier, Tier::Standard, Tier::Fast];

/// Filters candidates by tier and health and picks one with the strategy.
///
/// # Errors
///
/// [`NoCandidate`] when no candidate is eligible.
pub fn choose(
    req: Request,
    cands: &[Candidate],
    counters: &mut Counters,
) -> Result<Decision, NoCandidate> {
    let mut eligible = filter(cands, allowed_tiers(req.importance));
    let mut downgraded = false;
    if eligible.is_empty() && req.allow_downgrade {
        eligible = filter(cands, ALL_TIERS);
        downgraded = !eligible.is_empty();
    }
    if eligible.is_empty() {
        let tiers: Vec<&str> = allowed_tiers(req.importance)
            .iter()
            .map(|t| t.as_str())
            .collect();
        return Err(NoCandidate {
            importance: req.importance,
            tiers: tiers.join(", "),
        });
    }
    let mut d = match req.strategy {
        Strategy::Ordered => Decision {
            agent: eligible[0].agent.clone(),
            reason: "first available in order".to_owned(),
            downgraded: false,
        },
        Strategy::Weighted => weighted(&eligible, counters),
        Strategy::Auto => auto(&eligible),
    };
    if downgraded {
        d.downgraded = true;
        d.reason
            .push_str(" (downgraded: no agent of an allowed tier was available)");
    }
    Ok(d)
}

fn filter<'a>(cands: &'a [Candidate], tiers: &[Tier]) -> Vec<&'a Candidate> {
    cands
        .iter()
        .filter(|c| !c.excluded && c.health != Health::Exhausted && tiers.contains(&c.tier))
        .collect()
}

/// Smooth weighted round-robin: deterministic, and exact shares over time. Agents
/// unavailable this time do not take part, so their share is redistributed
/// proportionally among the others.
fn weighted(eligible: &[&Candidate], counters: &mut Counters) -> Decision {
    let total: i64 = eligible.iter().map(|c| i64::from(c.weight)).sum();
    if total == 0 {
        return Decision {
            agent: eligible[0].agent.clone(),
            reason: "weighted (no weights; first available)".to_owned(),
            downgraded: false,
        };
    }
    let mut best: Option<(&Candidate, i64)> = None;
    for c in eligible {
        let v = counters.get(&c.agent).copied().unwrap_or(0) + i64::from(c.weight);
        let _previous = counters.insert(c.agent.clone(), v);
        if best.is_none_or(|(_, bv)| v > bv) {
            best = Some((c, v));
        }
    }
    let Some((pick, value)) = best else {
        // `eligible` is never empty here; the first candidate is the safe answer.
        return Decision {
            agent: eligible[0].agent.clone(),
            reason: "weighted".to_owned(),
            downgraded: false,
        };
    };
    let _previous = counters.insert(pick.agent.clone(), value - total);
    Decision {
        agent: pick.agent.clone(),
        reason: format!(
            "weighted ({}% target share)",
            100 * i64::from(pick.weight) / total
        ),
        downgraded: false,
    }
}

/// Spends the subscriptions the user pays for evenly before their windows reset:
/// headroom, weighted up when the reset is near, discounted by recent failures, for API
/// pools and for red health. Ties keep config order.
fn auto(eligible: &[&Candidate]) -> Decision {
    let mut best = eligible[0];
    let mut best_score = score(best);
    for c in &eligible[1..] {
        let s = score(c);
        if s > best_score {
            best = c;
            best_score = s;
        }
    }
    Decision {
        agent: best.agent.clone(),
        reason: format!(
            "auto (headroom {:.0}%, {})",
            100.0 * best.headroom,
            best.health.as_str()
        ),
        downgraded: false,
    }
}

const DAY: Duration = Duration::from_hours(24);

fn score(c: &Candidate) -> f64 {
    let mut s = if c.health == Health::Unknown {
        0.5 // no data: neither preferred nor avoided
    } else {
        c.headroom
    };
    if let Some(reset) = c.reset_in.filter(|r| !r.is_zero() && *r < DAY) {
        // Use it or lose it: headroom that resets within the day is worth up to 50 % more.
        s *= 1.0 + 0.5 * (1.0 - reset.as_secs_f64() / DAY.as_secs_f64());
    }
    s *= 1.0 - 0.8 * c.failure_rate;
    if c.api {
        s *= 0.3;
    }
    if c.health == Health::Red {
        s *= 0.5;
    }
    s
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use openchef_config::Strategy;
    use openchef_core::{Importance, Tier};
    use openchef_quota::Health;

    use super::{Candidate, Counters, Request, choose};

    fn cand(agent: &str, pool: &str, tier: Tier, headroom: f64, weight: u32) -> Candidate {
        Candidate {
            agent: agent.into(),
            pool: pool.into(),
            tier,
            health: Health::Green,
            headroom,
            reset_in: None,
            failure_rate: 0.0,
            api: false,
            excluded: false,
            weight,
        }
    }

    fn cands() -> Vec<Candidate> {
        vec![
            cand("opus", "claude", Tier::Frontier, 0.5, 50),
            cand("gpt", "chatgpt", Tier::Frontier, 0.5, 30),
            cand("gemini", "google", Tier::Frontier, 0.5, 20),
            cand("haiku", "claude", Tier::Fast, 0.9, 0),
        ]
    }

    fn req(strategy: Strategy, importance: Importance) -> Request {
        Request {
            strategy,
            importance,
            allow_downgrade: false,
        }
    }

    #[test]
    fn tier_filter_never_downgrades() {
        let mut c = cands();
        let mut n = Counters::new();
        let d = choose(req(Strategy::Ordered, Importance::Low), &c, &mut n).unwrap();
        assert_eq!(d.agent, "haiku", "low importance takes the fast agent");
        for x in &mut c[..3] {
            x.health = Health::Exhausted;
        }
        let err = choose(req(Strategy::Ordered, Importance::High), &c, &mut n).unwrap_err();
        assert!(
            err.to_string().contains("never silently downgraded"),
            "{err}"
        );
        let mut down = req(Strategy::Ordered, Importance::High);
        down.allow_downgrade = true;
        let d = choose(down, &c, &mut n).unwrap();
        assert!(d.agent == "haiku" && d.downgraded, "{d:?}");
    }

    #[test]
    fn ordered() {
        let mut c = cands();
        c[0].excluded = true;
        let d = choose(
            req(Strategy::Ordered, Importance::Critical),
            &c,
            &mut Counters::new(),
        )
        .unwrap();
        assert_eq!(d.agent, "gpt");
    }

    /// The M4 exit criterion: weighted shares hold within ±1 step at every point.
    #[test]
    fn weighted_shares_within_one_step() {
        let c = &cands()[..3];
        let mut counters = Counters::new();
        let mut got: BTreeMap<String, u32> = BTreeMap::new();
        for n in 1..=1000_u32 {
            let d = choose(req(Strategy::Weighted, Importance::High), c, &mut counters).unwrap();
            *got.entry(d.agent).or_default() += 1;
            for cand in c {
                let ideal = f64::from(n) * f64::from(cand.weight) / 100.0;
                let have = f64::from(got.get(&cand.agent).copied().unwrap_or(0));
                assert!(
                    (have - ideal).abs() <= 1.0,
                    "after {n} picks {} has {have}, ideal {ideal:.1}",
                    cand.agent
                );
            }
        }
        assert_eq!((got["opus"], got["gpt"], got["gemini"]), (500, 300, 200));
    }

    /// An unavailable agent's share goes to the others in proportion.
    #[test]
    fn weighted_redistributes() {
        let mut c = cands()[..3].to_vec();
        c[1].excluded = true; // gpt unavailable
        let mut counters = Counters::new();
        let mut got: BTreeMap<String, u32> = BTreeMap::new();
        for _ in 0..700 {
            let d = choose(req(Strategy::Weighted, Importance::High), &c, &mut counters).unwrap();
            *got.entry(d.agent).or_default() += 1;
        }
        assert_eq!(got.get("gpt"), None);
        assert_eq!((got["opus"], got["gemini"]), (500, 200), "shares 5:2");
    }

    /// Counters persisted across "restarts" keep the sequence exact.
    #[test]
    fn weighted_counters_persist() {
        let c = &cands()[..3];
        let mut shared = Counters::new();
        let mut got: BTreeMap<String, u32> = BTreeMap::new();
        for _ in 0..10 {
            let mut restored = shared.clone();
            for _ in 0..10 {
                let d =
                    choose(req(Strategy::Weighted, Importance::High), c, &mut restored).unwrap();
                *got.entry(d.agent).or_default() += 1;
            }
            shared = restored;
        }
        assert_eq!((got["opus"], got["gpt"], got["gemini"]), (50, 30, 20));
    }

    #[test]
    fn auto_strategy() {
        let pick = |c: &[Candidate]| {
            choose(
                req(Strategy::Auto, Importance::High),
                c,
                &mut Counters::new(),
            )
            .unwrap()
            .agent
        };
        let mut c = cands();
        assert_eq!(pick(&c), "opus", "equal scores: ties keep config order");
        c[1].headroom = 0.9;
        assert_eq!(pick(&c), "gpt", "more headroom");
        c[2].headroom = 0.8;
        c[2].reset_in = Some(Duration::from_hours(1));
        assert_eq!(
            pick(&c),
            "gemini",
            "headroom that resets soon is worth more"
        );
        c[2].failure_rate = 1.0;
        assert_eq!(pick(&c), "gpt", "failing agent avoided");
        c[1].api = true;
        assert_eq!(pick(&c), "opus", "API pools rank below subscriptions");
    }
}

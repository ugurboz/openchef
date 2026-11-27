//! Step cost (L3), the cost estimate, headroom and admission (§10.3, D-026).

use std::time::Duration;

use time::OffsetDateTime;

use crate::health::Health;
use crate::state::{State, is_later_window};

/// The safety factor on a step's estimated cost.
pub const MARGIN: f64 = 1.3;
/// The largest estimated cost (share of a window) a red pool accepts.
pub const SMALL_STEP: f64 = 0.05;
/// How many recent step costs an estimate looks at.
const COST_SAMPLES: usize = 20;
/// How many a percentile needs to mean anything.
const MIN_SAMPLES: usize = 3;

/// The share of a window one step used (L3, §10.1): the largest utilization increase
/// over the windows present in both readings that did not reset in between. `None`
/// when the readings cannot tell (no common window, or nothing newer after the step).
#[must_use]
pub fn spent(before: &State, after: &State) -> Option<f64> {
    if after.updated <= before.updated {
        return None;
    }
    let mut share: Option<f64> = None;
    for a in &after.windows {
        let Some(b) = before.window(&a.name) else {
            continue;
        };
        if is_later_window(a.resets_at, b.resets_at) {
            continue; // the window reset during the step: the difference means nothing
        }
        let delta = a.utilization - b.utilization;
        share = Some(share.map_or(delta, |s| s.max(delta)));
    }
    share.map(|s| s.max(0.0))
}

/// The expected cost of the next step from recent ones (oldest first): the 75th
/// percentile of the last 20; `None` below three samples.
#[must_use]
pub fn estimate(costs: &[f64]) -> Option<f64> {
    let recent = &costs[costs.len().saturating_sub(COST_SAMPLES)..];
    if recent.len() < MIN_SAMPLES {
        return None;
    }
    let mut sorted = recent.to_vec();
    sorted.sort_by(f64::total_cmp);
    // Nearest rank of the 75th percentile, round(0.75·(n−1)), in integers: Go computed
    // int(0.75·(n−1) + 0.5), which is exactly ⌊(3(n−1) + 2) / 4⌋.
    let idx = (3 * (sorted.len() - 1) + 2) / 4;
    sorted.get(idx).copied()
}

/// The unused share of the pool's tightest window and when that window resets.
fn tightest(s: &State) -> (f64, Option<OffsetDateTime>) {
    let mut free = 1.0;
    let mut reset = None;
    for w in &s.windows {
        let unused = 1.0 - w.utilization;
        if unused < free {
            free = unused;
            reset = w.resets_at;
        }
    }
    (free.max(0.0), reset)
}

/// The unused share of the pool's tightest window and how soon it resets after `now`
/// (`None` if unknown or past). A pool without windows has all its headroom.
#[must_use]
pub fn headroom(s: &State, now: OffsetDateTime) -> (f64, Option<Duration>) {
    let (free, reset) = tightest(s);
    let reset_in = reset
        .filter(|r| *r > now)
        .and_then(|r| Duration::try_from(r - now).ok());
    (free, reset_in)
}

/// Whether a pool takes a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// The step may run on the pool.
    Admitted,
    /// The pool refuses the step.
    Refused {
        /// Why, for display.
        reason: String,
    },
}

/// Decides whether a pool takes a step of the estimated cost (§10.3): exhausted pools
/// take nothing; red pools only small steps; any pool only what its headroom covers
/// with the margin. An unknown estimate (`None`) is admitted, since there is nothing to
/// compare, except where exhaustion already rules it out.
#[must_use]
pub fn admit(h: Health, s: &State, estimate: Option<f64>) -> Admission {
    if h == Health::Exhausted {
        return refuse("exhausted".to_owned());
    }
    let Some(estimate) = estimate else {
        return Admission::Admitted;
    };
    if h == Health::Red && estimate > SMALL_STEP {
        return refuse(format!(
            "red: takes only small steps (estimate {:.0}% > {:.0}%)",
            100.0 * estimate,
            100.0 * SMALL_STEP
        ));
    }
    let (free, _) = tightest(s);
    let need = estimate * MARGIN;
    if !s.windows.is_empty() && need > free {
        return refuse(format!(
            "headroom {:.0}% < {:.0}% (estimate with {:.0}% margin)",
            100.0 * free,
            100.0 * need,
            100.0 * (MARGIN - 1.0)
        ));
    }
    Admission::Admitted
}

const fn refuse(reason: String) -> Admission {
    Admission::Refused { reason }
}

//! Pool health (§10.3).

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::state::State;

/// Utilization from which a pool is yellow (§10.3).
pub const YELLOW_AT: f64 = 0.60;
/// Utilization from which a pool is red.
pub const RED_AT: f64 = 0.85;
/// Utilization from which a window counts as full.
pub const EXHAUSTED_AT: f64 = 0.99;
/// Data older than this degrades health by one level.
pub const STALE_AFTER: Duration = Duration::from_mins(15);

/// A pool's routing state, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Below 60 %.
    Green,
    /// From 60 % to below 85 %.
    Yellow,
    /// From 85 % to below 99 %: small steps only.
    Red,
    /// A full window, or the provider said so: no work until reset.
    Exhausted,
    /// No observation yet; the router treats it like green but marks it.
    Unknown,
}

impl Health {
    /// The lowercase name (`green`, …, `unknown`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
            Self::Exhausted => "exhausted",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rates a pool state at `now`: the worse of its windows, one level worse when the data
/// is stale (never past red), exhausted when the provider said so.
#[must_use]
pub fn health_of(s: &State, now: OffsetDateTime) -> Health {
    if s.exhausted {
        return Health::Exhausted;
    }
    if s.windows.is_empty() {
        return Health::Unknown;
    }
    let worst = s.windows.iter().map(|w| w.utilization).fold(0.0, f64::max);
    if worst >= EXHAUSTED_AT {
        return Health::Exhausted; // a full window is full however old the reading
    }
    let fresh = if worst >= RED_AT {
        Health::Red
    } else if worst >= YELLOW_AT {
        Health::Yellow
    } else {
        Health::Green
    };
    // No update time is Go's zero time: as stale as data gets.
    let stale = s.updated.is_none_or(|u| now - u > STALE_AFTER);
    if !stale {
        return fresh;
    }
    match fresh {
        Health::Green => Health::Yellow,
        Health::Yellow | Health::Red => Health::Red,
        Health::Exhausted => Health::Exhausted,
        Health::Unknown => Health::Unknown,
    }
}

/// When an exhausted pool is expected to take work again: the latest reset among its
/// full windows, or `exhausted_until`; `None` if unknown.
#[must_use]
pub fn reset_at(s: &State) -> Option<OffsetDateTime> {
    let mut at = s.exhausted_until;
    for w in &s.windows {
        if w.utilization >= EXHAUSTED_AT && w.resets_at > at {
            at = w.resets_at;
        }
    }
    at
}

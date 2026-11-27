//! Observations, windows and the merged per-pool state (§10.1).

use std::collections::BTreeMap;
use std::time::Duration;

use openchef_runtime::QuotaSignal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::health::EXHAUSTED_AT;

/// Where an observation came from (§10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    /// The CLI's own stream (`rate_limit_event`, `account/rateLimits/updated`).
    #[serde(rename = "L1")]
    Stream,
    /// An out-of-band probe.
    #[serde(rename = "L2")]
    Probe,
    /// Cost history.
    #[serde(rename = "L3")]
    Estimate,
}

impl Source {
    /// The level as shown to users and stored: `L1`, `L2` or `L3`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "L1",
            Self::Probe => "L2",
            Self::Estimate => "L3",
        }
    }
}

/// One rate-limit window of a pool, normalized.
///
/// JSON keeps the Go field names (`name`, `utilization`, `resetsAt`, `duration`).
/// `duration` is whole seconds (the Go encoding was nanoseconds): every provider
/// reports window lengths in seconds or minutes, and seconds read plainly in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// `five_hour`, `weekly` or a provider-specific name.
    pub name: String,
    /// In `[0, 1]`.
    pub utilization: f64,
    /// When the window resets, if known.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<OffsetDateTime>,
    /// The window's length, if known.
    #[serde(
        default,
        with = "duration_secs",
        skip_serializing_if = "Option::is_none"
    )]
    pub duration: Option<Duration>,
}

/// What one source said about one pool at one time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// The pool observed.
    pub pool: String,
    /// The source level.
    pub source: Source,
    /// When it was observed.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The windows reported.
    #[serde(default)]
    pub windows: Vec<Window>,
    /// The provider said the limit is reached and cannot be extended.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exhausted: bool,
}

/// Converts an in-stream runtime signal (L1).
#[must_use]
pub fn from_signal(pool: &str, at: OffsetDateTime, signal: &QuotaSignal) -> Observation {
    let windows = signal
        .windows
        .iter()
        .map(|w| Window {
            name: canonical(&w.name),
            utilization: clamp(w.utilization),
            resets_at: w.resets_at,
            // A zero length is "unknown" in the runtime's vocabulary too.
            duration: w.duration.filter(|d| !d.is_zero()),
        })
        .collect();
    Observation {
        pool: pool.to_owned(),
        source: Source::Stream,
        at,
        windows,
        exhausted: signal.exhausted,
    }
}

/// Maps provider window names onto OpenChef's.
pub(crate) fn canonical(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "five_hour" | "5h" | "session" | "primary" => "five_hour".to_owned(),
        "seven_day" | "weekly" | "weekly_all" | "secondary" | "7d" => "weekly".to_owned(),
        _ => name.to_owned(),
    }
}

/// Clamps a utilization into `[0, 1]`.
pub(crate) const fn clamp(u: f64) -> f64 {
    u.clamp(0.0, 1.0)
}

/// Reset times this close are the same window: sources round differently (epoch
/// seconds vs ISO strings with fractions).
pub(crate) const RESET_SLACK: Duration = Duration::from_secs(120);

/// Whether `new` is a later window than `old`: both resets known and `new` more than
/// the slack after `old`.
pub(crate) fn is_later_window(new: Option<OffsetDateTime>, old: Option<OffsetDateTime>) -> bool {
    matches!((new, old), (Some(n), Some(o)) if n > o + RESET_SLACK)
}

/// The merged view of one pool.
///
/// The `Option` times stand for Go's zero times; `None` sorts before every time, which
/// is how the Go code compared them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// The pool.
    pub pool: String,
    /// Its windows, sorted by name.
    #[serde(default)]
    pub windows: Vec<Window>,
    /// The newest observation said the limit is reached.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exhausted: bool,
    /// When an exhaustion report stops applying (the latest reset of the full windows
    /// known then); `None` means until the next observation.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub exhausted_until: Option<OffsetDateTime>,
    /// The time of the newest observation merged in; `None` before the first.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated: Option<OffsetDateTime>,
    /// The sources merged so far, in first-seen order, for display.
    #[serde(default)]
    pub sources: Vec<Source>,
}

impl State {
    /// Folds `o` into the state (§10.1): within one window utilization only increases,
    /// so a stale lower reading never undoes a newer higher one; a window with a later
    /// reset is a new window and replaces the old one; the newest observation wins
    /// across sources for exhaustion.
    pub fn merge(&mut self, o: &Observation) {
        for w in &o.windows {
            self.merge_window(w);
        }
        self.windows.sort_by(|a, b| a.name.cmp(&b.name));
        if self.updated.is_none_or(|u| o.at >= u) {
            self.exhausted = o.exhausted;
            self.exhausted_until = None;
            if o.exhausted {
                for w in &o.windows {
                    if w.utilization >= EXHAUSTED_AT && w.resets_at > self.exhausted_until {
                        self.exhausted_until = w.resets_at;
                    }
                }
            }
            self.updated = Some(o.at);
        }
        if !self.sources.contains(&o.source) {
            self.sources.push(o.source);
        }
    }

    fn merge_window(&mut self, w: &Window) {
        let Some(cur) = self.windows.iter_mut().find(|c| c.name == w.name) else {
            self.windows.push(w.clone());
            return;
        };
        if is_later_window(w.resets_at, cur.resets_at) {
            *cur = w.clone();
        } else if is_later_window(cur.resets_at, w.resets_at) {
            // An observation of a window that has already been replaced.
        } else {
            cur.utilization = cur.utilization.max(w.utilization);
            if cur.resets_at.is_none() {
                cur.resets_at = w.resets_at;
            }
            if cur.duration.is_none() {
                cur.duration = w.duration;
            }
        }
    }

    /// The window called `name`.
    pub(crate) fn window(&self, name: &str) -> Option<&Window> {
        self.windows.iter().find(|w| w.name == name)
    }

    /// A copy as of `now`, forgetting windows that have reset (their usage no longer
    /// counts) and an exhaustion whose reset has passed.
    fn current(&self, now: OffsetDateTime) -> Self {
        let mut c = self.clone();
        c.windows.retain(|w| w.resets_at.is_none_or(|r| now < r));
        if c.exhausted && c.exhausted_until.is_some_and(|u| now >= u) {
            c.exhausted = false;
            c.exhausted_until = None;
        }
        c
    }
}

/// The merged state of every pool. Owned by the quota actor; not shared.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    pools: BTreeMap<String, State>,
}

impl Tracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Merges an observation; one without a pool is ignored.
    pub fn observe(&mut self, o: &Observation) {
        if o.pool.is_empty() {
            return;
        }
        self.pools
            .entry(o.pool.clone())
            .or_insert_with(|| State {
                pool: o.pool.clone(),
                ..State::default()
            })
            .merge(o);
    }

    /// A pool's state at `now`, with windows past their reset dropped; `None` if the
    /// pool was never observed.
    #[must_use]
    pub fn state(&self, pool: &str, now: OffsetDateTime) -> Option<State> {
        self.pools.get(pool).map(|s| s.current(now))
    }

    /// Every pool's state at `now`, sorted by pool.
    #[must_use]
    pub fn states(&self, now: OffsetDateTime) -> Vec<State> {
        self.pools.values().map(|s| s.current(now)).collect()
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if signature"
)]
const fn is_false(b: &bool) -> bool {
    !*b
}

/// `Option<Duration>` as whole seconds.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(clippy::ref_option, reason = "serde's `with` signature")]
    pub(super) fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_u64(d.as_secs()),
            None => s.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_secs))
    }
}

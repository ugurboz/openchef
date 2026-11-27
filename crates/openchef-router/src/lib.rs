//! Choosing which agent does a step and how hard it thinks (`docs/architecture.md`
//! §7.3–7.4), ported from Go `internal/router`: tiers allowed by importance, effort
//! from importance, profile and quota, and the auto, weighted and ordered strategies.
//! Pure decision logic: the caller supplies candidates and state.

mod effort;
mod select;

pub use effort::{Effort, EffortInput, resolve_effort};
pub use select::{Candidate, Counters, Decision, NoCandidate, Request, allowed_tiers, choose};

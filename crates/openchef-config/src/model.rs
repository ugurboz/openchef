//! The typed configuration and its vocabularies (§7.3, §7.4, §10, §13).

use std::collections::BTreeMap;
use std::path::Path;

use openchef_core::{Effort, Tier};

use crate::Error;
use crate::raw;

/// Concurrent sessions per pool when `max_concurrent` is omitted.
pub const DEFAULT_MAX_CONCURRENT: u32 = 2;

openchef_core::string_enum! {
    /// A company / CLI family (§4).
    pub enum Provider: "provider" {
        /// Claude Code.
        Anthropic => "anthropic",
        /// Codex.
        OpenAi => "openai",
        /// Antigravity.
        Google => "google",
    }
}

openchef_core::string_enum! {
    /// How a pool is paid for.
    pub enum Billing: "billing" {
        /// A plan the user already pays for (G4).
        Subscription => "subscription",
        /// Pay per use with an API key.
        Api => "api",
        /// A free tier.
        Free => "free",
    }
}

openchef_core::string_enum! {
    /// The plan behind a pool; drives the `auto` effort start (§7.4).
    pub enum PlanTier: "plan tier" {
        /// Free plan.
        Free => "free",
        /// Claude Pro, Google AI Pro.
        Pro => "pro",
        /// ChatGPT Plus.
        Plus => "plus",
        /// Claude Max.
        Max => "max",
        /// Google Ultra.
        Ultra => "ultra",
        /// Team plans.
        Team => "team",
        /// Enterprise plans.
        Enterprise => "enterprise",
        /// API billing.
        Api => "api",
    }
}

openchef_core::string_enum! {
    /// How effort adapts to quota (§7.4).
    pub enum EffortProfile: "effort profile" {
        /// Plan-based start, then adaptive.
        Auto => "auto",
        /// Cheapest possible runs (−2).
        Minimal => "minimal",
        /// Entry plans (−1).
        Economy => "economy",
        /// Large plans (0).
        Balanced => "balanced",
        /// Quality over quota (+1).
        Quality => "quality",
    }
}

openchef_core::string_enum! {
    /// An agent CLI OpenChef drives (§6).
    pub enum Runtime: "runtime" {
        /// Claude Code.
        Claude => "claude",
        /// Codex CLI.
        Codex => "codex",
        /// Antigravity.
        Agy => "agy",
    }
}

openchef_core::string_enum! {
    /// How the router chooses among eligible agents (§7.4).
    pub enum Strategy: "strategy" {
        /// Spend subscriptions evenly before their windows reset.
        Auto => "auto",
        /// Target shares, smooth weighted round-robin.
        Weighted => "weighted",
        /// The first available agent.
        Ordered => "ordered",
    }
}

openchef_core::string_enum! {
    /// A pipeline phase with its own routing (§7.3).
    pub enum Phase: "phase" {
        /// Planning.
        Plan => "plan",
        /// Implementing steps.
        Implement => "implement",
        /// Reviewing.
        Review => "review",
        /// Cheap helper work (handoff summaries).
        Helper => "helper",
    }
}

/// The content of `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Quota buckets, by name.
    pub pools: BTreeMap<String, Pool>,
    /// Named (runtime, model, pool) triples.
    pub agents: BTreeMap<String, Agent>,
    /// Agent selection per phase.
    pub routing: BTreeMap<Phase, PhaseRouting>,
    /// Quota tracking switches.
    pub quota: Quota,
}

/// One quota bucket the user owns.
#[derive(Debug, Clone, PartialEq)]
pub struct Pool {
    /// Who provides it.
    pub provider: Provider,
    /// How it is paid for.
    pub billing: Billing,
    /// The plan, when known.
    pub plan_tier: Option<PlanTier>,
    /// Sessions that may run on it at once.
    pub max_concurrent: u32,
    /// How effort adapts on this pool.
    pub effort_profile: EffortProfile,
    /// The environment variable holding the key (API pools only).
    pub api_key_env: Option<String>,
    /// Daily USD budget (API pools only, §10.3).
    pub daily_budget_usd: Option<f64>,
    /// Monthly USD budget (API pools only).
    pub monthly_budget_usd: Option<f64>,
}

/// A named (runtime, model, pool) triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// The CLI that runs it.
    pub runtime: Runtime,
    /// Passed to the runtime as-is; `None` uses the runtime's default model.
    pub model: Option<String>,
    /// The pool it spends.
    pub pool: String,
    /// Capability tier.
    pub tier: Tier,
    /// Pinned effort, if any.
    pub effort: Option<Effort>,
}

/// Agent selection for one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRouting {
    /// How to choose.
    pub strategy: Strategy,
    /// Eligible agents, in preference order.
    pub agents: Vec<String>,
    /// Target shares (`weighted` only).
    pub weights: BTreeMap<String, u32>,
}

/// Quota tracking switches (§10.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quota {
    /// L2 probes for every provider; `None` means on.
    pub probes: Option<bool>,
    /// Per-provider switches.
    pub providers: BTreeMap<Provider, bool>,
}

impl Quota {
    /// Whether L2 probes may run for `provider`. A global `false` wins over everything.
    #[must_use]
    pub fn probes_enabled(&self, provider: Provider) -> bool {
        if self.probes == Some(false) {
            return false;
        }
        self.providers.get(&provider).copied().unwrap_or(true)
    }
}

impl Config {
    /// Reads, checks and returns the configuration at `path`.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(Error::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Self::parse(&text).map_err(|e| e.in_file(path.to_path_buf()))
    }

    /// Parses and checks TOML content.
    pub fn parse(text: &str) -> Result<Self, Error> {
        raw::parse(text)
    }

    /// Encodes the configuration as TOML, in a stable key order meant to be read and
    /// edited by people.
    pub fn to_toml(&self) -> Result<String, Error> {
        raw::encode(self)
    }

    /// Writes the configuration to `path` atomically (a temporary file in the same
    /// directory, then a rename), readable only by the user on Unix.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        self.save_with_header(path, "")
    }

    /// [`Config::save`] with a comment block (or any text) written before the TOML.
    pub fn save_with_header(&self, path: &Path, header: &str) -> Result<(), Error> {
        let text = format!("{header}{}", self.to_toml()?);
        crate::paths::write_private_atomic(path, text.as_bytes())
    }
}

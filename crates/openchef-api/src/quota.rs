//! `quota/*`: what OpenChef knows about each pool's windows, and the probe switch
//! (§10, D-025).

use openchef_quota::{Health, State};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Client, Error};

/// Every pool's quota and every probe's state.
pub const METHOD_QUOTA_REPORT: &str = "quota/report";
/// Turns the usage probes on or off.
pub const METHOD_QUOTA_PROBES: &str = "quota/probes";

/// `quota/report`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaParams {
    /// Probe the providers first (each at most every 30 s).
    #[serde(default)]
    pub refresh: bool,
}

/// `quota/probes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbesParams {
    /// On or off; kept across restarts.
    pub on: bool,
}

/// One pool's quota as people read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolQuota {
    /// The merged state.
    #[serde(flatten)]
    pub state: State,
    /// Its health now.
    pub health: Health,
    /// When an exhausted pool takes work again.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub reset_at: Option<OffsetDateTime>,
}

/// One probe's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeStatus {
    /// `claude` or `chatgpt`.
    pub name: String,
    /// The pool it observes.
    pub pool: String,
    /// Why it cannot run, or why its last run failed; `None` when it is fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `quota/report`'s result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaResult {
    /// Known pools, by name.
    pub pools: Vec<PoolQuota>,
    /// Configured probes.
    pub probes: Vec<ProbeStatus>,
}

impl Client {
    /// Every pool's quota and every probe's state.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn quota(&self, p: &QuotaParams) -> Result<QuotaResult, Error> {
        self.call(METHOD_QUOTA_REPORT, p).await
    }

    /// Turns the usage probes on or off.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn set_probes(&self, p: &ProbesParams) -> Result<(), Error> {
        self.call::<serde_json::Value>(METHOD_QUOTA_PROBES, p)
            .await
            .map(|_| ())
    }
}

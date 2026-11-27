//! Quota windows, sources (L1/L2/L3), health and admission (`docs/architecture.md` §10).
//!
//! Observations from the CLIs' own streams (L1), out-of-band probes (L2) and estimates
//! (L3) are normalized to utilization in `[0, 1]` with absolute reset times, merged per
//! window, and turned into a health level and an admission decision the router uses.
//!
//! Everything here is plain state with `&mut self` methods and an explicit `now`: the
//! quota service owns it inside one actor (D-030), so nothing in this crate locks.

mod admission;
mod health;
mod probe;
mod providers;
mod state;

use std::path::PathBuf;

use time::OffsetDateTime;

pub use admission::{Admission, MARGIN, SMALL_STEP, admit, estimate, headroom, spent};
pub use health::{EXHAUSTED_AT, Health, RED_AT, STALE_AFTER, YELLOW_AT, health_of, reset_at};
pub use openchef_runtime::BoxFuture;
pub use probe::{Fetcher, HttpsFetcher, MIN_INTERVAL, Parser, Prober};
pub use providers::{
    CHATGPT_USAGE_URL, CLAUDE_USAGE_URL, ClaudeCredentials, CodexCredentials, Credentials, Login,
    Token, chatgpt_probe, claude_credentials, claude_probe, codex_credentials, parse_chatgpt,
    parse_claude,
};
pub use state::{Observation, Source, State, Tracker, Window, from_signal};

/// Why a probe, a credential read or a probe response could not be used. Messages never
/// contain tokens or request headers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A probe response lacks the fields OpenChef relies on. The probe disables itself
    /// (§10.2: fail closed) until the daemon restarts.
    #[error("quota: {probe} probe response changed shape ({reason}); probe disabled")]
    ShapeChanged {
        /// The probe ("claude", "chatgpt").
        probe: String,
        /// Which field was missing or malformed.
        reason: String,
    },
    /// The CLI's stored login was not found, has no usable token, or has expired.
    #[error("quota: no usable CLI login for the probe: {reason}")]
    NoCredentials {
        /// What was missing.
        reason: String,
    },
    /// A credential file exists but could not be read.
    #[error("quota: read {}: {reason}", path.display())]
    ReadCredentials {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        reason: String,
    },
    /// The probe is backing off (rate limiting, server errors, a rejected login, or
    /// its minimum interval).
    #[error("quota: probe is backing off until {until}")]
    Muted {
        /// When the probe may run again.
        until: OffsetDateTime,
    },
    /// The probe was turned off with the kill switch.
    #[error("quota: probe is disabled")]
    Disabled,
    /// The provider answered with a status other than 200.
    #[error("quota: {host} answered HTTP {code}")]
    Status {
        /// The provider's host.
        host: String,
        /// The HTTP status code.
        code: u16,
    },
    /// The request failed before a status arrived (connection, TLS, timeout, body).
    #[error("quota: request to {host}: {reason}")]
    Request {
        /// The provider's host.
        host: String,
        /// What failed.
        reason: String,
    },
    /// A probe URL is not an absolute `https` URL with a host.
    #[error("quota: probe URL {url:?} must be an https URL: {reason}")]
    InvalidUrl {
        /// The URL as given.
        url: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The probe was cancelled.
    #[error("quota: probe of {host} cancelled")]
    Cancelled {
        /// The provider's host.
        host: String,
    },
}

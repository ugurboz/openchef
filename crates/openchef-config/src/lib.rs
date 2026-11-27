//! OpenChef's configuration (`docs/architecture.md` §13): file locations, and loading,
//! validating and generating `config.toml` (pools, agents, routing, quota).
//!
//! Loading is "parse, don't validate": the file is read into loosely typed raw
//! structures, every problem is collected (unknown keys with their line, unknown values,
//! broken references), and only a fully valid file becomes a typed [`Config`].

mod defaults;
mod error;
mod model;
mod paths;
mod raw;

pub use defaults::default_config;
pub use error::Error;
pub use model::{
    Agent, Billing, Config, DEFAULT_MAX_CONCURRENT, EffortProfile, Phase, PhaseRouting, PlanTier,
    Pool, Provider, Quota, Runtime, Strategy,
};
pub use paths::{HOME_ENV, Paths};

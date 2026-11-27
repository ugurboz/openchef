//! Drives tasks through their steps (`docs/architecture.md` §7), ported from Go
//! `internal/engine`: it opens agent sessions on routed agents, runs turns with timeouts
//! and loop detection, verifies, commits, retries, fails over to another pool when
//! quota runs out, and parks a task in `waiting_quota` or `needs_attention` when it
//! cannot continue.

mod engine;
mod events;
mod prompts;
mod recover;
mod router;
mod runtimes;
mod session;
mod step;
mod turn;
pub mod verify;

pub use engine::{Clock, Config, Engine, Error, Limits, PoolExhausted, Sleep, SpecFor};
pub use events::{Discard, Event, EventKind, Sink};
pub use router::{Agent, Cooldown, NoAgent, Ordered, Router};
pub use runtimes::{DynRuntime, DynSession, Runtimes};

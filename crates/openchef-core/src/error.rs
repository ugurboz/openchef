//! The crate's error type.

use crate::machine::Entity;

/// Why a domain value or a state change was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A state machine does not allow this change.
    #[error("{entity} cannot move from {from:?} to {to:?}")]
    InvalidTransition {
        /// Which machine rejected it.
        entity: Entity,
        /// The current state.
        from: &'static str,
        /// The requested state.
        to: &'static str,
    },
    /// A string is not one of a vocabulary's values.
    #[error("unknown {kind} {value:?}")]
    UnknownValue {
        /// The vocabulary, e.g. "task status".
        kind: &'static str,
        /// The rejected input.
        value: String,
    },
    /// A value does not satisfy the domain rules.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The operating system could not provide randomness for a new id.
    #[error("cannot generate a task id: {0}")]
    Entropy(String),
}

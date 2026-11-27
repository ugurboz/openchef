//! The state machine contract shared by every status enum.

use std::fmt;

use crate::Error;

/// The kinds of entity that have a state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Entity {
    /// A task (§7.1).
    Task,
    /// A plan step.
    Step,
    /// An agent's attempt at a step.
    Attempt,
    /// An agent CLI process and its conversation.
    Session,
    /// One message sent to a session.
    Turn,
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Task => "task",
            Self::Step => "step",
            Self::Attempt => "attempt",
            Self::Session => "session",
            Self::Turn => "turn",
        })
    }
}

/// A status enum together with the states each status may move to.
///
/// `next` is written as an exhaustive `match`, so adding a state forces its edges to be
/// decided. A state with no successors is terminal.
pub trait StateMachine: Copy + Eq + 'static {
    /// The entity this machine governs, used in error messages.
    const ENTITY: Entity;

    /// The states this state may move to.
    fn next(self) -> &'static [Self];

    /// The fixed string form of the state.
    fn name(self) -> &'static str;

    /// Whether the state is final.
    fn is_terminal(self) -> bool {
        self.next().is_empty()
    }

    /// Accepts the change `self → to` or explains why it is not allowed.
    fn check_transition(self, to: Self) -> Result<(), Error> {
        if self.next().contains(&to) {
            Ok(())
        } else {
            Err(Error::InvalidTransition {
                entity: Self::ENTITY,
                from: self.name(),
                to: to.name(),
            })
        }
    }
}

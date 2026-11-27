//! The crate's error type.

use std::io;
use std::path::PathBuf;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested row does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A compare-and-set lost: the row is not in the state the caller expected (another
    /// writer moved it first, or the caller's view is stale).
    #[error("{entity} {key} is {got:?}, expected {want:?}")]
    Conflict {
        /// What kind of row.
        entity: &'static str,
        /// Which row.
        key: String,
        /// The state the caller expected.
        want: String,
        /// The state the row is in.
        got: String,
    },
    /// The request breaks a domain rule checked by the store.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A domain rule from `openchef-core` (invalid value or state transition).
    #[error(transparent)]
    Domain(#[from] openchef_core::Error),
    /// The migration set is malformed (bad name, duplicate or missing version): a build
    /// defect.
    #[error("invalid migration set: {0}")]
    InvalidMigration(String),
    /// The database was migrated by a newer OpenChef; this binary refuses to touch it.
    #[error("database schema version {found} is newer than this binary (knows up to {known})")]
    SchemaTooNew {
        /// The highest version in the database.
        found: i64,
        /// The highest version this binary has.
        known: i64,
    },
    /// An applied migration's content changed. Applied migrations are immutable.
    #[error("applied migration was modified: {0}")]
    MigrationModified(String),
    /// The database path is unusable.
    #[error("database path: {0}")]
    Path(String),
    /// Creating the database directory or file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// SQLite reported an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored value cannot be read back (a manual edit or a bug).
    #[error("corrupt value in {column}: {value:?}")]
    Corrupt {
        /// Where.
        column: &'static str,
        /// What was found.
        value: String,
    },
    /// Rolling back after an error failed as well.
    #[error("{cause} (and rollback failed: {source})")]
    Rollback {
        /// The error that caused the rollback.
        cause: Box<Error>,
        /// The rollback failure.
        #[source]
        source: rusqlite::Error,
    },
    /// The store is closed.
    #[error("the store is closed")]
    Closed,
}

impl Error {
    /// Whether this is a lost compare-and-set ([`Error::Conflict`]).
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// Whether the row does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    /// Whether a domain rule was broken (invalid value, not a state conflict).
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        matches!(
            self,
            Self::Invalid(_) | Self::Domain(openchef_core::Error::Invalid(_))
        )
    }

    /// Whether a state machine forbids the requested change.
    #[must_use]
    pub fn is_invalid_transition(&self) -> bool {
        matches!(
            self,
            Self::Domain(openchef_core::Error::InvalidTransition { .. })
        )
    }
}

//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Why a configuration could not be loaded, parsed or written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration file does not exist (run `openchef init`).
    #[error("configuration file not found: {} (run `openchef init`)", path.display())]
    NotFound {
        /// The missing file.
        path: PathBuf,
    },
    /// Reading a file failed.
    #[error("read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// Writing a file or creating a directory failed.
    #[error("write {}: {source}", path.display())]
    Write {
        /// The file or directory.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML, or a value has the wrong type.
    #[error("{}line {line}, column {column}: {message}", Origin(path.as_ref()))]
    Syntax {
        /// The file, when the content came from one.
        path: Option<PathBuf>,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// What the parser reported.
        message: String,
    },
    /// The file parsed but breaks the configuration rules. Every problem is listed, each
    /// prefixed with the TOML key it concerns.
    #[error("{}invalid configuration:\n  - {}", Origin(path.as_ref()), problems.join("\n  - "))]
    Invalid {
        /// The file, when the content came from one.
        path: Option<PathBuf>,
        /// One line per problem, in a stable order.
        problems: Vec<String>,
    },
    /// The configuration could not be encoded as TOML.
    #[error("encode configuration: {0}")]
    Encode(String),
    /// The user's home directory could not be determined.
    #[error("cannot locate the home directory; set {}", crate::HOME_ENV)]
    NoHome,
}

impl Error {
    /// Attaches the file the content came from.
    pub(crate) fn in_file(self, file: PathBuf) -> Self {
        match self {
            Self::Syntax {
                line,
                column,
                message,
                ..
            } => Self::Syntax {
                path: Some(file),
                line,
                column,
                message,
            },
            Self::Invalid { problems, .. } => Self::Invalid {
                path: Some(file),
                problems,
            },
            other @ (Self::NotFound { .. }
            | Self::Read { .. }
            | Self::Write { .. }
            | Self::Encode(_)
            | Self::NoHome) => other,
        }
    }
}

/// Formats an optional file as a `path: ` prefix.
struct Origin<'a>(Option<&'a PathBuf>);

impl fmt::Display for Origin<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(path) => write!(f, "{}: ", path.display()),
            None => Ok(()),
        }
    }
}

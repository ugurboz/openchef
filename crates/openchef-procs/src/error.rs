//! The crate's error type.

use std::io;
use std::path::PathBuf;

/// Why a process could not be started, signalled or inspected.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This platform has no supervision implementation.
    #[error("process supervision is not supported on this platform")]
    Unsupported,
    /// The spec names no program.
    #[error("empty program path")]
    EmptyProgram,
    /// The operating system refused to start the program.
    #[error("start {program}: {source}")]
    Spawn {
        /// The program that failed to start.
        program: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The supervisor thread could not be created or died during start.
    #[error("supervisor thread for {program}: {reason}")]
    Supervisor {
        /// The program being started.
        program: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// The process start token could not be read.
    #[error("read start token of pid {pid}: {source}")]
    StartToken {
        /// The process.
        pid: u32,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// A signal or kill request failed.
    #[error("signal {signal} to process group {pid}: {source}")]
    Signal {
        /// The group leader.
        pid: u32,
        /// The signal name.
        signal: &'static str,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The process did not exit even after a forced kill.
    #[error("pid {pid} did not exit after kill")]
    KillTimeout {
        /// The process.
        pid: u32,
    },
    /// Writing to the child's stdin failed or stdin is already closed.
    #[error("stdin of pid {pid} is closed")]
    StdinClosed {
        /// The process.
        pid: u32,
    },
    /// Writing to the child's stdin failed.
    #[error("write stdin of pid {pid}: {source}")]
    StdinWrite {
        /// The process.
        pid: u32,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// Inspecting a process failed.
    #[error("inspect pid {pid}: {source}")]
    Inspect {
        /// The process.
        pid: u32,
        /// The OS error.
        #[source]
        source: io::Error,
    },
}

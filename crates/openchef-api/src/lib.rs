//! The daemon's JSON-RPC 2.0 interface (`docs/architecture.md` §12.3): method names,
//! parameter and result types, the socket transport and a client. Messages are
//! newline-delimited JSON over a local socket. The API is versioned ([`VERSION`]) and
//! internal in v1: documented, not promised stable.

mod approvals;
mod client;
pub mod mcp;
mod quota;
mod tasks;
pub mod transport;

use std::path::PathBuf;

pub use approvals::*;
pub use client::Client;
use openchef_runtime::linerpc::RpcError;
pub use quota::*;
use serde::{Deserialize, Serialize};
pub use tasks::*;
use time::OffsetDateTime;

/// The protocol version a client and the daemon must agree on.
pub const VERSION: &str = "openchef.v1";

/// Must be the first call on a connection.
pub const METHOD_INITIALIZE: &str = "initialize";
/// The daemon's status.
pub const METHOD_STATUS: &str = "daemon/status";
/// Asks the daemon to shut down (§5.4).
pub const METHOD_STOP: &str = "daemon/stop";

/// A call before `initialize`.
pub const CODE_NOT_INITIALIZED: i64 = -32002;
/// Client and daemon speak different protocol versions.
pub const CODE_VERSION_MISMATCH: i64 = -32003;
/// Parameters that do not decode.
pub const CODE_INVALID_PARAMS: i64 = -32602;

/// Opens a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// [`VERSION`] as the client knows it.
    pub version: String,
    /// The caller, e.g. `openchef-cli 0.3.0`.
    pub client: String,
}

/// Describes the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// The protocol version.
    pub version: String,
    /// The daemon's build version.
    #[serde(rename = "daemonVersion")]
    pub daemon_version: String,
    /// The daemon's process id.
    pub pid: u32,
}

/// The result of `daemon/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon's process id.
    pub pid: u32,
    /// The daemon's build version.
    #[serde(rename = "daemonVersion")]
    pub daemon_version: String,
    /// When it started.
    #[serde(rename = "startedAt", with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// Its socket.
    pub socket: PathBuf,
    /// The OpenChef home it serves.
    pub home: PathBuf,
    /// Connected clients, this one included.
    pub connections: usize,
}

/// Why a client call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No daemon is listening on the socket.
    #[error("api: the OpenChef daemon is not running (start it with: openchef daemon start)")]
    NotRunning,
    /// Client and daemon speak different protocol versions, typically after an upgrade
    /// with the old daemon still running.
    #[error(
        "api: client and daemon versions differ ({0}); restart the daemon (openchef daemon stop)"
    )]
    VersionMismatch(String),
    /// The call failed.
    #[error("api: {0}")]
    Rpc(#[from] openchef_runtime::linerpc::Error),
    /// The socket could not be used.
    #[error("api: {context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The MCP server's input failed.
    #[error("api: mcp: {0}")]
    Mcp(String),
    /// This platform has no daemon socket yet (Windows: R3.3).
    #[error("api: the daemon socket is not available on this platform yet")]
    Unsupported,
}

/// A JSON-RPC error with `code`.
#[must_use]
pub fn rpc_error(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::{InitializeResult, Status};

    #[test]
    fn wire_names() {
        let s = Status {
            pid: 7,
            daemon_version: "0.1.0".into(),
            started_at: datetime!(2026-11-27 20:00 UTC),
            socket: "/h/daemon.sock".into(),
            home: "/h".into(),
            connections: 1,
        };
        let v = serde_json::to_value(&s).unwrap_or_default();
        assert_eq!(v["daemonVersion"], "0.1.0");
        assert_eq!(v["startedAt"], "2026-11-27T20:00:00Z");
        let back: Status = serde_json::from_value(v).unwrap_or_else(|_| s.clone());
        assert_eq!(back, s);
        let i: Result<InitializeResult, _> =
            serde_json::from_str(r#"{"version":"openchef.v1","daemonVersion":"x","pid":1}"#);
        assert!(i.is_ok());
    }
}

//! A client connection to the daemon.

use std::path::Path;

use openchef_runtime::linerpc::{self, Notification, Options};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::transport::{Connection, dial};
use crate::{
    CODE_VERSION_MISMATCH, Error, InitializeParams, InitializeResult, METHOD_INITIALIZE,
    METHOD_STATUS, METHOD_STOP, Status, VERSION,
};

/// One connection to the daemon.
#[derive(Debug)]
pub struct Client {
    connection: Connection,
    /// The daemon, from the handshake.
    pub daemon: InitializeResult,
}

impl Client {
    /// Connects to the daemon's socket and performs the version handshake.
    ///
    /// # Errors
    ///
    /// [`Error::NotRunning`], [`Error::VersionMismatch`], or a failed call.
    pub async fn dial(socket: &Path, client: &str) -> Result<Self, Error> {
        let connection = dial(socket, Options::default()).await?;
        let params = InitializeParams {
            version: VERSION.to_owned(),
            client: client.to_owned(),
        };
        let daemon = match connection
            .peer
            .call(METHOD_INITIALIZE, params, &CancellationToken::new())
            .await
        {
            Ok(d) => d,
            Err(linerpc::Error::Rpc { error, .. }) if error.code == CODE_VERSION_MISMATCH => {
                connection.shutdown();
                return Err(Error::VersionMismatch(error.message));
            }
            Err(e) => {
                connection.shutdown();
                return Err(e.into());
            }
        };
        Ok(Self { connection, daemon })
    }

    /// Invokes `method` and decodes its result.
    ///
    /// # Errors
    ///
    /// When the call fails or the result does not decode.
    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        Ok(self
            .connection
            .peer
            .call(method, params, &CancellationToken::new())
            .await?)
    }

    /// The daemon's status.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub async fn status(&self) -> Result<Status, Error> {
        self.call(METHOD_STATUS, serde_json::Value::Null).await
    }

    /// Asks the daemon to shut down. Returns once it accepted; it then stops its
    /// sessions and exits.
    ///
    /// # Errors
    ///
    /// When the call fails (other than the daemon closing the connection meanwhile).
    pub async fn stop(&self) -> Result<(), Error> {
        match self
            .call::<serde_json::Value>(METHOD_STOP, serde_json::Value::Null)
            .await
        {
            Ok(_) | Err(Error::Rpc(linerpc::Error::Closed { .. })) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The next server notification; `None` once the connection closed.
    pub async fn notification(&mut self) -> Option<Notification> {
        self.connection.notifications.recv().await
    }

    /// Fires when the daemon closes the connection.
    #[must_use]
    pub fn closed(&self) -> CancellationToken {
        self.connection.peer.closed()
    }

    /// Ends the connection.
    pub fn close(self) {
        self.connection.shutdown();
    }
}

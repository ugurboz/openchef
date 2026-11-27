//! The daemon's API server (§5, §12.3): the single-instance lock, the JSON-RPC API on a
//! local socket, and an orderly shutdown.
// Until the Windows socket lands (R3.3) the server itself is Unix only; the types
// stay so callers compile everywhere.
#![cfg_attr(not(unix), allow(dead_code, unused_imports, clippy::unused_async))]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openchef_api::{
    CODE_INVALID_PARAMS, CODE_NOT_INITIALIZED, CODE_VERSION_MISMATCH, InitializeParams,
    InitializeResult, METHOD_INITIALIZE, METHOD_STATUS, METHOD_STOP, Status, VERSION, rpc_error,
};
use openchef_config::Paths;
use openchef_runtime::BoxFuture;
use openchef_runtime::linerpc::{
    CODE_METHOD_NOT_FOUND, Handler, Options as PeerOptions, Peer, RpcError,
};
use serde_json::Value;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{Error, Lock};

/// Bounds the shutdown work of `on_stop` (§5.4: graceful interrupt, then 10 s).
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The portable limit of a Unix socket path (`sun_path` is 104 bytes on macOS, 108 on
/// Linux, including the terminating NUL).
const MAX_SOCKET_PATH: usize = 103;

/// One client connection, as a method handler sees it.
#[derive(Debug, Clone)]
pub struct Conn {
    id: u64,
    peer: Peer,
}

impl Conn {
    /// Identifies the connection within this daemon.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Fires when the client disconnects.
    #[must_use]
    pub fn closed(&self) -> CancellationToken {
        self.peer.closed()
    }

    /// Sends a server notification (e.g. `event/appended`) to this client.
    ///
    /// # Errors
    ///
    /// When the client is gone.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), Error> {
        self.peer
            .notify(method, params)
            .await
            .map_err(|e| Error::Notify {
                id: self.id,
                reason: e.to_string(),
            })
    }
}

/// Handles one API method on one connection. `cancel` fires when the client goes.
pub type Method = Arc<
    dyn Fn(Conn, Value, CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>>
        + Send
        + Sync,
>;

/// Runs once on shutdown, before connections close, to stop sessions gracefully.
pub type OnStop = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), String>> + Send>;

/// How to run the daemon.
pub struct Options {
    /// Where everything lives.
    pub paths: Paths,
    /// The daemon's build version, reported to clients.
    pub version: String,
    /// API methods beyond `initialize`, `daemon/status` and `daemon/stop`.
    pub methods: HashMap<String, Method>,
    /// Bounded by [`STOP_TIMEOUT`].
    pub on_stop: Option<OnStop>,
    /// Stops the daemon from outside (signals, tests).
    pub shutdown: CancellationToken,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("paths", &self.paths)
            .field("version", &self.version)
            .field("methods", &self.methods.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Connection bookkeeping: a small leaf value, never held across `.await` (D-030).
#[derive(Default)]
struct Registry {
    next: u64,
    open: HashMap<u64, openchef_api::transport::Connection>,
    initialized: HashSet<u64>,
}

struct Server {
    paths: Paths,
    version: String,
    started: OffsetDateTime,
    methods: HashMap<String, Method>,
    stop: CancellationToken,
    registry: Mutex<Registry>,
}

impl Server {
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> Status {
        Status {
            pid: std::process::id(),
            daemon_version: self.version.clone(),
            started_at: self.started,
            socket: self.paths.socket.clone(),
            home: self.paths.home.clone(),
            connections: self.registry().open.len(),
        }
    }

    fn handle(
        self: &Arc<Self>,
        conn: Conn,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ready = |r: Result<Value, RpcError>| -> BoxFuture<'static, Result<Value, RpcError>> {
            Box::pin(async move { r })
        };
        if method == METHOD_INITIALIZE {
            return ready(self.initialize(conn.id, params));
        }
        if !self.registry().initialized.contains(&conn.id) {
            return ready(Err(rpc_error(
                CODE_NOT_INITIALIZED,
                format!("call {METHOD_INITIALIZE} first"),
            )));
        }
        match method {
            METHOD_STATUS => ready(
                serde_json::to_value(self.status()).map_err(|e| RpcError::server(e.to_string())),
            ),
            METHOD_STOP => {
                self.stop.cancel();
                ready(Ok(Value::Object(serde_json::Map::new())))
            }
            other => match self.methods.get(other) {
                Some(m) => m(conn, params, cancel),
                None => ready(Err(rpc_error(
                    CODE_METHOD_NOT_FOUND,
                    format!("unknown method {other:?}"),
                ))),
            },
        }
    }

    fn initialize(&self, id: u64, params: Value) -> Result<Value, RpcError> {
        let p: InitializeParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(CODE_INVALID_PARAMS, format!("initialize: {e}")))?;
        if p.version != VERSION {
            return Err(rpc_error(
                CODE_VERSION_MISMATCH,
                format!("daemon speaks {VERSION}, client {}", p.version),
            ));
        }
        let _new = self.registry().initialized.insert(id);
        let result = InitializeResult {
            version: VERSION.to_owned(),
            daemon_version: self.version.clone(),
            pid: std::process::id(),
        };
        serde_json::to_value(result).map_err(|e| RpcError::server(e.to_string()))
    }

    /// Registers an accepted stream and wires its handler.
    #[cfg(unix)]
    fn adopt(self: &Arc<Self>, stream: std::os::unix::net::UnixStream) {
        let id = {
            let mut r = self.registry();
            r.next += 1;
            r.next
        };
        // The peer starts dispatching before `from_unix` returns, so a request can
        // arrive before the peer is known; its handler waits for it.
        let (ready_tx, ready_rx) = tokio::sync::watch::channel::<Option<Peer>>(None);
        let handler: Handler = {
            let server = Arc::clone(self);
            Arc::new(move |method, params, cancel| {
                let (server, mut ready) = (Arc::clone(&server), ready_rx.clone());
                Box::pin(async move {
                    let peer = tokio::select! {
                        got = ready.wait_for(Option::is_some) => match got {
                            Ok(p) => p.clone(),
                            Err(_) => None,
                        },
                        () = cancel.cancelled() => None,
                    };
                    let Some(peer) = peer else {
                        return Err(RpcError::server("connection closed before it was ready"));
                    };
                    server
                        .handle(Conn { id, peer }, &method, params, cancel)
                        .await
                })
            })
        };
        let options = PeerOptions {
            handler: Some(handler),
            ..PeerOptions::default()
        };
        let connection = match openchef_api::transport::from_unix(stream, options) {
            Ok(c) => c,
            Err(e) => return eprintln_note(&format!("daemon: open connection: {e}")),
        };
        // No receiver left only means no request arrived yet; the peer is kept anyway.
        let _ready = ready_tx.send(Some(connection.peer.clone()));
        let closed = connection.peer.closed();
        drop(self.registry().open.insert(id, connection));
        let server = Arc::clone(self);
        drop(tokio::spawn(async move {
            closed.cancelled().await;
            drop(ready_tx); // kept until the connection ends, so late handlers see the peer
            server.drop_conn(id);
        }));
    }

    fn drop_conn(&self, id: u64) {
        let mut r = self.registry();
        let _was = r.initialized.remove(&id);
        if let Some(c) = r.open.remove(&id) {
            c.shutdown();
        }
    }
}

/// Operational messages go to the daemon log (its stderr is redirected there).
fn eprintln_note(message: &str) {
    use std::io::Write as _;
    // The log is best effort: a failed write must not take the daemon down.
    let _logged = writeln!(std::io::stderr(), "{message}");
}

/// Starts the daemon and serves until `shutdown` fires or a client calls `daemon/stop`.
///
/// # Errors
///
/// [`Error::Running`] when another daemon holds the lock; set-up failures.
pub async fn run(options: Options) -> Result<(), Error> {
    let Options {
        paths,
        version,
        methods,
        on_stop,
        shutdown,
    } = options;
    std::fs::create_dir_all(&paths.home)
        .map_err(|e| Error::io(format!("create {}", paths.home.display()), e))?;
    let socket_len = paths.socket.as_os_str().len();
    if socket_len > MAX_SOCKET_PATH {
        return Err(Error::SocketPathTooLong {
            path: paths.socket.clone(),
            len: socket_len,
            max: MAX_SOCKET_PATH,
        });
    }
    let mut lock = Lock::acquire(&paths.lock)?;
    let served = serve(paths, version, methods, on_stop, shutdown).await;
    let released = lock.release();
    served.and(released)
}

#[cfg(unix)]
async fn serve(
    paths: Paths,
    version: String,
    methods: HashMap<String, Method>,
    on_stop: Option<OnStop>,
    shutdown: CancellationToken,
) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;

    // Holding the lock, any socket file left behind belongs to a dead daemon.
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io("remove stale socket", e)),
    }
    let listener = tokio::net::UnixListener::bind(&paths.socket)
        .map_err(|e| Error::io(format!("listen on {}", paths.socket.display()), e))?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::io("restrict socket", e))?;
    let stop = shutdown.child_token();
    let server = Arc::new(Server {
        paths,
        version,
        started: OffsetDateTime::now_utc(),
        methods,
        stop: stop.clone(),
        registry: Mutex::new(Registry::default()),
    });
    eprintln_note(&format!(
        "daemon started: pid {} socket {}",
        std::process::id(),
        server.paths.socket.display()
    ));
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => match stream.into_std() {
                    Ok(std_stream) => server.adopt(std_stream),
                    Err(e) => eprintln_note(&format!("daemon: accept: {e}")),
                },
                Err(e) => {
                    eprintln_note(&format!("daemon: accept: {e}"));
                    break;
                }
            },
        }
    }
    drop(listener);
    shutdown_server(&server, on_stop).await
}

#[cfg(not(unix))]
async fn serve(
    paths: Paths,
    _version: String,
    _methods: HashMap<String, Method>,
    _on_stop: Option<OnStop>,
    _shutdown: CancellationToken,
) -> Result<(), Error> {
    drop(paths);
    Err(Error::Unsupported)
}

/// Lets `on_stop` stop the sessions, closes every connection and removes the socket
/// (§5.4).
#[cfg(unix)]
async fn shutdown_server(server: &Arc<Server>, on_stop: Option<OnStop>) -> Result<(), Error> {
    eprintln_note("daemon stopping");
    let mut result = Ok(());
    if let Some(stop) = on_stop {
        match tokio::time::timeout(STOP_TIMEOUT, stop()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => result = Err(Error::Stop(e)),
            Err(_) => {
                result = Err(Error::Stop(format!(
                    "sessions did not stop within {STOP_TIMEOUT:?}"
                )));
            }
        }
    }
    let ids: Vec<u64> = server.registry().open.keys().copied().collect();
    for id in ids {
        server.drop_conn(id);
    }
    match std::fs::remove_file(&server.paths.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => result = result.and(Err(Error::io("remove socket", e))),
    }
    eprintln_note("daemon stopped");
    result
}

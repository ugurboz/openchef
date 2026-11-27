//! A JSON-RPC peer over the daemon's local socket: blocking reads on the peer's reader
//! thread, writes on a writer thread in the order they were awaited.

use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use openchef_runtime::BoxFuture;
use openchef_runtime::linerpc::{LineSink, Notifications, Options, Peer};
use tokio::sync::oneshot;

use crate::Error;

type Line = (Vec<u8>, oneshot::Sender<io::Result<()>>);

/// Writes lines to a stream from one thread, so the order of awaited sends holds.
pub struct SocketSink {
    tx: mpsc::Sender<Line>,
}

impl std::fmt::Debug for SocketSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketSink").finish_non_exhaustive()
    }
}

impl SocketSink {
    /// Starts the writer thread for `stream`.
    ///
    /// # Errors
    ///
    /// When the thread cannot be started.
    pub fn start(mut stream: impl io::Write + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Line>();
        thread::Builder::new()
            .name("openchef-socket-writer".to_owned())
            .spawn(move || {
                for (mut line, done) in rx {
                    line.push(b'\n');
                    let result = stream.write_all(&line).and_then(|()| stream.flush());
                    let failed = result.is_err();
                    drop(done.send(result));
                    if failed {
                        return;
                    }
                }
            })?;
        Ok(Self { tx })
    }
}

impl LineSink for SocketSink {
    fn send_line(&self, line: Vec<u8>) -> BoxFuture<'static, io::Result<()>> {
        let (done, result) = oneshot::channel();
        let sent = self.tx.send((line, done));
        Box::pin(async move {
            if sent.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection closed",
                ));
            }
            result.await.unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection closed",
                ))
            })
        })
    }
}

/// An open connection: the peer and a handle to shut the socket down.
pub struct Connection {
    /// The JSON-RPC peer.
    pub peer: Peer,
    /// Server notifications, in order.
    pub notifications: Notifications,
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Closes both directions; the peer's reader then ends.
    pub fn shutdown(&self) {
        // Shutting down an already closed socket fails harmlessly.
        #[cfg(unix)]
        drop(self.stream.shutdown(std::net::Shutdown::Both));
    }
}

/// Wraps an accepted or dialled Unix stream in a peer.
///
/// # Errors
///
/// When the stream cannot be cloned or the writer thread not started.
#[cfg(unix)]
pub fn from_unix(
    stream: std::os::unix::net::UnixStream,
    options: Options,
) -> io::Result<Connection> {
    stream.set_nonblocking(false)?;
    let reader = stream.try_clone()?;
    let writer = stream.try_clone()?;
    let (peer, notifications) = Peer::new(
        reader,
        std::sync::Arc::new(SocketSink::start(writer)?),
        options,
    );
    Ok(Connection {
        peer,
        notifications,
        stream,
    })
}

/// Connects to the daemon's socket.
///
/// # Errors
///
/// [`Error::NotRunning`] when nothing listens there; [`Error::Unsupported`] off Unix.
pub async fn dial(socket: &Path, options: Options) -> Result<Connection, Error> {
    // Checked first: a missing socket file surfaces as different errors per OS.
    if !socket.exists() {
        return Err(Error::NotRunning);
    }
    #[cfg(unix)]
    {
        let path = socket.to_path_buf();
        let stream =
            tokio::task::spawn_blocking(move || std::os::unix::net::UnixStream::connect(path))
                .await
                .map_err(|e| Error::Io {
                    context: "connect task".into(),
                    source: io::Error::other(e),
                })?
                .map_err(|e| {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) {
                        Error::NotRunning
                    } else {
                        Error::Io {
                            context: format!("connect {}", socket.display()),
                            source: e,
                        }
                    }
                })?;
        from_unix(stream, options).map_err(|e| Error::Io {
            context: "open connection".into(),
            source: e,
        })
    }
    #[cfg(not(unix))]
    {
        drop(options);
        Err(Error::Unsupported)
    }
}

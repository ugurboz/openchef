//! Opening the database, the writer actor and the read pool.

use std::fs::{self, OpenOptions as FsOpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};

use crate::Error;
use crate::migrate::{self, Migration};

/// Stamps `created_at`, `updated_at` and similar columns. Tests replace it.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// How long a connection waits for a lock before failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Read connections kept for reuse.
const READ_POOL: usize = 4;
/// Queued writes before `write` waits for room.
const WRITE_QUEUE: usize = 256;

/// Options for [`Store::open_with`].
#[derive(Clone, Default)]
pub struct OpenOptions {
    /// Migrations to apply instead of the embedded ones (tests).
    pub migrations: Option<Vec<Migration>>,
    /// Clock instead of the system time (tests).
    pub clock: Option<Clock>,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

enum Msg {
    Job(Job),
    Close(oneshot::Sender<()>),
}

struct Inner {
    path: PathBuf,
    writer: mpsc::Sender<Msg>,
    // A pool of idle read-only connections: a leaf value, locked only to take or return
    // a connection, never across an `.await` (D-030).
    readers: Mutex<Vec<Connection>>,
    clock: Clock,
}

/// An open state database. Cheap to clone; all clones share one writer and one read
/// pool.
///
/// Concurrency model (D-030): one writer thread owns the only read-write connection and
/// runs every write transaction (`BEGIN IMMEDIATE`) in arrival order, so writers queue
/// instead of failing on lock upgrades; reads use separate `query_only` connections and
/// run concurrently on Tokio's blocking pool. A write whose caller stops waiting still
/// runs to completion or rollback: transactions are atomic either way.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if necessary) the database at `path` and applies pending
    /// migrations. The parent directory and the file are created owner-only on Unix.
    pub async fn open(path: &Path) -> Result<Self, Error> {
        Self::open_with(path, OpenOptions::default()).await
    }

    /// [`Store::open`] with custom migrations or clock.
    pub async fn open_with(path: &Path, options: OpenOptions) -> Result<Self, Error> {
        if path.as_os_str().is_empty() {
            return Err(Error::Path("empty database path".to_owned()));
        }
        let path = std::path::absolute(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let migrations = options.migrations.unwrap_or_else(migrate::embedded);
        let open_path = path.clone();
        let writer = tokio::task::spawn_blocking(move || -> Result<Connection, Error> {
            ensure_file(&open_path)?;
            let mut conn = open_writer(&open_path)?;
            migrate::migrate(&mut conn, &migrations)?;
            Ok(conn)
        })
        .await
        .map_err(|e| Error::Path(format!("open task failed: {e}")))??;
        let reader = open_reader(&path)?;

        let (tx, rx) = mpsc::channel(WRITE_QUEUE);
        thread::Builder::new()
            .name("openchef-store-writer".to_owned())
            .spawn(move || run_writer(writer, rx))
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        let clock = options
            .clock
            .unwrap_or_else(|| Arc::new(OffsetDateTime::now_utc));
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                writer: tx,
                readers: Mutex::new(vec![reader]),
                clock,
            }),
        })
    }

    /// The absolute path of the database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The current time from the store's clock.
    pub(crate) fn now(&self) -> OffsetDateTime {
        (self.inner.clock)()
    }

    /// Runs `f` in an immediate write transaction on the writer thread. It commits if `f`
    /// returns `Ok` and rolls back otherwise, returning `f`'s error unchanged.
    pub async fn write<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, Error> + Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            // The caller may have stopped waiting; the transaction outcome stands.
            drop(reply.send(run_tx(conn, f)));
        });
        self.inner
            .writer
            .send(Msg::Job(job))
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    /// Runs `f` on a read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, Error> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let pooled = inner
                .readers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => open_reader(&inner.path)?,
            };
            let result = f(&conn);
            let mut pool = inner.readers.lock().unwrap_or_else(PoisonError::into_inner);
            if pool.len() < READ_POOL {
                pool.push(conn);
            }
            result
        })
        .await
        .map_err(|e| Error::Path(format!("read task failed: {e}")))?
    }

    /// The highest applied migration version.
    pub async fn schema_version(&self) -> Result<i64, Error> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Stops the writer after the writes already queued and waits until its connection
    /// is closed. Other clones fail with [`Error::Closed`] afterwards.
    pub async fn close(self) -> Result<(), Error> {
        let (done, closed) = oneshot::channel();
        if self.inner.writer.send(Msg::Close(done)).await.is_err() {
            return Ok(()); // already closed
        }
        // The writer drops `done` only after closing its connection.
        drop(closed.await);
        self.inner
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        Ok(())
    }
}

fn run_tx<T>(
    conn: &mut Connection,
    f: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
) -> Result<T, Error> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match f(&tx) {
        Ok(value) => {
            tx.commit()?;
            Ok(value)
        }
        Err(cause) => match tx.rollback() {
            Ok(()) => Err(cause),
            Err(source) => Err(Error::Rollback {
                cause: Box::new(cause),
                source,
            }),
        },
    }
}

fn run_writer(mut conn: Connection, mut rx: mpsc::Receiver<Msg>) {
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            Msg::Job(job) => job(&mut conn),
            Msg::Close(done) => {
                drop(conn.close());
                drop(done);
                return;
            }
        }
    }
}

/// Creates the directory (0700) and an empty database file (0600). SQLite gives the
/// `-wal` and `-shm` files the database file's permissions.
fn ensure_file(path: &Path) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        let _ = builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let _ = builder.mode(0o700);
        }
        builder.create(dir).map_err(io)?;
    }
    let mut options = FsOpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(io(e)),
    }
}

fn open_writer(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::Path(format!("journal mode is {mode:?}, want wal")));
    }
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn open_reader(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA query_only = ON;")?;
    Ok(conn)
}

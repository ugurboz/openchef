//! Records every protocol line exchanged with an agent CLI as JSONL, in the format of
//! the fixtures in `testdata/`, so a live session can be diagnosed and replayed.
//!
//! Record: `{"ms": <since start>, "dir": "send"|"recv"|"stderr"|"note", "msg": <JSON>}`,
//! or `"text": "…"` instead of `msg` for lines that are not JSON.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The direction of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Written to the CLI.
    Send,
    /// Read from the CLI.
    Recv,
    /// A line of the CLI's stderr.
    Stderr,
    /// An adapter observation.
    Note,
    /// The command line a process was started with (recorded by the spikes).
    Argv,
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Milliseconds since the log was opened.
    pub ms: u64,
    /// Direction.
    pub dir: Dir,
    /// The line, when it was JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<Value>,
    /// The line, when it was not JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

struct Inner {
    // One file written by several threads (reader, writer, stderr): a leaf lock, held
    // only for one write and never across an `.await` (D-030).
    file: Mutex<Writer>,
    start: Instant,
}

struct Writer {
    file: Box<dyn io::Write + Send>,
    error: Option<String>,
}

/// An append-only protocol log. Cheap to clone. [`RawLog::discard`] logs nothing, so
/// adapters log unconditionally. Logging never interrupts a session: the first write
/// error is remembered and reported by [`RawLog::error`].
#[derive(Clone)]
pub struct RawLog {
    inner: Option<Arc<Inner>>,
}

impl std::fmt::Debug for RawLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawLog")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

impl RawLog {
    /// A log that records nothing.
    #[must_use]
    pub const fn discard() -> Self {
        Self { inner: None }
    }

    /// Creates (or appends to) `path`, owner-only on Unix.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut options = OpenOptions::new();
        let _ = options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let _ = options.mode(0o600);
        }
        let file: File = options.open(path)?;
        Ok(Self::to_writer(Box::new(file)))
    }

    /// Writes to `w` (tests).
    #[must_use]
    pub fn to_writer(w: Box<dyn io::Write + Send>) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                file: Mutex::new(Writer {
                    file: w,
                    error: None,
                }),
                start: Instant::now(),
            })),
        }
    }

    /// Records one protocol line; valid JSON is embedded as is.
    pub fn line(&self, dir: Dir, line: &[u8]) {
        let Some(inner) = &self.inner else { return };
        let (msg, text) = match serde_json::from_slice::<Value>(line) {
            Ok(v) => (Some(v), None),
            Err(_) => (None, Some(String::from_utf8_lossy(line).into_owned())),
        };
        Self::write(
            inner,
            &Record {
                ms: elapsed_ms(inner),
                dir,
                msg,
                text,
            },
        );
    }

    /// Records free text (a stderr line, an observation).
    pub fn note(&self, dir: Dir, text: &str) {
        let Some(inner) = &self.inner else { return };
        Self::write(
            inner,
            &Record {
                ms: elapsed_ms(inner),
                dir,
                msg: None,
                text: Some(text.to_owned()),
            },
        );
    }

    fn write(inner: &Inner, record: &Record) {
        let mut w = inner.file.lock().unwrap_or_else(PoisonError::into_inner);
        if w.error.is_some() {
            return;
        }
        let result = serde_json::to_vec(record)
            .map_err(|e| e.to_string())
            .and_then(|mut raw| {
                raw.push(b'\n');
                w.file
                    .write_all(&raw)
                    .and_then(|()| w.file.flush())
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            w.error = Some(e);
        }
    }

    /// The first write error, if any.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        self.inner.as_ref().and_then(|i| {
            i.file
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .error
                .clone()
        })
    }
}

fn elapsed_ms(inner: &Inner) -> u64 {
    u64::try_from(inner.start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Decodes a log or a `testdata/` fixture.
pub fn read_file(path: &Path) -> io::Result<Vec<Record>> {
    let reader = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    for (n, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} line {}: {e}", path.display(), n + 1),
            )
        })?;
        out.push(record);
    }
    Ok(out)
}

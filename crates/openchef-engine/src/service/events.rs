//! Every engine event, persisted to the task's `events.jsonl` (for `logs` and late
//! viewers) and fanned out to live subscribers. Each task has its own lock, so one
//! task's viewers never slow another task's engine.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openchef_core::TaskId;
use tokio::sync::mpsc;

use crate::events::{Event, Sink};

/// How far a slow viewer may fall behind before it is cut off; the engine never waits
/// for viewers.
pub const SUBSCRIBER_BUFFER: usize = 1024;

/// Reports a failure to write the log; the event still reaches live viewers.
pub type OnFail = Arc<dyn Fn(String) + Send + Sync>;

/// One viewer of a task's events.
#[derive(Debug)]
pub struct Subscription {
    /// Events in order; ends (`None`) when the subscription ends.
    pub rx: mpsc::Receiver<Event>,
    task: TaskId,
    id: u64,
    lagged: Arc<AtomicBool>,
}

impl Subscription {
    /// Whether it ended because the viewer could not keep up. The viewer can watch
    /// again from the start: the log has everything.
    #[must_use]
    pub fn lagged(&self) -> bool {
        self.lagged.load(Ordering::SeqCst)
    }
}

struct Subscriber {
    id: u64,
    tx: mpsc::Sender<Event>,
    lagged: Arc<AtomicBool>,
}

/// One task's log file and subscribers.
#[derive(Default)]
struct TaskLog {
    file: Option<File>,
    /// Lines in the file, counted once on open and kept up to date.
    lines: Option<usize>,
    subs: Vec<Subscriber>,
}

/// The event logs of all tasks. Locks are leaf values, never held across `.await`.
pub struct EventLog {
    dir: PathBuf,
    on_fail: OnFail,
    tasks: Mutex<HashMap<TaskId, Arc<Mutex<TaskLog>>>>,
    next: Mutex<u64>,
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl EventLog {
    /// Logs below `dir` (`~/.openchef/sessions`): `<dir>/<task>/events.jsonl`.
    #[must_use]
    pub fn new(dir: PathBuf, on_fail: OnFail) -> Self {
        Self {
            dir,
            on_fail,
            tasks: Mutex::new(HashMap::new()),
            next: Mutex::new(0),
        }
    }

    /// Where a task's log is.
    #[must_use]
    pub fn path(&self, task: &TaskId) -> PathBuf {
        self.dir.join(task.as_str()).join("events.jsonl")
    }

    fn log(&self, task: &TaskId) -> Arc<Mutex<TaskLog>> {
        Arc::clone(lock(&self.tasks).entry(task.clone()).or_default())
    }

    fn append(&self, task: &TaskId, l: &mut TaskLog, line: &[u8]) -> Result<(), String> {
        if l.file.is_none() {
            let path = self.path(task);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("create event log directory: {e}"))?;
            }
            if l.lines.is_none() {
                l.lines = Some(count_lines(&path)?);
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&path)
                .map_err(|e| format!("open event log: {e}"))?;
            terminate_last_line(&mut file)?;
            l.file = Some(file);
        }
        let Some(file) = l.file.as_mut() else {
            return Ok(());
        };
        let mut buf = line.to_vec();
        buf.push(b'\n');
        file.write_all(&buf)
            .map_err(|e| format!("write event log: {e}"))?;
        l.lines = Some(l.lines.unwrap_or(0) + 1);
        Ok(())
    }

    /// The task's past events (with `replay`) and a subscription to new ones: no event
    /// is missed or delivered twice. The subscription is registered under the task's
    /// lock together with the line count; the log is read outside the lock, up to that
    /// count, so a long replay never holds up the engine.
    ///
    /// # Errors
    ///
    /// When the log cannot be read.
    pub fn subscribe(
        &self,
        task: &TaskId,
        replay: bool,
    ) -> Result<(Vec<Event>, Subscription), String> {
        let path = self.path(task);
        let (tx, rx) = mpsc::channel(SUBSCRIBER_BUFFER);
        let lagged = Arc::new(AtomicBool::new(false));
        let id = {
            let mut n = lock(&self.next);
            *n += 1;
            *n
        };
        let log = self.log(task);
        let up_to = {
            let mut l = lock(&log);
            let lines = if let Some(n) = l.lines {
                n
            } else {
                let n = count_lines(&path)?;
                l.lines = Some(n);
                n
            };
            l.subs.push(Subscriber {
                id,
                tx,
                lagged: Arc::clone(&lagged),
            });
            lines
        };
        let sub = Subscription {
            rx,
            task: task.clone(),
            id,
            lagged,
        };
        if !replay {
            return Ok((Vec::new(), sub));
        }
        match read_log(&path, up_to) {
            Ok(past) => Ok((past, sub)),
            Err(e) => {
                self.unsubscribe(&sub);
                Err(e)
            }
        }
    }

    /// Ends a subscription.
    pub fn unsubscribe(&self, sub: &Subscription) {
        lock(&self.log(&sub.task)).subs.retain(|s| s.id != sub.id);
    }

    /// Whether anyone watches the task live.
    #[must_use]
    pub fn watched(&self, task: &TaskId) -> bool {
        !lock(&self.log(task)).subs.is_empty()
    }

    /// Releases a finished task's log file; a later event reopens it.
    pub fn close_task(&self, task: &TaskId) {
        lock(&self.log(task)).file = None;
    }

    /// Ends every subscription and closes every log.
    pub fn close(&self) {
        let logs: Vec<Arc<Mutex<TaskLog>>> = lock(&self.tasks).drain().map(|(_, l)| l).collect();
        for l in logs {
            let mut l = lock(&l);
            l.file = None;
            l.subs.clear();
        }
    }
}

impl Sink for EventLog {
    /// Append, then fan out, under the task's lock.
    fn publish(&self, event: Event) {
        let line = match serde_json::to_vec(&event) {
            Ok(l) => l,
            Err(e) => return (self.on_fail)(format!("encode event: {e}")),
        };
        let log = self.log(&event.task);
        let mut l = lock(&log);
        if let Err(e) = self.append(&event.task, &mut l, &line) {
            (self.on_fail)(e);
        }
        l.subs.retain(|s| match s.tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                s.lagged.store(true, Ordering::SeqCst);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// Ends a line a crash cut short, so the next event starts its own line instead of
/// merging into the broken one (which `read_log` skips but counts).
fn terminate_last_line(file: &mut File) -> Result<(), String> {
    let len = file
        .metadata()
        .map_err(|e| format!("stat event log: {e}"))?
        .len();
    if len == 0 {
        return Ok(());
    }
    let mut last = [0_u8; 1];
    file.seek(SeekFrom::Start(len - 1))
        .and_then(|_| file.read_exact(&mut last))
        .map_err(|e| format!("read event log: {e}"))?;
    if last[0] != b'\n' {
        file.write_all(b"\n")
            .map_err(|e| format!("write event log: {e}"))?;
    }
    Ok(())
}

fn count_lines(path: &Path) -> Result<usize, String> {
    match File::open(path) {
        Ok(f) => Ok(BufReader::new(f).split(b'\n').count()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("open event log: {e}")),
    }
}

/// The events on the first `up_to` lines; lines that do not decode (cut short by a
/// crash) are skipped but counted.
fn read_log(path: &Path, up_to: usize) -> Result<Vec<Event>, String> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("open event log: {e}")),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).split(b'\n').take(up_to) {
        let line = line.map_err(|e| format!("read event log: {e}"))?;
        if let Ok(ev) = serde_json::from_slice(&line) {
            out.push(ev);
        }
    }
    Ok(out)
}

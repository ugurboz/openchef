//! Attempts, sessions and turns.
//!
//! They can only start while their task is active. Together with `transition_task`
//! stopping a task's work when it leaves the active states, this keeps a cancel
//! committed by the CLI from being overtaken by the engine: whatever the engine tries
//! next is rejected before its side effect.

use std::path::PathBuf;

use openchef_core::{
    Attempt, AttemptStatus, Session, SessionState, StateMachine as _, StepStatus, TaskId, Turn,
    TurnKind, TurnStatus,
};
use rusqlite::{Connection, OptionalExtension as _, Params, Row, Transaction, params};

use crate::sql::{
    cas_failed, exec_one, format_time, non_empty, opt_path_text, parse_enum, parse_opt_time,
    parse_time,
};
use crate::tasks::require_active;
use crate::{Error, Store};

/// A new attempt for [`Store::start_attempt`].
#[derive(Debug, Clone)]
pub struct NewAttempt {
    /// The task.
    pub task_id: TaskId,
    /// The step, which must be running.
    pub step_index: u32,
    /// The routed agent.
    pub agent: String,
    /// Its runtime.
    pub runtime: String,
    /// Its model; empty for the runtime's default.
    pub model: String,
    /// Its pool.
    pub pool: String,
}

/// A spawned process to record with [`Store::record_session`].
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The running attempt it serves.
    pub attempt_id: i64,
    /// The closed session whose conversation it continues after a daemon restart; its
    /// native id is then taken from that session.
    pub resumed_from: Option<i64>,
    /// The runtime.
    pub runtime: String,
    /// The native id, when known before the CLI confirms it (Claude takes it as an
    /// argument).
    pub native_id: Option<String>,
    /// The process id.
    pub pid: u32,
    /// The process start token.
    pub start_token: u64,
    /// Where its event logs go.
    pub log_dir: Option<PathBuf>,
}

const ATTEMPT_COLUMNS: &str =
    "id, task_id, step_idx, agent, runtime, model, pool, status, outcome, started_at, ended_at";
const SESSION_COLUMNS: &str = "s.id, s.attempt_id, s.resumed_from, s.runtime, s.native_id, s.pid, s.start_token, \
     s.state, s.log_dir, s.opened_at, s.closed_at";
const TURN_COLUMNS: &str =
    "id, session_id, seq, kind, message, status, outcome, started_at, ended_at";

fn scan_attempt(r: &Row<'_>) -> Result<Attempt, Error> {
    Ok(Attempt {
        id: r.get(0)?,
        task_id: parse_enum("attempts.task_id", &r.get::<_, String>(1)?)?,
        step_index: r.get(2)?,
        agent: r.get(3)?,
        runtime: r.get(4)?,
        model: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        pool: r.get(6)?,
        status: parse_enum("attempts.status", &r.get::<_, String>(7)?)?,
        outcome: r.get(8)?,
        started_at: parse_time("attempts.started_at", &r.get::<_, String>(9)?)?,
        ended_at: parse_opt_time(
            "attempts.ended_at",
            r.get::<_, Option<String>>(10)?.as_deref(),
        )?,
    })
}

fn scan_session(r: &Row<'_>) -> Result<Session, Error> {
    let token: String = r.get(6)?;
    Ok(Session {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        resumed_from: r.get(2)?,
        runtime: r.get(3)?,
        native_id: r.get(4)?,
        pid: r.get(5)?,
        start_token: token.parse().map_err(|_| Error::Corrupt {
            column: "sessions.start_token",
            value: token.clone(),
        })?,
        state: parse_enum("sessions.state", &r.get::<_, String>(7)?)?,
        log_dir: r.get::<_, Option<String>>(8)?.map(Into::into),
        opened_at: parse_time("sessions.opened_at", &r.get::<_, String>(9)?)?,
        closed_at: parse_opt_time(
            "sessions.closed_at",
            r.get::<_, Option<String>>(10)?.as_deref(),
        )?,
    })
}

fn scan_turn(r: &Row<'_>) -> Result<Turn, Error> {
    Ok(Turn {
        id: r.get(0)?,
        session_id: r.get(1)?,
        seq: r.get(2)?,
        kind: parse_enum("turns.kind", &r.get::<_, String>(3)?)?,
        message: r.get(4)?,
        status: parse_enum("turns.status", &r.get::<_, String>(5)?)?,
        outcome: r.get(6)?,
        started_at: parse_time("turns.started_at", &r.get::<_, String>(7)?)?,
        ended_at: parse_opt_time("turns.ended_at", r.get::<_, Option<String>>(8)?.as_deref())?,
    })
}

fn query_all<T, P: Params>(
    c: &Connection,
    sql: &str,
    params: P,
    scan: fn(&Row<'_>) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(params, |r| Ok(scan(r)))?;
    rows.map(|r| r?).collect()
}

fn require_no_live_native(tx: &Transaction<'_>, runtime: &str, native: &str) -> Result<(), Error> {
    let live: Option<i64> = tx
        .query_row(
            "SELECT id FROM sessions WHERE runtime = ?1 AND native_id = ?2 AND state <> 'closed'",
            params![runtime, native],
            |r| r.get(0),
        )
        .optional()?;
    match live {
        None => Ok(()),
        Some(id) => Err(Error::Conflict {
            entity: "native session",
            key: format!("{runtime}/{native}"),
            want: "no live session".to_owned(),
            got: format!("live in session {id}"),
        }),
    }
}

/// Checks that `s` may continue the closed session it names and returns that session's
/// native id.
fn resumable(tx: &Transaction<'_>, from: i64, s: &NewSession) -> Result<String, Error> {
    let row: Option<(String, String, Option<String>)> = tx
        .query_row(
            "SELECT state, runtime, native_id FROM sessions WHERE id = ?1",
            [from],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (state, runtime, native) = row.ok_or_else(|| Error::NotFound(format!("session {from}")))?;
    if state != SessionState::Closed.as_str() {
        return Err(Error::Conflict {
            entity: "session",
            key: from.to_string(),
            want: "closed".to_owned(),
            got: state,
        });
    }
    if runtime != s.runtime {
        return Err(Error::Invalid(format!(
            "resume session {from}: runtime {runtime}, not {}",
            s.runtime
        )));
    }
    let native = native.ok_or_else(|| {
        Error::Invalid(format!(
            "resume session {from}: it never reported a native id"
        ))
    })?;
    if let Some(given) = s.native_id.as_deref().filter(|n| !n.is_empty())
        && given != native
    {
        return Err(Error::Invalid(format!(
            "resume session {from}: native id {native:?}, not {given:?}"
        )));
    }
    Ok(native)
}

impl Store {
    /// Records a new running attempt at a step. The task must be active, the step
    /// running, and the step may have only one running attempt at a time.
    pub async fn start_attempt(&self, a: NewAttempt) -> Result<Attempt, Error> {
        let key = format!("{}/{}", a.task_id, a.step_index);
        if a.agent.is_empty() || a.runtime.is_empty() || a.pool.is_empty() {
            return Err(Error::Invalid(format!(
                "attempt at {key}: agent, runtime and pool are required"
            )));
        }
        let now = self.now();
        self.write(move |tx| {
            require_active(tx, a.task_id.as_str())?;
            let row: Option<(String, i64)> = tx
                .query_row(
                    "SELECT status, (SELECT COUNT(*) FROM attempts WHERE task_id = ?1 AND step_idx = ?2 AND status = 'running') \
                     FROM steps WHERE task_id = ?1 AND idx = ?2",
                    params![a.task_id.as_str(), a.step_index],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (step, running) = row.ok_or_else(|| Error::NotFound(format!("step {key}")))?;
            if step != StepStatus::Running.as_str() {
                return Err(Error::Conflict { entity: "step", key, want: "running".to_owned(), got: step });
            }
            if running > 0 {
                return Err(Error::Conflict {
                    entity: "step",
                    key,
                    want: "no running attempt".to_owned(),
                    got: "attempt running".to_owned(),
                });
            }
            let _ = tx.execute(
                "INSERT INTO attempts (task_id, step_idx, agent, runtime, model, pool, status, started_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
                params![a.task_id.as_str(), a.step_index, a.agent, a.runtime, non_empty(&a.model), a.pool, format_time(now)],
            )?;
            Ok(Attempt {
                id: tx.last_insert_rowid(),
                task_id: a.task_id,
                step_index: a.step_index,
                agent: a.agent,
                runtime: a.runtime,
                model: a.model,
                pool: a.pool,
                status: AttemptStatus::Running,
                outcome: None,
                started_at: now,
                ended_at: None,
            })
        })
        .await
    }

    /// Moves a running attempt to a terminal status. An attempt succeeds only once its
    /// step is verified and no turn of it is in flight.
    pub async fn end_attempt(
        &self,
        id: i64,
        to: AttemptStatus,
        outcome: &str,
    ) -> Result<(), Error> {
        AttemptStatus::Running.check_transition(to)?;
        let outcome = non_empty(outcome).map(str::to_owned);
        let now = format_time(self.now());
        self.write(move |tx| {
            if to == AttemptStatus::Succeeded {
                let row: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT st.status, (SELECT COUNT(*) FROM turns t JOIN sessions s ON s.id = t.session_id \
                         WHERE s.attempt_id = a.id AND t.status = 'sent') \
                         FROM attempts a JOIN steps st ON st.task_id = a.task_id AND st.idx = a.step_idx WHERE a.id = ?1",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (step, in_flight) = row.ok_or_else(|| Error::NotFound(format!("attempt {id}")))?;
                if step != StepStatus::Verified.as_str() {
                    return Err(Error::Invalid(format!("attempt {id}: succeeded but its step is {step}")));
                }
                if in_flight > 0 {
                    return Err(Error::Invalid(format!("attempt {id}: succeeded with a turn in flight")));
                }
            }
            let ok = exec_one(
                tx,
                "UPDATE attempts SET status = ?1, outcome = ?2, ended_at = ?3 WHERE id = ?4 AND status = 'running'",
                params![to.as_str(), outcome, now, id],
            )?;
            if ok {
                Ok(())
            } else {
                Err(cas_failed(tx, "attempt", &id.to_string(), "running", "SELECT status FROM attempts WHERE id = ?1", [id]))
            }
        })
        .await
    }

    /// A task's attempts, oldest first.
    pub async fn attempts(&self, task: &TaskId) -> Result<Vec<Attempt>, Error> {
        let task = task.clone();
        self.read(move |c| {
            query_all(
                c,
                &format!("SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE task_id = ?1 ORDER BY id"),
                [task.as_str()],
                scan_attempt,
            )
        })
        .await
    }

    /// Every running attempt across all tasks, oldest first: after a daemon restart these
    /// are the attempts recovery resumes (§5.3).
    pub async fn running_attempts(&self) -> Result<Vec<Attempt>, Error> {
        self.read(|c| {
            query_all(
                c,
                &format!(
                    "SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE status = 'running' ORDER BY id"
                ),
                [],
                scan_attempt,
            )
        })
        .await
    }

    /// Records a spawned agent process as a starting session. It must be called before
    /// anything is written to the process (§5.1). The attempt must be running and its
    /// task active. A live session with the same native id is a conflict.
    pub async fn record_session(&self, s: NewSession) -> Result<Session, Error> {
        if s.runtime.is_empty() || s.pid == 0 {
            return Err(Error::Invalid(format!(
                "session of attempt {}: runtime and pid are required",
                s.attempt_id
            )));
        }
        let log_dir = opt_path_text("log directory", s.log_dir.as_ref())?;
        let now = self.now();
        self.write(move |tx| {
            let row: Option<(String, String)> = tx
                .query_row("SELECT status, task_id FROM attempts WHERE id = ?1", [s.attempt_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let (status, task) = row.ok_or_else(|| Error::NotFound(format!("attempt {}", s.attempt_id)))?;
            if status != AttemptStatus::Running.as_str() {
                return Err(Error::Conflict { entity: "attempt", key: s.attempt_id.to_string(), want: "running".to_owned(), got: status });
            }
            require_active(tx, &task)?;
            let native = match s.resumed_from {
                Some(from) => Some(resumable(tx, from, &s)?),
                None => s.native_id.clone().filter(|n| !n.is_empty()),
            };
            if let Some(native) = &native {
                require_no_live_native(tx, &s.runtime, native)?;
            }
            let _ = tx.execute(
                "INSERT INTO sessions (attempt_id, resumed_from, runtime, native_id, pid, start_token, state, log_dir, opened_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'starting', ?7, ?8)",
                params![s.attempt_id, s.resumed_from, s.runtime, native, s.pid, s.start_token.to_string(), log_dir, format_time(now)],
            )?;
            Ok(Session {
                id: tx.last_insert_rowid(),
                attempt_id: s.attempt_id,
                resumed_from: s.resumed_from,
                runtime: s.runtime,
                native_id: native,
                pid: s.pid,
                start_token: s.start_token,
                state: SessionState::Starting,
                log_dir: s.log_dir,
                opened_at: now,
                closed_at: None,
            })
        })
        .await
    }

    /// Records the CLI's native session id and marks the session open. A native id
    /// recorded earlier must match.
    pub async fn open_session(&self, id: i64, native_id: &str) -> Result<(), Error> {
        if native_id.is_empty() {
            return Err(Error::Invalid(format!("session {id}: empty native id")));
        }
        let native = native_id.to_owned();
        self.write(move |tx| {
            let row: Option<(String, String, Option<String>)> = tx
                .query_row(
                    "SELECT state, runtime, native_id FROM sessions WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (state, runtime, known) =
                row.ok_or_else(|| Error::NotFound(format!("session {id}")))?;
            if state != SessionState::Starting.as_str() {
                return Err(Error::Conflict {
                    entity: "session",
                    key: id.to_string(),
                    want: "starting".to_owned(),
                    got: state,
                });
            }
            match &known {
                Some(known) if *known != native => {
                    return Err(Error::Invalid(format!(
                        "session {id}: CLI reported native id {native:?}, recorded {known:?}"
                    )));
                }
                Some(_) => {}
                None => require_no_live_native(tx, &runtime, &native)?,
            }
            let _ = tx.execute(
                "UPDATE sessions SET state = 'open', native_id = ?1 WHERE id = ?2",
                params![native, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Records a new process for a live session: some CLIs replace their process within
    /// one conversation (D-019). Call it before anything is written to the new process.
    pub async fn set_session_process(
        &self,
        id: i64,
        pid: u32,
        start_token: u64,
    ) -> Result<(), Error> {
        if pid == 0 {
            return Err(Error::Invalid(format!("session {id}: pid 0")));
        }
        self.write(move |tx| {
            let ok = exec_one(
                tx,
                "UPDATE sessions SET pid = ?1, start_token = ?2 WHERE id = ?3 AND state <> 'closed'",
                params![pid, start_token.to_string(), id],
            )?;
            if ok {
                Ok(())
            } else {
                Err(cas_failed(tx, "session", &id.to_string(), "not closed", "SELECT state FROM sessions WHERE id = ?1", [id]))
            }
        })
        .await
    }

    /// Marks a session closed once its process is gone. Closing a closed session is a
    /// no-op.
    pub async fn close_session(&self, id: i64) -> Result<(), Error> {
        let now = format_time(self.now());
        self.write(move |tx| {
            let ok = exec_one(
                tx,
                "UPDATE sessions SET state = 'closed', closed_at = ?1 WHERE id = ?2 AND state <> 'closed'",
                params![now, id],
            )?;
            if ok || tx.query_row("SELECT 1 FROM sessions WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some() {
                Ok(())
            } else {
                Err(Error::NotFound(format!("session {id}")))
            }
        })
        .await
    }

    /// An attempt's sessions, oldest first.
    pub async fn sessions(&self, attempt: i64) -> Result<Vec<Session>, Error> {
        self.read(move |c| {
            query_all(
                c,
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.attempt_id = ?1 ORDER BY s.id"
                ),
                [attempt],
                scan_session,
            )
        })
        .await
    }

    /// The sessions of every attempt of a task, oldest first; with `live`, only those not
    /// yet closed (the processes to stop after a cancel).
    pub async fn task_sessions(&self, task: &TaskId, live: bool) -> Result<Vec<Session>, Error> {
        let task = task.clone();
        let filter = if live { " AND s.state <> 'closed'" } else { "" };
        self.read(move |c| {
            query_all(
                c,
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions s JOIN attempts a ON a.id = s.attempt_id \
                     WHERE a.task_id = ?1{filter} ORDER BY s.id"
                ),
                [task.as_str()],
                scan_session,
            )
        })
        .await
    }

    /// Every session not yet closed, across all tasks: after a daemon restart these are
    /// the processes that may still be running (§5.3).
    pub async fn live_sessions(&self) -> Result<Vec<Session>, Error> {
        self.read(|c| {
            query_all(c, &format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.state <> 'closed' ORDER BY s.id"), [], scan_session)
        })
        .await
    }

    /// Records a message as sent to a session, before it is written to the CLI. The
    /// session must be open, its attempt running, its task active and no other turn of it
    /// in flight.
    pub async fn begin_turn(
        &self,
        session: i64,
        kind: TurnKind,
        message: &str,
    ) -> Result<Turn, Error> {
        if message.is_empty() {
            return Err(Error::Invalid(format!(
                "turn of session {session}: empty message"
            )));
        }
        let message = message.to_owned();
        let now = self.now();
        self.write(move |tx| {
            let row: Option<(String, String, String, i64, u32)> = tx
                .query_row(
                    "SELECT s.state, a.status, a.task_id, \
                     (SELECT COUNT(*) FROM turns WHERE session_id = s.id AND status = 'sent'), \
                     (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE session_id = s.id) \
                     FROM sessions s JOIN attempts a ON a.id = s.attempt_id WHERE s.id = ?1",
                    [session],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let (state, attempt, task, in_flight, seq) =
                row.ok_or_else(|| Error::NotFound(format!("session {session}")))?;
            let key = session.to_string();
            if state != SessionState::Open.as_str() {
                return Err(Error::Conflict { entity: "session", key, want: "open".to_owned(), got: state });
            }
            if attempt != AttemptStatus::Running.as_str() {
                return Err(Error::Conflict { entity: "attempt of session", key, want: "running".to_owned(), got: attempt });
            }
            if in_flight > 0 {
                return Err(Error::Conflict {
                    entity: "session",
                    key,
                    want: "no turn in flight".to_owned(),
                    got: "turn in flight".to_owned(),
                });
            }
            require_active(tx, &task)?;
            let _ = tx.execute(
                "INSERT INTO turns (session_id, seq, kind, message, status, started_at) VALUES (?1, ?2, ?3, ?4, 'sent', ?5)",
                params![session, seq, kind.as_str(), message, format_time(now)],
            )?;
            Ok(Turn {
                id: tx.last_insert_rowid(),
                session_id: session,
                seq,
                kind,
                message,
                status: TurnStatus::Sent,
                outcome: None,
                started_at: now,
                ended_at: None,
            })
        })
        .await
    }

    /// Moves a sent turn to a terminal status.
    pub async fn end_turn(&self, id: i64, to: TurnStatus, outcome: &str) -> Result<(), Error> {
        TurnStatus::Sent.check_transition(to)?;
        let outcome = non_empty(outcome).map(str::to_owned);
        let now = format_time(self.now());
        self.write(move |tx| {
            let ok = exec_one(
                tx,
                "UPDATE turns SET status = ?1, outcome = ?2, ended_at = ?3 WHERE id = ?4 AND status = 'sent'",
                params![to.as_str(), outcome, now, id],
            )?;
            if ok {
                Ok(())
            } else {
                Err(cas_failed(tx, "turn", &id.to_string(), "sent", "SELECT status FROM turns WHERE id = ?1", [id]))
            }
        })
        .await
    }

    /// A session's turns in order.
    pub async fn turns(&self, session: i64) -> Result<Vec<Turn>, Error> {
        self.read(move |c| {
            query_all(
                c,
                &format!("SELECT {TURN_COLUMNS} FROM turns WHERE session_id = ?1 ORDER BY seq"),
                [session],
                scan_turn,
            )
        })
        .await
    }

    /// The session's turn that was sent but never ended: the one recovery retries with a
    /// recovery note (§5.3).
    pub async fn in_flight_turn(&self, session: i64) -> Result<Turn, Error> {
        self.read(move |c| {
            c.query_row(
                &format!(
                    "SELECT {TURN_COLUMNS} FROM turns WHERE session_id = ?1 AND status = 'sent'"
                ),
                [session],
                |r| Ok(scan_turn(r)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("turn in flight in session {session}")))?
        })
        .await
    }
}

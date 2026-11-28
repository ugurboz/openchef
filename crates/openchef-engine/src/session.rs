//! Opening, recording and closing agent sessions.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use openchef_core::Attempt;
use openchef_procs::Identity;
use openchef_runtime::SessionRef;
use openchef_store::{NewSession, Store};

use crate::engine::{Error, Run};
use crate::router::Agent;
use crate::runtimes::DynSession;

/// Implements `on_spawn` for one session row: the first process creates the row, later
/// processes (respawns within the conversation) update it. Both happen before the
/// adapter writes anything to the process (§5.1, D-019).
struct SpawnRecorder {
    store: Store,
    attempt: i64,
    runtime: String,
    log_dir: PathBuf,
    resumed_from: Option<i64>,
    /// The row, once created. A leaf value; never held across `.await`.
    row: Mutex<Option<i64>>,
}

impl SpawnRecorder {
    fn row(&self) -> Option<i64> {
        *self.row.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn on_spawn(&self, p: Identity) -> Result<(), String> {
        if let Some(id) = self.row() {
            return self
                .store
                .set_session_process(id, p.pid, p.start_token)
                .await
                .map_err(|e| e.to_string());
        }
        let session = self
            .store
            .record_session(NewSession {
                attempt_id: self.attempt,
                resumed_from: self.resumed_from,
                runtime: self.runtime.clone(),
                native_id: None,
                pid: p.pid,
                start_token: p.start_token,
                log_dir: Some(self.log_dir.clone()),
            })
            .await
            .map_err(|e| e.to_string())?;
        *self.row.lock().unwrap_or_else(PoisonError::into_inner) = Some(session.id);
        Ok(())
    }
}

/// An open agent session and its store row.
pub(crate) struct Live {
    pub(crate) session: Box<dyn DynSession>,
    pub(crate) row: i64,
}

impl Run<'_> {
    /// Starts (`resume` is `None`) or resumes a session for an attempt and records it.
    pub(crate) async fn open_session(
        &self,
        a: &Attempt,
        agent: &Agent,
        resume: Option<SessionRef>,
        resumed_from: Option<i64>,
        seq: u32,
    ) -> Result<Live, Error> {
        let runtime = self.e.cfg.runtimes.get(&agent.runtime).ok_or_else(|| {
            Error::Session(format!(
                "agent {}: runtime {:?} is not available",
                agent.name, agent.runtime
            ))
        })?;
        let mut spec = (self.e.cfg.spec_for)(&self.task, agent, a.role)
            .map_err(|e| Error::Session(format!("session settings for {}: {e}", agent.name)))?;
        let log_dir = self
            .e
            .cfg
            .session_root
            .join(self.task.id.as_str())
            .join(format!("{}-{seq}", a.id));
        let recorder = Arc::new(SpawnRecorder {
            store: self.store().clone(),
            attempt: a.id,
            runtime: agent.runtime.clone(),
            log_dir: log_dir.clone(),
            resumed_from,
            row: Mutex::new(None),
        });
        spec.dir = self.worktree()?.path.clone();
        spec.model.clone_from(&agent.model);
        spec.effort = agent.effort;
        spec.log_path = Some(log_dir.join("raw.jsonl"));
        spec.on_spawn = Some({
            let recorder = Arc::clone(&recorder);
            Arc::new(move |id| {
                let recorder = Arc::clone(&recorder);
                Box::pin(async move { recorder.on_spawn(id).await })
            })
        });
        let opened = match resume {
            None => runtime.open(spec).await,
            Some(reference) => runtime.resume(reference, spec).await,
        };
        let session = match opened {
            Ok(s) => s,
            Err(e) => {
                if let Some(row) = recorder.row() {
                    self.store().close_session(row).await?;
                }
                return Err(Error::Session(format!("start {} session: {e}", agent.name)));
            }
        };
        let Some(row) = recorder.row() else {
            let closed = session.close().await;
            return Err(Error::Session(format!(
                "the {} adapter did not report its process (on_spawn){}",
                agent.runtime,
                closed
                    .err()
                    .map_or_else(String::new, |e| format!("; closing it failed: {e}"))
            )));
        };
        let live = Live { session, row };
        if let Err(e) = self
            .store()
            .open_session(row, &live.session.session_ref().id)
            .await
        {
            return Err(match self.close_session(live).await {
                Ok(()) => e.into(),
                Err(c) => Error::Session(format!("{e}; and {c}")),
            });
        }
        Ok(live)
    }

    /// Stops the CLI and then records the session closed. It runs on every path,
    /// shutdown included.
    pub(crate) async fn close_session(&self, live: Live) -> Result<(), Error> {
        let closed = live.session.close().await;
        let recorded = self.store().close_session(live.row).await;
        match (closed, recorded) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(Error::Session(format!("close session {}: {e}", live.row))),
            (Ok(()), Err(e)) => Err(e.into()),
        }
    }

    /// Closes `live` if there is one, reporting a failure as a notice.
    pub(crate) async fn close_quietly(&self, live: Option<Live>, step: u32, attempt: i64) {
        if let Some(l) = live
            && let Err(e) = self.close_session(l).await
        {
            self.notice(step, Some(attempt), e.to_string());
        }
    }
}

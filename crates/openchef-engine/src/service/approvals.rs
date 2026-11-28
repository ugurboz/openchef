//! The approval queue's in-memory half (§9.2, D-022), ported from Go
//! `internal/service/approvals.go`: who waits for which request. The store holds the
//! requests themselves, so they survive the daemon and `openchef approvals` lists them.

use std::sync::Arc;
use std::time::Duration;

use openchef_core::{Approval, ApprovalKind, ApprovalStatus, DecidedBy, Decision, TaskId};
use openchef_store::{ApprovalFilter, NewApproval};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{Error, Inner, Service};
use crate::events::{Event, EventKind, Sink as _};

/// Settles requests a previous daemon left pending (D-022).
const RESTART_REASON: &str = "The daemon restarted while this request was waiting; the agent asks again if it still needs it.";

/// Shows the user a desktop notification: title, body. Best effort, never blocks.
pub type Notify = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// One thing an agent wants from the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    /// The task asking.
    pub task: TaskId,
    /// The agent asking.
    pub agent: String,
    /// What is asked.
    pub kind: ApprovalKind,
    /// The tool in Claude Code vocabulary.
    pub tool: Option<String>,
    /// What the user decides on, never shortened: people approve exactly what runs.
    pub summary: String,
    /// A question's suggested answers.
    pub options: Vec<String>,
    /// The normalized runtime request as JSON, kept for audit.
    pub raw: Option<String>,
}

/// Removes a waiter however `ask` ends.
struct Waiting<'a> {
    inner: &'a Inner,
    id: i64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        drop(self.inner.state().waiting.remove(&self.id));
    }
}

impl Service {
    /// Queues `req` and waits until the user settles it, it expires (§9.2: then a
    /// permission is denied), `cancel` fires (the agent stopped waiting: the request is
    /// withdrawn) or the service stops (left pending; the next start expires it).
    /// Returns the settled request.
    ///
    /// # Errors
    ///
    /// [`Error::Withdrawn`] after `cancel`; [`Error::Stopping`]; store failures.
    pub async fn ask(
        &self,
        req: AskRequest,
        cancel: &CancellationToken,
    ) -> Result<Approval, Error> {
        let inner = &self.inner;
        let a = inner
            .store
            .create_approval(NewApproval {
                task_id: req.task,
                agent: req.agent,
                kind: req.kind,
                tool: req.tool,
                summary: req.summary,
                options: req.options,
                request: req.raw,
                expires_at: Some(inner.now() + inner.approval_timeout),
            })
            .await?;
        let (tx, rx) = oneshot::channel();
        drop(inner.state().waiting.insert(a.id, tx));
        let _waiting = Waiting { inner, id: a.id };
        inner.approval_event(EventKind::ApprovalRequested, &a);
        // The request was listable before the waiter existed: a decision in between
        // found no one to wake. `settle` writes the store before it looks for the
        // waiter, so reading the store now sees every such decision.
        let current = inner.store.approval(a.id).await?;
        if current.status != ApprovalStatus::Pending {
            return Ok(current);
        }
        if let Some(notify) = &inner.notify
            && !inner.events.watched(&a.task_id)
        {
            notify("OpenChef needs you", &notification(&a));
        }
        let wait = Duration::try_from(a.expires_at - inner.now()).unwrap_or(Duration::ZERO);
        let settle = tokio::select! {
            got = rx => match got {
                Ok(got) => return Ok(got),
                Err(_) => return Err(Error::Stopping), // the waiter map was dropped
            },
            () = tokio::time::sleep(wait) => Decision {
                status: ApprovalStatus::Expired,
                answer: Some(format!("No one answered within {}.", minutes(inner.approval_timeout))),
                decided_by: DecidedBy::Timeout,
            },
            () = cancel.cancelled() => Decision {
                status: ApprovalStatus::Withdrawn,
                answer: Some("The agent stopped waiting.".to_owned()),
                decided_by: DecidedBy::Daemon,
            },
            () = inner.life.cancelled() => return Err(Error::Stopping),
        };
        let withdrawn = settle.status == ApprovalStatus::Withdrawn;
        match self.settle(a.id, settle).await {
            // The user answered at the same moment; their answer stands.
            Err(Error::Settled { .. }) => Ok(inner.store.approval(a.id).await?),
            Err(e) => Err(e),
            Ok(_) if withdrawn => Err(Error::Withdrawn(a.id)),
            Ok(got) => Ok(got),
        }
    }

    /// Settles a pending request for the user.
    ///
    /// # Errors
    ///
    /// [`Error::Settled`] when it was settled already; [`Error::Invalid`] when the
    /// decision does not fit the request; store failures.
    pub async fn decide(
        &self,
        id: i64,
        status: ApprovalStatus,
        answer: Option<String>,
    ) -> Result<Approval, Error> {
        let d = Decision {
            status,
            answer,
            decided_by: DecidedBy::User,
        };
        self.settle(id, d).await
    }

    /// Writes the decision, wakes the waiter and tells the task's viewers.
    async fn settle(&self, id: i64, d: Decision) -> Result<Approval, Error> {
        let got = match self.inner.store.decide_approval(id, d).await {
            Ok(got) => got,
            Err(openchef_store::Error::Conflict { got, .. }) => {
                let status = got
                    .parse()
                    .map_err(|e: openchef_core::Error| Error::Invalid(e.to_string()))?;
                return Err(Error::Settled { id, status });
            }
            Err(openchef_store::Error::Domain(e)) => return Err(Error::Invalid(e.to_string())),
            Err(e) => return Err(e.into()),
        };
        if let Some(tx) = self.inner.state().waiting.remove(&id) {
            // The waiter may have stopped waiting at this moment; it then reads the store.
            let _sent = tx.send(got.clone());
        }
        self.inner.approval_event(EventKind::ApprovalDecided, &got);
        Ok(got)
    }

    /// Requests, oldest first.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub async fn approvals(&self, f: ApprovalFilter) -> Result<Vec<Approval>, Error> {
        Ok(self.inner.store.approvals(f).await?)
    }

    /// Settles requests a previous daemon left pending: nobody waits for them.
    pub(super) async fn expire_stale(&self) -> Result<(), Error> {
        for a in self.inner.store.expire_pending(RESTART_REASON).await? {
            self.inner.approval_event(EventKind::ApprovalDecided, &a);
        }
        Ok(())
    }
}

impl Inner {
    fn approval_event(&self, kind: EventKind, a: &Approval) {
        let mut ev = Event::new(&a.task_id, self.now(), kind);
        ev.agent = Some(a.agent.clone());
        ev.approval = Some(a.clone());
        self.events.publish(ev);
    }
}

fn minutes(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m >= 1 {
        format!("{m} minutes")
    } else {
        format!("{} seconds", d.as_secs())
    }
}

/// The notification text, with the command that settles the request.
#[must_use]
pub(super) fn notification(a: &Approval) -> String {
    if a.kind == ApprovalKind::Question {
        return format!(
            "{} asks: {} (openchef answer {} \"…\")",
            a.agent, a.summary, a.id
        );
    }
    format!(
        "{} wants {}: {} (openchef approve {})",
        a.agent,
        a.tool.as_deref().unwrap_or("to act"),
        a.summary,
        a.id
    )
}

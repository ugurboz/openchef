//! The approval queue (§9.2, D-022), ported from Go `internal/store/approvals.go`:
//! permission requests and agent questions waiting for a person. Rows stay after they
//! are settled, as the audit trail.

use openchef_core::{
    APPROVAL_TIMEOUT, Approval, ApprovalKind, ApprovalStatus, DecidedBy, Decision, TaskId,
};
use rusqlite::{OptionalExtension as _, Row, params};
use time::OffsetDateTime;

use crate::sql::{format_time, non_empty, parse_enum, parse_opt_time, parse_time};
use crate::tasks::require_active;
use crate::{Error, Store};

/// A request to queue with [`Store::create_approval`].
#[derive(Debug, Clone)]
pub struct NewApproval {
    /// The task asking; it must be active.
    pub task_id: TaskId,
    /// The agent asking.
    pub agent: String,
    /// What is asked.
    pub kind: ApprovalKind,
    /// The tool in Claude Code vocabulary.
    pub tool: Option<String>,
    /// The one line a person decides on.
    pub summary: String,
    /// A question's suggested answers.
    pub options: Vec<String>,
    /// The normalized request as JSON, kept for audit.
    pub request: Option<String>,
    /// When it expires; `None` is [`APPROVAL_TIMEOUT`] from now.
    pub expires_at: Option<OffsetDateTime>,
}

/// Selects requests for [`Store::approvals`].
#[derive(Debug, Clone, Default)]
pub struct ApprovalFilter {
    /// Only this task's requests.
    pub task: Option<TaskId>,
    /// Only requests still waiting.
    pub pending: bool,
    /// At most the newest this many.
    pub limit: Option<u32>,
}

const APPROVAL_COLUMNS: &str = "id, task_id, agent, kind, status, tool, summary, options, answer, \
     decided_by, created_at, expires_at, decided_at";

fn scan_approval(r: &Row<'_>) -> Result<Approval, Error> {
    let options: String = r.get(7)?;
    let options: Vec<String> = serde_json::from_str(&options)
        .map_err(|e| Error::Invalid(format!("approvals.options: {e}")))?;
    Ok(Approval {
        id: r.get(0)?,
        task_id: parse_enum("approvals.task_id", &r.get::<_, String>(1)?)?,
        agent: r.get(2)?,
        kind: parse_enum("approvals.kind", &r.get::<_, String>(3)?)?,
        status: parse_enum("approvals.status", &r.get::<_, String>(4)?)?,
        tool: r.get(5)?,
        summary: r.get(6)?,
        options,
        answer: r.get(8)?,
        decided_by: r
            .get::<_, Option<String>>(9)?
            .map(|d| parse_enum::<DecidedBy>("approvals.decided_by", &d))
            .transpose()?,
        created_at: parse_time("approvals.created_at", &r.get::<_, String>(10)?)?,
        expires_at: parse_time("approvals.expires_at", &r.get::<_, String>(11)?)?,
        decided_at: parse_opt_time(
            "approvals.decided_at",
            r.get::<_, Option<String>>(12)?.as_deref(),
        )?,
    })
}

impl Store {
    /// Queues a pending request for an active task.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] without a summary or agent; [`Error::Conflict`] when the task
    /// is no longer active.
    pub async fn create_approval(&self, a: NewApproval) -> Result<Approval, Error> {
        if a.summary.is_empty() || a.agent.is_empty() {
            return Err(Error::Invalid(format!(
                "approval for task {}: summary and agent are required",
                a.task_id
            )));
        }
        let options = serde_json::to_string(&a.options)
            .map_err(|e| Error::Invalid(format!("approval options: {e}")))?;
        let now = self.now();
        let expires = a.expires_at.unwrap_or(now + APPROVAL_TIMEOUT);
        self.write(move |tx| {
            require_active(tx, a.task_id.as_str())?;
            let _ = tx.execute(
                "INSERT INTO approvals (task_id, agent, kind, status, tool, summary, options, request, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    a.task_id.as_str(),
                    a.agent,
                    a.kind.as_str(),
                    a.tool.as_deref().and_then(non_empty),
                    a.summary,
                    options,
                    a.request.as_deref().and_then(non_empty),
                    format_time(now),
                    format_time(expires),
                ],
            )?;
            Ok(Approval {
                id: tx.last_insert_rowid(),
                task_id: a.task_id,
                agent: a.agent,
                kind: a.kind,
                status: ApprovalStatus::Pending,
                tool: a.tool.filter(|t| !t.is_empty()),
                summary: a.summary,
                options: a.options,
                answer: None,
                decided_by: None,
                created_at: now,
                expires_at: expires,
                decided_at: None,
            })
        })
        .await
    }

    /// Settles a pending request (compare-and-set on pending).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::Conflict`] with the settled status when the request
    /// was already settled, so a late answer is reported instead of overwriting the
    /// first; [`Error::Domain`] when the decision does not fit the request's kind.
    pub async fn decide_approval(&self, id: i64, d: Decision) -> Result<Approval, Error> {
        let now = self.now();
        self.write(move |tx| {
            let mut a = tx
                .query_row(
                    &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE id = ?1"),
                    [id],
                    |r| Ok(scan_approval(r)),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("approval {id}")))??;
            if a.status != ApprovalStatus::Pending {
                return Err(Error::Conflict {
                    entity: "approval",
                    key: id.to_string(),
                    want: ApprovalStatus::Pending.to_string(),
                    got: a.status.to_string(),
                });
            }
            d.validate(a.kind)?;
            let _ = tx.execute(
                "UPDATE approvals SET status = ?1, answer = ?2, decided_by = ?3, decided_at = ?4 \
                 WHERE id = ?5 AND status = 'pending'",
                params![
                    d.status.as_str(),
                    d.answer.as_deref(),
                    d.decided_by.as_str(),
                    format_time(now),
                    id
                ],
            )?;
            a.status = d.status;
            a.answer = d.answer;
            a.decided_by = Some(d.decided_by);
            a.decided_at = Some(now);
            Ok(a)
        })
        .await
    }

    /// Settles every pending request as expired by the daemon, with `reason` as the
    /// message to the agent. The daemon calls it on start: whoever waited for those
    /// answers died with the previous daemon. Returns the settled requests.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub async fn expire_pending(&self, reason: &str) -> Result<Vec<Approval>, Error> {
        let (now, reason) = (format_time(self.now()), reason.to_owned());
        self.write(move |tx| {
            let mut stmt = tx.prepare(&format!(
                "UPDATE approvals SET status = 'expired', answer = ?1, decided_by = 'daemon', decided_at = ?2 \
                 WHERE status = 'pending' RETURNING {APPROVAL_COLUMNS}"
            ))?;
            let rows = stmt.query_map(params![non_empty(&reason), now], |r| Ok(scan_approval(r)))?;
            let mut out: Vec<Approval> = rows.map(|r| r?).collect::<Result<_, _>>()?;
            out.sort_by_key(|a| a.id);
            Ok(out)
        })
        .await
    }

    /// One request.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`].
    pub async fn approval(&self, id: i64) -> Result<Approval, Error> {
        self.read(move |c| {
            c.query_row(
                &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE id = ?1"),
                [id],
                |r| Ok(scan_approval(r)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("approval {id}")))?
        })
        .await
    }

    /// Requests, oldest first (the order to answer them in); with a limit, the newest
    /// that many.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub async fn approvals(&self, f: ApprovalFilter) -> Result<Vec<Approval>, Error> {
        let mut sql = format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE 1 = 1");
        if f.task.is_some() {
            sql.push_str(" AND task_id = ?1");
        }
        if f.pending {
            sql.push_str(" AND status = 'pending'");
        }
        sql.push_str(" ORDER BY id DESC");
        if let Some(limit) = f.limit {
            sql.push_str(" LIMIT ");
            sql.push_str(&limit.to_string());
        }
        let task = f.task.map(|t| t.as_str().to_owned());
        self.read(move |c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = match &task {
                Some(t) => stmt
                    .query_map([t], |r| Ok(scan_approval(r)))?
                    .collect::<Vec<_>>(),
                None => stmt
                    .query_map([], |r| Ok(scan_approval(r)))?
                    .collect::<Vec<_>>(),
            };
            let mut out: Vec<Approval> = rows.into_iter().map(|r| r?).collect::<Result<_, _>>()?;
            out.reverse();
            Ok(out)
        })
        .await
    }
}

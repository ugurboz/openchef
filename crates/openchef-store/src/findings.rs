//! Review findings (§7.5, D-038). The store keeps them as plain rows; the review
//! vocabulary lives in the engine.

use openchef_core::TaskId;
use rusqlite::params;

use crate::sql::{format_time, non_empty, parse_enum};
use crate::{Error, Store};

/// One stored finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingRow {
    /// Store id.
    pub id: i64,
    /// The task.
    pub task_id: TaskId,
    /// The step; 0 for the plan council.
    pub step_index: u32,
    /// The review round, from 1.
    pub round: u32,
    /// The reviewer's attempt.
    pub attempt_id: Option<i64>,
    /// The reviewer's id for it (`F1`).
    pub finding: String,
    /// `blocker`, `major` or `nit`.
    pub severity: String,
    /// The file.
    pub file: Option<String>,
    /// The line.
    pub line: Option<u32>,
    /// The contract.
    pub contract: Option<String>,
    /// What is wrong.
    pub claim: String,
    /// Why.
    pub evidence: String,
    /// The suggested change.
    pub fix: String,
    /// `open`, `fixed`, `disputed`, `dismissed` (by an arbiter) or `nit`.
    pub status: String,
    /// A rebuttal's evidence.
    pub note: Option<String>,
}

const SEVERITIES: [&str; 3] = ["blocker", "major", "nit"];
const STATUSES: [&str; 5] = ["open", "fixed", "disputed", "dismissed", "nit"];

impl Store {
    /// Stores a review round's findings (their `id` is ignored and assigned).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an unknown severity or status.
    pub async fn record_findings(&self, rows: Vec<FindingRow>) -> Result<(), Error> {
        for r in &rows {
            if !SEVERITIES.contains(&r.severity.as_str()) || !STATUSES.contains(&r.status.as_str())
            {
                return Err(Error::Invalid(format!(
                    "finding {}: severity {:?}, status {:?}",
                    r.finding, r.severity, r.status
                )));
            }
        }
        let now = format_time(self.now());
        self.write(move |tx| {
            for r in &rows {
                let _ = tx.execute(
                    "INSERT INTO findings (task_id, step_idx, round, attempt_id, finding, severity, file, line, contract, \
                     claim, evidence, fix, status, note, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
                    params![
                        r.task_id.as_str(), r.step_index, r.round, r.attempt_id, r.finding, r.severity,
                        r.file.as_deref().and_then(non_empty), r.line, r.contract.as_deref().and_then(non_empty),
                        r.claim, r.evidence, r.fix, r.status, r.note.as_deref().and_then(non_empty), now
                    ],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Sets the status of a step's open findings with the given reviewer ids, from
    /// `round`; returns how many changed.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an unknown status.
    pub async fn settle_findings(
        &self,
        task: &TaskId,
        step: u32,
        round: u32,
        ids: Vec<(String, Option<String>)>,
        status: &str,
    ) -> Result<usize, Error> {
        if !STATUSES.contains(&status) {
            return Err(Error::Invalid(format!("finding status {status:?}")));
        }
        let (task, status, now) = (task.clone(), status.to_owned(), format_time(self.now()));
        self.write(move |tx| {
            let mut changed = 0;
            for (id, note) in &ids {
                changed += tx.execute(
                    "UPDATE findings SET status = ?1, note = COALESCE(?2, note), updated_at = ?3 \
                     WHERE task_id = ?4 AND step_idx = ?5 AND round = ?6 AND finding = ?7 AND status = 'open'",
                    params![status, note.as_deref().and_then(non_empty), now, task.as_str(), step, round, id],
                )?;
            }
            Ok(changed)
        })
        .await
    }

    /// A task's findings, by step, round and order of report.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn findings(&self, task: &TaskId) -> Result<Vec<FindingRow>, Error> {
        let task = task.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, task_id, step_idx, round, attempt_id, finding, severity, file, line, contract, claim, \
                 evidence, fix, status, note FROM findings WHERE task_id = ?1 ORDER BY step_idx, round, id",
            )?;
            let mut rows = stmt.query([task.as_str()])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push(FindingRow {
                    id: r.get(0)?,
                    task_id: parse_enum("findings.task_id", &r.get::<_, String>(1)?)?,
                    step_index: r.get(2)?,
                    round: r.get(3)?,
                    attempt_id: r.get(4)?,
                    finding: r.get(5)?,
                    severity: r.get(6)?,
                    file: r.get(7)?,
                    line: r.get(8)?,
                    contract: r.get(9)?,
                    claim: r.get(10)?,
                    evidence: r.get(11)?,
                    fix: r.get(12)?,
                    status: r.get(13)?,
                    note: r.get(14)?,
                });
            }
            Ok(out)
        })
        .await
    }
}

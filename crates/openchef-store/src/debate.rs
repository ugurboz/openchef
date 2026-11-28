//! The debate budget of a task (§7.5, D-040): turns spent by reviewers, critics,
//! arbiters, fixes and escalations, counted on the task row so a restart never refills it.

use openchef_core::TaskId;
use rusqlite::{OptionalExtension as _, params};

use crate::sql::exec_one;
use crate::{Error, Store};

impl Store {
    /// Spends one debate turn of the task when fewer than `max` are spent. `Ok(false)`
    /// when the budget is used up; one conditional update, so two callers never both
    /// take the last turn.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown task.
    pub async fn spend_debate_turn(&self, id: &TaskId, max: u32) -> Result<bool, Error> {
        let id = id.clone();
        self.write(move |tx| {
            if exec_one(
                tx,
                "UPDATE tasks SET debate_turns = debate_turns + 1 WHERE id = ?1 AND debate_turns < ?2",
                params![id.as_str(), max],
            )? {
                return Ok(true);
            }
            let known: Option<i64> = tx
                .query_row("SELECT 1 FROM tasks WHERE id = ?1", [id.as_str()], |r| r.get(0))
                .optional()?;
            match known {
                Some(_) => Ok(false),
                None => Err(Error::NotFound(format!("task {id}"))),
            }
        })
        .await
    }

    /// The debate turns the task has spent.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown task.
    pub async fn debate_turns(&self, id: &TaskId) -> Result<u32, Error> {
        let id = id.clone();
        self.read(move |c| {
            c.query_row(
                "SELECT debate_turns FROM tasks WHERE id = ?1",
                [id.as_str()],
                |r| r.get::<_, u32>(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("task {id}")))
        })
        .await
    }
}

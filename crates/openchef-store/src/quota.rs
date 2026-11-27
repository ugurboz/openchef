//! Quota records (§10, D-025, D-026), ported from Go `internal/store/quota.go`:
//! observations of pools' windows, the CLI's switches, per-step costs and API
//! spending. The store keeps windows as the JSON the quota crate writes, so it does not
//! depend on that crate's types.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension as _, params};
use time::OffsetDateTime;

use crate::sql::{format_time, parse_time};
use crate::{Error, Store};

/// The sources an observation may come from (§10.1).
const SOURCES: [&str; 3] = ["L1", "L2", "L3"];

/// One stored quota observation.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationRow {
    /// The pool observed.
    pub pool: String,
    /// `L1` (stream), `L2` (probe) or `L3` (estimate).
    pub source: String,
    /// When it was observed.
    pub at: OffsetDateTime,
    /// The provider said the limit is reached.
    pub exhausted: bool,
    /// The windows as a JSON array.
    pub windows: String,
}

/// Money an API pool spent at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spend {
    /// US dollars.
    pub usd: f64,
    /// When.
    pub at: OffsetDateTime,
}

fn invalid(what: &str, why: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{what}: {why}"))
}

impl Store {
    /// Stores one observation.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an unknown source, an empty pool or windows that are not
    /// a JSON array.
    pub async fn record_observation(&self, o: ObservationRow) -> Result<(), Error> {
        if !SOURCES.contains(&o.source.as_str()) {
            return Err(invalid(
                "record observation",
                format!("unknown source {:?}", o.source),
            ));
        }
        if o.pool.is_empty() {
            return Err(invalid("record observation", "pool is required"));
        }
        let windows = if o.windows.trim().is_empty() {
            "[]".to_owned()
        } else {
            o.windows
        };
        match serde_json::from_str::<serde_json::Value>(&windows) {
            Ok(serde_json::Value::Array(_)) => {}
            Ok(_) => return Err(invalid("record observation", "windows must be an array")),
            Err(e) => return Err(invalid("record observation", e)),
        }
        self.write(move |tx| {
            let _ = tx.execute(
                "INSERT INTO quota_observations (pool, source, at, exhausted, windows) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    o.pool,
                    o.source,
                    format_time(o.at),
                    i64::from(o.exhausted),
                    windows
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// The observations made since `since`, oldest first.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn observations(&self, since: OffsetDateTime) -> Result<Vec<ObservationRow>, Error> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pool, source, at, exhausted, windows FROM quota_observations \
                 WHERE at >= ?1 ORDER BY at, id",
            )?;
            let mut rows = stmt.query(params![format_time(since)])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push(ObservationRow {
                    pool: r.get(0)?,
                    source: r.get(1)?,
                    at: parse_time("quota_observations.at", &r.get::<_, String>(2)?)?,
                    exhausted: r.get::<_, i64>(3)? == 1,
                    windows: r.get(4)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Deletes observations older than `before`.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn prune_observations(&self, before: OffsetDateTime) -> Result<(), Error> {
        self.write(move |tx| {
            let _ = tx.execute(
                "DELETE FROM quota_observations WHERE at < ?1",
                params![format_time(before)],
            )?;
            Ok(())
        })
        .await
    }

    /// A stored switch, `None` when it was never set.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn flag(&self, name: &str) -> Result<Option<String>, Error> {
        let name = name.to_owned();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM flags WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    /// Stores a switch.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn set_flag(&self, name: &str, value: &str) -> Result<(), Error> {
        let (name, value, now) = (name.to_owned(), value.to_owned(), self.now());
        self.write(move |tx| {
            let _ = tx.execute(
                "INSERT INTO flags (name, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (name) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![name, value, format_time(now)],
            )?;
            Ok(())
        })
        .await
    }

    /// Stores the share of `pool`'s window one attempt used.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty pool or a share outside `[0, 1]`.
    pub async fn record_step_cost(
        &self,
        pool: &str,
        attempt: Option<i64>,
        share: f64,
        at: OffsetDateTime,
    ) -> Result<(), Error> {
        if pool.is_empty() || !(0.0..=1.0).contains(&share) {
            return Err(invalid(
                "record step cost",
                format!("pool {pool:?}, share {share}"),
            ));
        }
        let pool = pool.to_owned();
        self.write(move |tx| {
            let _ = tx.execute(
                "INSERT INTO usage (pool, attempt_id, share, at) VALUES (?1, ?2, ?3, ?4)",
                params![pool, attempt, share, format_time(at)],
            )?;
            Ok(())
        })
        .await
    }

    /// Each pool's last `n` step costs, oldest first.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn step_costs(&self, n: u32) -> Result<BTreeMap<String, Vec<f64>>, Error> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pool, share FROM ( \
                   SELECT pool, share, id, ROW_NUMBER() OVER (PARTITION BY pool ORDER BY id DESC) AS r \
                   FROM usage \
                 ) WHERE r <= ?1 ORDER BY pool, id",
            )?;
            let mut rows = stmt.query(params![n])?;
            let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            while let Some(r) = rows.next()? {
                out.entry(r.get(0)?).or_default().push(r.get(1)?);
            }
            Ok(out)
        })
        .await
    }

    /// Stores what an API pool spent.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty pool or a negative or non-finite amount.
    pub async fn record_spend(
        &self,
        pool: &str,
        usd: f64,
        at: OffsetDateTime,
    ) -> Result<(), Error> {
        if pool.is_empty() || !usd.is_finite() || usd < 0.0 {
            return Err(invalid("record spend", format!("pool {pool:?}, usd {usd}")));
        }
        let pool = pool.to_owned();
        self.write(move |tx| {
            let _ = tx.execute(
                "INSERT INTO spend (pool, usd, at) VALUES (?1, ?2, ?3)",
                params![pool, usd, format_time(at)],
            )?;
            Ok(())
        })
        .await
    }

    /// Each pool's spending since `since`, oldest first.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn spend_since(
        &self,
        since: OffsetDateTime,
    ) -> Result<BTreeMap<String, Vec<Spend>>, Error> {
        self.read(move |conn| {
            let mut stmt =
                conn.prepare("SELECT pool, usd, at FROM spend WHERE at >= ?1 ORDER BY at, id")?;
            let mut rows = stmt.query(params![format_time(since)])?;
            let mut out: BTreeMap<String, Vec<Spend>> = BTreeMap::new();
            while let Some(r) = rows.next()? {
                let spend = Spend {
                    usd: r.get(1)?,
                    at: parse_time("spend.at", &r.get::<_, String>(2)?)?,
                };
                out.entry(r.get(0)?).or_default().push(spend);
            }
            Ok(out)
        })
        .await
    }
}

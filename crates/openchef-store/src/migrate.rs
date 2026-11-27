//! Versioned, forward-only migrations.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use crate::Error;
use crate::sql::format_time;

/// One migration file: `NNNN_name.sql` and its SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The file name, e.g. `0001_init.sql`.
    pub file_name: String,
    /// The SQL to run.
    pub sql: String,
}

impl Migration {
    /// A migration from its file name and SQL.
    #[must_use]
    pub fn new(file_name: &str, sql: &str) -> Self {
        Self {
            file_name: file_name.to_owned(),
            sql: sql.to_owned(),
        }
    }
}

/// The migrations compiled into this binary.
pub(crate) fn embedded() -> Vec<Migration> {
    vec![
        Migration::new("0001_init.sql", include_str!("../migrations/0001_init.sql")),
        Migration::new(
            "0002_quota.sql",
            include_str!("../migrations/0002_quota.sql"),
        ),
    ]
}

struct Checked {
    version: i64,
    name: String,
    sql: String,
    checksum: String,
}

/// Validates names (`NNNN_[a-z0-9_]+.sql`) and requires versions 1..=n with no gaps.
fn check(migrations: &[Migration]) -> Result<Vec<Checked>, Error> {
    let mut out = Vec::with_capacity(migrations.len());
    for m in migrations {
        let bad = || Error::InvalidMigration(format!("bad file name {:?}", m.file_name));
        let stem = m.file_name.strip_suffix(".sql").ok_or_else(bad)?;
        let (digits, name) = stem.split_once('_').ok_or_else(bad)?;
        let name_ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) || !name_ok {
            return Err(bad());
        }
        let version: i64 = digits.parse().map_err(|_| bad())?;
        let checksum = hex(&Sha256::digest(m.sql.as_bytes()));
        out.push(Checked {
            version,
            name: name.to_owned(),
            sql: m.sql.clone(),
            checksum,
        });
    }
    out.sort_by_key(|m| m.version);
    for (expected, m) in (1_i64..).zip(&out) {
        if m.version != expected {
            return Err(Error::InvalidMigration(format!(
                "expected version {expected}, found {:04}_{}",
                m.version, m.name
            )));
        }
    }
    Ok(out)
}

/// Lower-case hexadecimal of `bytes`.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER NOT NULL PRIMARY KEY,
    name       TEXT    NOT NULL,
    checksum   TEXT    NOT NULL,
    applied_at TEXT    NOT NULL
) STRICT";

/// Verifies applied migrations and applies pending ones, each in its own immediate
/// transaction. Re-checking inside the transaction keeps concurrent openers safe even
/// without the daemon's single-instance lock.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<(), Error> {
    let migrations = check(migrations)?;
    let _ = conn.execute(CREATE_TABLE, [])?;
    let applied: BTreeMap<i64, String> = conn
        .prepare("SELECT version, checksum FROM schema_migrations")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let known = i64::try_from(migrations.len()).unwrap_or(i64::MAX);
    for (version, checksum) in &applied {
        let Some(m) = migrations.iter().find(|m| m.version == *version) else {
            return Err(Error::SchemaTooNew {
                found: *version,
                known,
            });
        };
        if &m.checksum != checksum {
            return Err(Error::MigrationModified(format!(
                "{:04}_{}",
                m.version, m.name
            )));
        }
    }
    for m in migrations
        .iter()
        .filter(|m| !applied.contains_key(&m.version))
    {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let done = tx
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                [m.version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !done {
            tx.execute_batch(&m.sql).map_err(|e| {
                Error::InvalidMigration(format!("apply {:04}_{}: {e}", m.version, m.name))
            })?;
            let _ = tx.execute(
                "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, ?4)",
                params![m.version, m.name, m.checksum, format_time(OffsetDateTime::now_utc())],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

//! Shared SQL helpers: time format, value conversion, compare-and-set failures.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use rusqlite::{OptionalExtension as _, Params, Transaction};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::Error;

/// Fixed width, so text order equals time order.
const TIME_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z");

/// Formats `t` in UTC.
pub(crate) fn format_time(t: OffsetDateTime) -> String {
    // The format has only numeric components, so formatting cannot fail for years 0–9999.
    t.to_offset(UtcOffset::UTC)
        .format(TIME_FORMAT)
        .unwrap_or_default()
}

pub(crate) fn parse_time(column: &'static str, s: &str) -> Result<OffsetDateTime, Error> {
    PrimitiveDateTime::parse(s, TIME_FORMAT)
        .map(PrimitiveDateTime::assume_utc)
        .map_err(|_| Error::Corrupt {
            column,
            value: s.to_owned(),
        })
}

pub(crate) fn parse_opt_time(
    column: &'static str,
    s: Option<&str>,
) -> Result<Option<OffsetDateTime>, Error> {
    s.map(|s| parse_time(column, s)).transpose()
}

/// Parses a stored vocabulary value.
pub(crate) fn parse_enum<T: FromStr>(column: &'static str, s: &str) -> Result<T, Error> {
    s.parse().map_err(|_| Error::Corrupt {
        column,
        value: s.to_owned(),
    })
}

pub(crate) fn path_text(what: &str, p: &Path) -> Result<String, Error> {
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Invalid(format!("{what} is not valid UTF-8: {}", p.display())))
}

pub(crate) fn opt_path_text(what: &str, p: Option<&PathBuf>) -> Result<Option<String>, Error> {
    p.map(|p| path_text(what, p)).transpose()
}

/// Stores an empty string as NULL.
pub(crate) fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Runs a guarded `UPDATE` and reports whether exactly one row changed.
pub(crate) fn exec_one<P: Params>(
    tx: &Transaction<'_>,
    sql: &str,
    params: P,
) -> Result<bool, Error> {
    Ok(tx.execute(sql, params)? == 1)
}

/// Explains why a guarded `UPDATE` touched no row: the row is missing (`NotFound`) or in
/// another state (`Conflict`). `state_sql` selects the row's current state.
pub(crate) fn cas_failed<P: Params>(
    tx: &Transaction<'_>,
    entity: &'static str,
    key: &str,
    want: &str,
    state_sql: &str,
    params: P,
) -> Error {
    match tx
        .query_row(state_sql, params, |r| r.get::<_, String>(0))
        .optional()
    {
        Ok(Some(got)) => Error::Conflict {
            entity,
            key: key.to_owned(),
            want: want.to_owned(),
            got,
        },
        Ok(None) => Error::NotFound(format!("{entity} {key}")),
        Err(e) => Error::Sqlite(e),
    }
}

/// The SQL list of terminal task states, e.g. `('cancelled', 'done', 'failed')`.
pub(crate) fn terminal_sql() -> String {
    let parts: Vec<String> = openchef_core::TaskStatus::terminal_statuses()
        .iter()
        .map(|s| format!("'{s}'"))
        .collect();
    format!("({})", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::{format_time, parse_time, terminal_sql};

    #[test]
    fn time_format_sorts() {
        let a = datetime!(2026-11-25 10:00:00 UTC);
        let b = a + time::Duration::nanoseconds(1500);
        let c = a + time::Duration::seconds(1);
        let mut got = vec![format_time(c), format_time(a), format_time(b)];
        got.sort();
        assert_eq!(got, vec![format_time(a), format_time(b), format_time(c)]);
        let shifted = b.to_offset(time::UtcOffset::from_hms(1, 0, 0).unwrap());
        assert_eq!(parse_time("t", &format_time(shifted)).unwrap(), b);
    }

    /// The SQL terminal list and the migration's partial index must match the domain.
    #[test]
    fn terminal_list_matches_domain() {
        assert_eq!(terminal_sql(), "('cancelled', 'done', 'failed')");
        let index = include_str!("../migrations/0001_init.sql")
            .lines()
            .find(|l| l.starts_with("CREATE INDEX tasks_active"))
            .unwrap();
        for s in openchef_core::TaskStatus::ALL {
            use openchef_core::StateMachine as _;
            assert_eq!(
                index.contains(&format!("'{s}'")),
                s.is_terminal(),
                "{s}: {index}"
            );
        }
    }
}

//! Ported from `internal/store/store_test.go` (Go reference): opening, migrations,
//! the writer and the read pool.
//!
//! Dropped: `TestCancelledContext` (Rust writes take no context; a write whose caller
//! stops waiting still commits or rolls back atomically) and the `'?'` case of
//! `TestOpenRejectsBadPaths` (a Go driver DSN limitation; rusqlite takes the path as is).

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::PathBuf;

use openchef_store::{Error, Migration, OpenOptions, Store};

const ITEMS: &str = "CREATE TABLE items (id INTEGER PRIMARY KEY, n INTEGER NOT NULL) STRICT;";
const TAGS: &str = "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;\nCREATE INDEX tags_name ON tags(name);";

fn base() -> Vec<Migration> {
    vec![
        Migration::new("0001_items.sql", ITEMS),
        Migration::new("0002_tags.sql", TAGS),
    ]
}

fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("state").join("state.db")
}

async fn open_test(path: &std::path::Path, migrations: Vec<Migration>) -> Result<Store, Error> {
    Store::open_with(
        path,
        OpenOptions {
            migrations: Some(migrations),
            clock: None,
        },
    )
    .await
}

async fn count(store: &Store, sql: &'static str) -> i64 {
    store
        .read(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn open_embedded_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&db_path(&dir)).await.unwrap();
    assert!(store.schema_version().await.unwrap() >= 1);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM meta").await, 0);
    store.close().await.unwrap();
}

#[tokio::test]
async fn open_creates_private_file_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let store = open_test(&path, base()).await.unwrap();
    assert_eq!(store.path(), path);
    let mode: String = store
        .read(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn migrations_are_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let store = open_test(&path, base()).await.unwrap();
    store
        .write(|tx| Ok(tx.execute("INSERT INTO items (n) VALUES (7)", [])?))
        .await
        .unwrap();
    store.close().await.unwrap();

    // Reopen with an extra migration: only the new one runs; data survives.
    let mut more = base();
    more.push(Migration::new(
        "0003_more.sql",
        "ALTER TABLE items ADD COLUMN note TEXT;",
    ));
    let store = open_test(&path, more).await.unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 3);
    assert_eq!(count(&store, "SELECT n FROM items").await, 7);
    store.close().await.unwrap();
}

#[tokio::test]
async fn schema_too_new_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    open_test(&path, base())
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let older = vec![Migration::new("0001_items.sql", ITEMS)];
    assert!(matches!(
        open_test(&path, older).await,
        Err(Error::SchemaTooNew { found: 2, known: 1 })
    ));
}

#[tokio::test]
async fn modified_migration_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    open_test(&path, base())
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let edited = vec![
        Migration::new("0001_items.sql", &format!("{ITEMS}\n-- edited")),
        Migration::new("0002_tags.sql", TAGS),
    ];
    assert!(matches!(
        open_test(&path, edited).await,
        Err(Error::MigrationModified(_))
    ));
}

#[tokio::test]
async fn failed_migration_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let broken = vec![
        Migration::new("0001_items.sql", ITEMS),
        Migration::new(
            "0002_broken.sql",
            "CREATE TABLE half (id INTEGER PRIMARY KEY) STRICT;\nTHIS IS NOT SQL;",
        ),
    ];
    assert!(open_test(&path, broken).await.is_err());
    let store = open_test(&path, vec![Migration::new("0001_items.sql", ITEMS)])
        .await
        .unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 1);
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'half'"
        )
        .await,
        0
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn invalid_migration_sets() {
    let cases = [
        (
            "gap",
            vec![
                Migration::new("0001_a.sql", "SELECT 1;"),
                Migration::new("0003_c.sql", "SELECT 1;"),
            ],
        ),
        ("bad name", vec![Migration::new("1_a.sql", "SELECT 1;")]),
        ("uppercase", vec![Migration::new("0001_A.sql", "SELECT 1;")]),
        ("not first", vec![Migration::new("0002_b.sql", "SELECT 1;")]),
    ];
    for (name, migrations) in cases {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            matches!(
                open_test(&db_path(&dir), migrations).await,
                Err(Error::InvalidMigration(_))
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn write_rolls_back_and_preserves_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_test(&db_path(&dir), base()).await.unwrap();
    let err = store
        .write(|tx| {
            let _ = tx.execute("INSERT INTO items (n) VALUES (1)", [])?;
            Err::<(), _>(Error::Invalid("boom".to_owned()))
        })
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m == "boom"), "{err:?}");
    assert_eq!(count(&store, "SELECT COUNT(*) FROM items").await, 0);
    store.close().await.unwrap();
}

const WORKERS: i64 = 8;
const PER_WORKER: i64 = 25;

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_are_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_test(&db_path(&dir), base()).await.unwrap();
    store
        .write(|tx| Ok(tx.execute("INSERT INTO items (id, n) VALUES (1, 0)", [])?))
        .await
        .unwrap();
    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..PER_WORKER {
                // Read-modify-write: lost updates would show up as a wrong total.
                store
                    .write(|tx| {
                        let n: i64 =
                            tx.query_row("SELECT n FROM items WHERE id = 1", [], |r| r.get(0))?;
                        Ok(tx.execute("UPDATE items SET n = ?1 WHERE id = 1", [n + 1])?)
                    })
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(
        count(&store, "SELECT n FROM items WHERE id = 1").await,
        WORKERS * PER_WORKER
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn reader_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_test(&db_path(&dir), base()).await.unwrap();
    let err = store
        .read(|c| Ok(c.execute("INSERT INTO items (n) VALUES (1)", [])?))
        .await
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("readonly")
            || err.to_string().contains("read-only"),
        "{err}"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn open_rejects_bad_paths() {
    assert!(matches!(
        Store::open(std::path::Path::new("")).await,
        Err(Error::Path(_))
    ));
}

#[tokio::test]
async fn closed_store_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_test(&db_path(&dir), base()).await.unwrap();
    let other = store.clone();
    store.close().await.unwrap();
    assert!(matches!(other.write(|_| Ok(())).await, Err(Error::Closed)));
}

/// Migration 0003 rebuilds `attempts` (D-037): existing attempts keep their rows as
/// `implement`, and the sessions that reference them survive the rebuild (foreign keys
/// are off while it runs, then checked).
#[tokio::test]
async fn attempt_roles_migration_keeps_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let old = vec![
        Migration::new("0001_init.sql", include_str!("../migrations/0001_init.sql")),
        Migration::new(
            "0002_quota.sql",
            include_str!("../migrations/0002_quota.sql"),
        ),
    ];
    let store = Store::open_with(
        &path,
        OpenOptions {
            migrations: Some(old),
            ..OpenOptions::default()
        },
    )
    .await
    .unwrap();
    store.close().await.unwrap();
    let t = "2026-11-28T00:00:00.000000000Z";
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         INSERT INTO tasks (id, kind, status, prompt, repo_root, mode, backend, importance,
                            permission_mode, created_at, updated_at)
             VALUES ('aaaaaaaaaa', 'run', 'done', 'p', '/r', 'isolated', 'local', 'normal',
                     'manual', '{t}', '{t}');
         INSERT INTO steps (task_id, idx, title, prompt, importance, status, updated_at)
             VALUES ('aaaaaaaaaa', 1, 's', 'p', 'normal', 'verified', '{t}');
         INSERT INTO attempts (id, task_id, step_idx, agent, runtime, pool, status, started_at)
             VALUES (7, 'aaaaaaaaaa', 1, 'opus', 'claude', 'max', 'succeeded', '{t}');
         INSERT INTO sessions (attempt_id, runtime, native_id, pid, start_token, state, opened_at)
             VALUES (7, 'claude', 'n1', 42, '1', 'closed', '{t}');"
    ))
    .unwrap();
    drop(conn);

    let store = Store::open(&path).await.unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 3);
    let id = "aaaaaaaaaa".parse().unwrap();
    let attempts = store.attempts(&id).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].role, openchef_core::AttemptRole::Implement);
    assert_eq!(
        store.task_sessions(&id, false).await.unwrap().len(),
        1,
        "sessions survived"
    );
    store.close().await.unwrap();
}

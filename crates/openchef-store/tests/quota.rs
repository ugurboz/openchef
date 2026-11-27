//! Ported from Go `internal/store/quota_test.go`: quota observations, flags, step costs
//! and spending.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::time::Duration;

use openchef_store::{ObservationRow, Store};
use time::OffsetDateTime;
use time::macros::datetime;

async fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state.db")).await.unwrap();
    (dir, store)
}

fn obs(
    pool: &str,
    source: &str,
    at: OffsetDateTime,
    exhausted: bool,
    windows: &str,
) -> ObservationRow {
    ObservationRow {
        pool: pool.into(),
        source: source.into(),
        at,
        exhausted,
        windows: windows.into(),
    }
}

#[tokio::test]
async fn observations_round_trip() {
    let (_dir, db) = open().await;
    let t0 = datetime!(2026-11-26 10:00 UTC);
    let five = r#"[{"name":"five_hour","utilization":0.4,"resetsAt":"2026-11-26T13:00:00Z","duration":18000}]"#;
    for o in [
        obs("claude-max", "L1", t0, false, five),
        obs("claude-max", "L2", t0 + Duration::from_mins(1), true, ""),
        obs("chatgpt", "L2", t0 + Duration::from_hours(2), false, "[]"),
    ] {
        db.record_observation(o).await.unwrap();
    }
    let got = db.observations(t0 + Duration::from_secs(30)).await.unwrap();
    assert_eq!(got.len(), 2);
    assert!(got[0].exhausted);
    assert_eq!(
        got[0].windows, "[]",
        "no windows are stored as an empty array"
    );
    let all = db.observations(OffsetDateTime::UNIX_EPOCH).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].windows, five, "windows come back as written");
    assert_eq!(all[0].at, t0);
    db.prune_observations(t0 + Duration::from_hours(1))
        .await
        .unwrap();
    let left = db.observations(OffsetDateTime::UNIX_EPOCH).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].pool, "chatgpt");
    assert!(
        db.record_observation(obs("p", "L9", t0, false, "[]"))
            .await
            .is_err(),
        "unknown source stored"
    );
    assert!(
        db.record_observation(obs("", "L1", t0, false, "[]"))
            .await
            .is_err()
    );
    assert!(
        db.record_observation(obs("p", "L1", t0, false, "{}"))
            .await
            .is_err(),
        "windows must be an array"
    );
}

#[tokio::test]
async fn flags() {
    let (_dir, db) = open().await;
    assert_eq!(db.flag("quota.probes").await.unwrap(), None);
    for v in ["off", "on"] {
        db.set_flag("quota.probes", v).await.unwrap();
        assert_eq!(db.flag("quota.probes").await.unwrap().as_deref(), Some(v));
    }
}

#[tokio::test]
async fn step_costs() {
    let (_dir, db) = open().await;
    let at = datetime!(2026-11-26 10:00 UTC);
    for (i, (pool, share)) in [("a", 0.01), ("a", 0.02), ("b", 0.5), ("a", 0.03)]
        .into_iter()
        .enumerate()
    {
        let minutes = u64::try_from(i).unwrap() * 60;
        db.record_step_cost(pool, None, share, at + Duration::from_secs(minutes))
            .await
            .unwrap();
    }
    let got = db.step_costs(2).await.unwrap();
    assert_eq!(got["a"], [0.02, 0.03]);
    assert_eq!(got["b"], [0.5]);
    assert!(
        db.record_step_cost("a", None, 1.5, at).await.is_err(),
        "share above 1 stored"
    );
    assert!(db.record_step_cost("", Some(1), 0.1, at).await.is_err());
}

#[tokio::test]
async fn spend() {
    let (_dir, db) = open().await;
    let at = datetime!(2026-11-26 10:00 UTC);
    for (i, usd) in [0.5, 1.25, 2.0].into_iter().enumerate() {
        let hours = u64::try_from(i).unwrap() * 3600;
        db.record_spend("api", usd, at + Duration::from_secs(hours))
            .await
            .unwrap();
    }
    let got = db.spend_since(at + Duration::from_mins(30)).await.unwrap();
    assert_eq!(got["api"].len(), 2);
    assert!((got["api"][0].usd - 1.25).abs() < f64::EPSILON);
    assert_eq!(got["api"][1].at, at + Duration::from_hours(2));
    assert!(
        db.record_spend("api", -1.0, at).await.is_err(),
        "negative spend stored"
    );
    assert!(db.record_spend("api", f64::NAN, at).await.is_err());
}

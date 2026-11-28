//! Review findings (D-038): recording a round, settling open findings, validation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use openchef_core::{Importance, Kind, Mode, PermissionMode, Task, TaskId, TaskStatus};
use openchef_store::{FindingRow, Store};
use time::macros::datetime;

async fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state.db")).await.unwrap();
    (dir, store)
}

#[tokio::test]
async fn findings_record_settle_and_list() {
    let (dir, db) = open().await;
    let now = datetime!(2026-11-28 10:00 UTC);
    let id = TaskId::generate().unwrap();
    let task = Task {
        id: id.clone(),
        kind: Kind::Pipeline,
        status: TaskStatus::Planning,
        prompt: "p".into(),
        repo_root: dir.path().to_path_buf(),
        base_commit: None,
        mode: Mode::Isolated,
        backend: openchef_core::Backend::Local,
        importance: Importance::Normal,
        agent_pin: None,
        permissions: PermissionMode::Manual,
        branch: None,
        worktree: None,
        resume_at: None,
        reason: None,
        created_at: now,
        updated_at: now,
    };
    db.create_task(task, Vec::new()).await.unwrap();
    let row = |f: &str, severity: &str, status: &str| FindingRow {
        id: 0,
        task_id: id.clone(),
        step_index: 1,
        round: 1,
        attempt_id: None,
        finding: f.into(),
        severity: severity.into(),
        file: Some("src/a.rs".into()),
        line: Some(3),
        contract: None,
        claim: "c".into(),
        evidence: "e".into(),
        fix: "f".into(),
        status: status.into(),
        note: None,
    };
    db.record_findings(vec![row("F1", "blocker", "open"), row("F2", "nit", "nit")])
        .await
        .unwrap();
    let changed = db
        .settle_findings(
            &id,
            1,
            1,
            vec![("F1".into(), Some("not a bug".into())), ("F2".into(), None)],
            "disputed",
        )
        .await
        .unwrap();
    assert_eq!(changed, 1, "only open findings change");
    let all = db.findings(&id).await.unwrap();
    assert_eq!(
        (all[0].status.as_str(), all[0].note.as_deref()),
        ("disputed", Some("not a bug"))
    );
    assert_eq!(all[1].status, "nit");
    assert!(
        db.record_findings(vec![row("F3", "fatal", "open")])
            .await
            .is_err()
    );
}

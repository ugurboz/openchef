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
    // An arbiter's verdict moves disputed findings only.
    let ruled = db
        .resettle_findings(
            &id,
            1,
            1,
            vec![
                ("F1".into(), Some("arbiter: the test covers it".into())),
                ("F2".into(), None),
            ],
            "disputed",
            "dismissed",
        )
        .await
        .unwrap();
    assert_eq!(ruled, 1, "F2 is a nit, not disputed");
    let all = db.findings(&id).await.unwrap();
    assert_eq!(
        (all[0].status.as_str(), all[0].note.as_deref()),
        ("dismissed", Some("arbiter: the test covers it"))
    );
    assert!(
        db.resettle_findings(&id, 1, 1, Vec::new(), "disputed", "upheld")
            .await
            .is_err()
    );
    // The plan council's findings live at step 0; an arbiter dismisses findings.
    let mut council = row("C1", "major", "dismissed");
    council.step_index = 0;
    db.record_findings(vec![council]).await.unwrap();
    let mut beyond = row("F4", "major", "open");
    beyond.step_index = 8;
    assert!(
        db.record_findings(vec![beyond]).await.is_err(),
        "steps end at 7"
    );
}

#[tokio::test]
async fn debate_budget_is_spent_up_to_its_limit() {
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
    assert_eq!(db.debate_turns(&id).await.unwrap(), 0);
    let spent = futures_spend(&db, &id, 3, 5).await;
    assert_eq!(spent, 3, "two callers never both take the last turn");
    assert_eq!(db.debate_turns(&id).await.unwrap(), 3);
    assert!(
        db.spend_debate_turn(&id, 4).await.unwrap(),
        "a raised limit gives more turns"
    );
    assert!(!db.spend_debate_turn(&id, 4).await.unwrap());
    let unknown = TaskId::generate().unwrap();
    assert!(
        db.spend_debate_turn(&unknown, 4)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(db.debate_turns(&unknown).await.unwrap_err().is_not_found());
}

/// Spends `tries` turns at once against `max`; returns how many were granted.
async fn futures_spend(db: &Store, id: &TaskId, max: u32, tries: usize) -> usize {
    let mut handles = Vec::new();
    for _ in 0..tries {
        let (db, id) = (db.clone(), id.clone());
        handles.push(tokio::spawn(
            async move { db.spend_debate_turn(&id, max).await },
        ));
    }
    let mut granted = 0;
    for h in handles {
        if h.await.unwrap().unwrap() {
            granted += 1;
        }
    }
    granted
}

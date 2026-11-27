//! Ported from `internal/task/task_test.go` (Go reference), plus Rust-only checks.
//!
//! Go tests that fed invalid strings into enum fields (`"bogus"` statuses, empty modes)
//! become parse tests here: in Rust such values cannot be constructed at all.

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::path::PathBuf;

use openchef_core::{
    ApprovalKind, ApprovalStatus, AttemptStatus, Backend, DecidedBy, Decision, Entity, Error,
    Importance, Kind, MAX_STEPS, Mode, PermissionMode, SessionState, StateMachine, Step,
    StepStatus, Task, TaskId, TaskStatus, TurnStatus, validate_plan,
};
use time::OffsetDateTime;

#[test]
fn new_id_round_trip() {
    let mut seen = HashSet::new();
    for _ in 0..1000 {
        let id = TaskId::generate().unwrap();
        let parsed: TaskId = id.as_str().parse().unwrap();
        assert_eq!(parsed, id);
        assert!(seen.insert(id.clone()), "duplicate id {id}");
    }
}

#[test]
fn parse_id_rejects() {
    for s in [
        "",
        "abc",
        "0123456789a",
        "ABCDEFGHJK",
        "abcdefghil",
        "abc/efghjk",
        "../../etc0",
        "abcdefghjé",
    ] {
        assert!(
            matches!(s.parse::<TaskId>(), Err(Error::Invalid(_))),
            "{s:?} should be rejected"
        );
    }
}

#[test]
fn task_transitions() {
    use TaskStatus::{
        AwaitingApproval, Cancelled, Done, Failed, NeedsAttention, Planning, Reviewing, Running,
        WaitingQuota,
    };
    let allowed = [
        (Planning, AwaitingApproval),
        (AwaitingApproval, Running),
        (Running, Done),
        (Running, Reviewing),
        (Running, WaitingQuota),
        (Running, NeedsAttention),
        (Reviewing, Running),
        (WaitingQuota, Running),
        (NeedsAttention, Running),
        (NeedsAttention, Cancelled),
    ];
    for (from, to) in allowed {
        assert_eq!(from.check_transition(to), Ok(()), "{from} → {to}");
    }
    let rejected = [
        (Done, Running),
        (Failed, Running),
        (Cancelled, Running),
        (Planning, Running),
        (AwaitingApproval, Done),
        (WaitingQuota, Done),
        (Running, Running),
    ];
    for (from, to) in rejected {
        assert_eq!(
            from.check_transition(to),
            Err(Error::InvalidTransition {
                entity: Entity::Task,
                from: from.as_str(),
                to: to.as_str()
            }),
            "{from} → {to}"
        );
    }
}

#[test]
fn unknown_status_strings_are_rejected() {
    // Go: `{StatusRunning, "bogus"}` and `{"bogus", StatusRunning}` transitions.
    assert_eq!(
        "bogus".parse::<TaskStatus>(),
        Err(Error::UnknownValue {
            kind: "task status",
            value: "bogus".to_owned()
        })
    );
    assert!("bogus".parse::<StepStatus>().is_err());
    assert!("bogus".parse::<AttemptStatus>().is_err());
    assert!("bogus".parse::<SessionState>().is_err());
    assert!("bogus".parse::<TurnStatus>().is_err());
    assert!("".parse::<Mode>().is_err());
    assert!("".parse::<Backend>().is_err());
    assert!("".parse::<Importance>().is_err());
    assert!("x".parse::<Kind>().is_err());
    assert!("default".parse::<PermissionMode>().is_err());
}

#[test]
fn terminal() {
    for s in [TaskStatus::Done, TaskStatus::Failed, TaskStatus::Cancelled] {
        assert!(s.is_terminal(), "{s} not terminal");
    }
    for s in [
        TaskStatus::Planning,
        TaskStatus::Running,
        TaskStatus::WaitingQuota,
        TaskStatus::NeedsAttention,
    ] {
        assert!(!s.is_terminal(), "{s} terminal");
    }
    assert!(!AttemptStatus::Running.is_terminal());
    assert!(AttemptStatus::FailedOver.is_terminal());
    assert_eq!(
        TaskStatus::terminal_statuses(),
        vec![TaskStatus::Cancelled, TaskStatus::Done, TaskStatus::Failed]
    );
}

#[test]
fn active() {
    let active: Vec<_> = TaskStatus::ALL
        .iter()
        .copied()
        .filter(|s| s.is_active())
        .collect();
    assert_eq!(active, vec![TaskStatus::Running, TaskStatus::Reviewing]);
}

#[test]
fn entity_transitions() {
    let checks = [
        (
            "step pending→running",
            StepStatus::Pending.check_transition(StepStatus::Running),
            true,
        ),
        (
            "step running→pending",
            StepStatus::Running.check_transition(StepStatus::Pending),
            true,
        ),
        (
            "step failed→pending",
            StepStatus::Failed.check_transition(StepStatus::Pending),
            true,
        ),
        (
            "step verified→running",
            StepStatus::Verified.check_transition(StepStatus::Running),
            false,
        ),
        (
            "step pending→verified",
            StepStatus::Pending.check_transition(StepStatus::Verified),
            false,
        ),
        (
            "attempt running→failed_over",
            AttemptStatus::Running.check_transition(AttemptStatus::FailedOver),
            true,
        ),
        (
            "attempt succeeded→running",
            AttemptStatus::Succeeded.check_transition(AttemptStatus::Running),
            false,
        ),
        (
            "session starting→open",
            SessionState::Starting.check_transition(SessionState::Open),
            true,
        ),
        (
            "session starting→closed",
            SessionState::Starting.check_transition(SessionState::Closed),
            true,
        ),
        (
            "session closed→open",
            SessionState::Closed.check_transition(SessionState::Open),
            false,
        ),
        (
            "turn sent→interrupted",
            TurnStatus::Sent.check_transition(TurnStatus::Interrupted),
            true,
        ),
        (
            "turn completed→failed",
            TurnStatus::Completed.check_transition(TurnStatus::Failed),
            false,
        ),
    ];
    for (name, result, ok) in checks {
        assert_eq!(result.is_ok(), ok, "{name}: {result:?}");
    }
}

#[test]
fn transition_error_names_the_entity() {
    let err = StepStatus::Verified
        .check_transition(StepStatus::Running)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"step cannot move from "verified" to "running""#
    );
}

#[test]
fn kind_initial_status() {
    assert_eq!(Kind::Run.initial_status(), TaskStatus::Running);
    assert_eq!(Kind::Pipeline.initial_status(), TaskStatus::Planning);
}

#[test]
fn importance_at_least() {
    assert_eq!(Importance::Low.at_least(Importance::High), Importance::High);
    assert_eq!(
        Importance::Critical.at_least(Importance::Normal),
        Importance::Critical
    );
}

/// A named change that makes a valid task invalid.
type Mutation = (&'static str, fn(&mut Task));

fn valid_task() -> Task {
    let now = OffsetDateTime::UNIX_EPOCH;
    Task {
        id: TaskId::generate().unwrap(),
        kind: Kind::Run,
        status: TaskStatus::Running,
        prompt: "fix it".to_owned(),
        repo_root: PathBuf::from("/repo"),
        base_commit: None,
        mode: Mode::Isolated,
        backend: Backend::Local,
        importance: Importance::Normal,
        agent_pin: None,
        permissions: PermissionMode::AcceptEdits,
        branch: None,
        worktree: None,
        resume_at: None,
        reason: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn task_validate() {
    let base = valid_task();
    assert_eq!(base.validate(), Ok(()));
    let mutations: [Mutation; 2] = [
        ("prompt", |t| t.prompt = "  ".to_owned()),
        ("repo", |t| t.repo_root = PathBuf::new()),
    ];
    for (name, mutate) in mutations {
        let mut t = base.clone();
        mutate(&mut t);
        assert!(matches!(t.validate(), Err(Error::Invalid(_))), "{name}");
    }
}

fn step(index: u32) -> Step {
    Step {
        task_id: "0123456789".parse().unwrap(),
        index,
        title: "t".to_owned(),
        prompt: "p".to_owned(),
        files: Vec::new(),
        verify: None,
        importance: Importance::Normal,
        status: StepStatus::Pending,
        retries: 0,
        verified_commit: None,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn validate_plan_cases() {
    assert_eq!(validate_plan(&[step(1), step(2)]), Ok(()));
    let too_many: Vec<Step> = (1..=u32::try_from(MAX_STEPS).unwrap() + 1)
        .map(step)
        .collect();
    let mut untitled = step(1);
    untitled.title = String::new();
    let mut no_prompt = step(1);
    no_prompt.prompt = " ".to_owned();
    let bad: [(&str, Vec<Step>); 6] = [
        ("empty", Vec::new()),
        ("too many", too_many),
        ("gap", vec![step(1), step(3)]),
        ("zero", vec![step(0)]),
        ("title", vec![untitled]),
        ("prompt", vec![no_prompt]),
    ];
    for (name, steps) in bad {
        assert!(
            matches!(validate_plan(&steps), Err(Error::Invalid(_))),
            "{name}"
        );
    }
}

#[test]
fn decision_validate() {
    let d = |status, answer: Option<&str>| Decision {
        status,
        answer: answer.map(str::to_owned),
        decided_by: DecidedBy::User,
    };
    assert_eq!(
        d(ApprovalStatus::Allowed, None).validate(ApprovalKind::Command),
        Ok(())
    );
    assert_eq!(
        d(ApprovalStatus::Answered, Some("yes")).validate(ApprovalKind::Question),
        Ok(())
    );
    assert_eq!(
        d(ApprovalStatus::Expired, None).validate(ApprovalKind::Question),
        Ok(())
    );
    for (status, answer, kind) in [
        (ApprovalStatus::Allowed, None, ApprovalKind::Question),
        (ApprovalStatus::Answered, Some("yes"), ApprovalKind::Command),
        (ApprovalStatus::Answered, Some(""), ApprovalKind::Question),
        (ApprovalStatus::Answered, None, ApprovalKind::Question),
        (ApprovalStatus::Pending, None, ApprovalKind::Tool),
    ] {
        assert!(
            matches!(d(status, answer).validate(kind), Err(Error::Invalid(_))),
            "{status} {answer:?} {kind}"
        );
    }
    assert!(
        ApprovalStatus::ALL
            .iter()
            .filter(|s| !s.is_final())
            .eq([&ApprovalStatus::Pending])
    );
}

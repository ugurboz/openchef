//! Properties of every state machine, and the string forms that the store and the API
//! depend on.

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use openchef_core::{
    ApprovalKind, ApprovalStatus, AttemptStatus, Backend, DecidedBy, Effort, Importance, Kind,
    Mode, PermissionMode, SessionState, StateMachine, StepStatus, TaskStatus, Tier, TurnKind,
    TurnStatus,
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

/// Walks the machine from `start` following `choices`; once a terminal state is
/// reached, every transition out of it must be rejected.
fn walk<S: StateMachine + std::fmt::Debug>(all: &[S], start: S, choices: &[usize]) {
    let mut state = start;
    for choice in choices {
        let next = state.next();
        if next.is_empty() {
            for &to in all {
                assert!(
                    state.check_transition(to).is_err(),
                    "{state:?} → {to:?} left a terminal state"
                );
            }
            return;
        }
        let to = next[choice % next.len()];
        assert_eq!(state.check_transition(to), Ok(()));
        state = to;
    }
}

proptest! {
    #[test]
    fn random_walks_never_leave_terminal_states(start in 0_usize..32, choices in prop::collection::vec(0_usize..16, 0..40)) {
        walk(TaskStatus::ALL, TaskStatus::ALL[start % TaskStatus::ALL.len()], &choices);
        walk(StepStatus::ALL, StepStatus::ALL[start % StepStatus::ALL.len()], &choices);
        walk(AttemptStatus::ALL, AttemptStatus::ALL[start % AttemptStatus::ALL.len()], &choices);
        walk(SessionState::ALL, SessionState::ALL[start % SessionState::ALL.len()], &choices);
        walk(TurnStatus::ALL, TurnStatus::ALL[start % TurnStatus::ALL.len()], &choices);
    }

    #[test]
    fn check_transition_matches_next(a in 0_usize..16, b in 0_usize..16) {
        let from = TaskStatus::ALL[a % TaskStatus::ALL.len()];
        let to = TaskStatus::ALL[b % TaskStatus::ALL.len()];
        prop_assert_eq!(from.check_transition(to).is_ok(), from.next().contains(&to));
    }
}

fn forms<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug + Copy>(
    all: &[T],
) -> Value {
    let values: Vec<Value> = all
        .iter()
        .map(|v| {
            let encoded = serde_json::to_value(v).unwrap();
            let decoded: T = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(&decoded, v, "serde round trip");
            encoded
        })
        .collect();
    Value::Array(values)
}

/// The string forms must equal the Go constants: they are persisted in SQLite and sent
/// over the API. Changing this snapshot breaks existing databases.
#[test]
fn string_forms_are_stable() {
    let mut map = Map::new();
    let _ = map.insert("task_status".into(), forms(TaskStatus::ALL));
    let _ = map.insert("step_status".into(), forms(StepStatus::ALL));
    let _ = map.insert("attempt_status".into(), forms(AttemptStatus::ALL));
    let _ = map.insert("session_state".into(), forms(SessionState::ALL));
    let _ = map.insert("turn_status".into(), forms(TurnStatus::ALL));
    let _ = map.insert("kind".into(), forms(Kind::ALL));
    let _ = map.insert("mode".into(), forms(Mode::ALL));
    let _ = map.insert("backend".into(), forms(Backend::ALL));
    let _ = map.insert("importance".into(), forms(Importance::ALL));
    let _ = map.insert("permission_mode".into(), forms(PermissionMode::ALL));
    let _ = map.insert("turn_kind".into(), forms(TurnKind::ALL));
    let _ = map.insert("effort".into(), forms(Effort::ALL));
    let _ = map.insert("tier".into(), forms(Tier::ALL));
    let _ = map.insert("approval_kind".into(), forms(ApprovalKind::ALL));
    let _ = map.insert("approval_status".into(), forms(ApprovalStatus::ALL));
    let _ = map.insert("decided_by".into(), forms(DecidedBy::ALL));
    insta::assert_json_snapshot!(Value::Object(map));
}

#[test]
fn levels_are_ordered() {
    assert!(Importance::Low < Importance::Normal && Importance::High < Importance::Critical);
    assert!(Effort::Low < Effort::Medium && Effort::High < Effort::Max);
    assert!(Tier::Fast < Tier::Standard && Tier::Standard < Tier::Frontier);
    assert_eq!(
        json!(PermissionMode::BypassPermissions),
        json!("bypassPermissions")
    );
}

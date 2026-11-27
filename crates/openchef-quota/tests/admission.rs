//! Ported from `internal/quota/admission_test.go`: measured step cost, the estimate,
//! headroom and admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
    clippy::float_cmp
)]

use std::time::Duration;

use openchef_quota::{Admission, Health, State, Window, admit, estimate, headroom, spent};
use time::OffsetDateTime;
use time::macros::datetime;

const T0: OffsetDateTime = datetime!(2026-11-26 10:00 UTC);
const HOUR: Duration = Duration::from_secs(3600);

fn window(name: &str, utilization: f64, resets_at: Option<OffsetDateTime>) -> Window {
    Window {
        name: name.to_owned(),
        utilization,
        resets_at,
        duration: None,
    }
}

#[test]
fn spent_measures_the_largest_increase() {
    let reset = T0 + 3 * HOUR;
    let before = State {
        updated: Some(T0),
        windows: vec![
            window("five_hour", 0.40, Some(reset)),
            window("weekly", 0.10, None),
        ],
        ..State::default()
    };
    let after = State {
        updated: Some(T0 + Duration::from_secs(600)),
        windows: vec![
            window("five_hour", 0.46, Some(reset)),
            window("weekly", 0.11, None),
        ],
        ..State::default()
    };
    let got = spent(&before, &after).unwrap();
    assert!((0.0599..=0.0601).contains(&got), "spent {got}");
    assert_eq!(
        spent(&before, &before),
        None,
        "no newer data must not count as a free step"
    );
    let reset2 = State {
        windows: vec![window("five_hour", 0.02, Some(reset + 5 * HOUR))],
        ..after.clone()
    };
    assert_eq!(
        spent(&before, &reset2),
        None,
        "a window that reset during the step says nothing"
    );
    assert_eq!(spent(&State::default(), &after), None, "no prior windows");
}

#[test]
fn estimate_is_the_windowed_p75() {
    assert_eq!(
        estimate(&[0.1, 0.2]),
        None,
        "two samples are not an estimate"
    );
    assert_eq!(estimate(&[0.01, 0.02, 0.03, 0.04, 0.10]), Some(0.04), "p75");
    // Only the last 20 count: an old expensive streak is forgotten.
    let mut costs = vec![0.5; 10];
    costs.extend([0.01; 20]);
    assert_eq!(estimate(&costs), Some(0.01), "windowed estimate");
}

#[test]
fn admit_by_health_and_headroom() {
    let at = |u: f64| State {
        updated: Some(T0),
        windows: vec![window("five_hour", u, Some(T0 + HOUR))],
        ..State::default()
    };
    let cases = [
        ("exhausted", Health::Exhausted, at(1.0), Some(0.01), false),
        ("unknown estimate", Health::Yellow, at(0.7), None, true),
        ("fits with margin", Health::Yellow, at(0.7), Some(0.2), true), // 0.26 ≤ 0.30
        (
            "margin too thin",
            Health::Yellow,
            at(0.7),
            Some(0.25),
            false,
        ), // 0.325 > 0.30
        ("red small step", Health::Red, at(0.9), Some(0.04), true),
        ("red big step", Health::Red, at(0.9), Some(0.06), false),
        (
            "no windows",
            Health::Unknown,
            State::default(),
            Some(0.5),
            true,
        ),
    ];
    for (name, h, s, est, want) in cases {
        let got = admit(h, &s, est);
        assert_eq!(matches!(got, Admission::Admitted), want, "{name}: {got:?}");
    }
    let (free, reset_in) = headroom(&at(0.7), T0);
    assert!(
        (0.2999..=0.3001).contains(&free) && reset_in == Some(HOUR),
        "headroom {free} in {reset_in:?}"
    );
}

#[test]
fn refusals_explain_themselves() {
    let s = State {
        updated: Some(T0),
        windows: vec![window("five_hour", 0.7, None)],
        ..State::default()
    };
    let Admission::Refused { reason } = admit(Health::Yellow, &s, Some(0.25)) else {
        panic!("admitted");
    };
    assert_eq!(reason, "headroom 30% < 32% (estimate with 30% margin)");
    let Admission::Refused { reason } = admit(Health::Red, &s, Some(0.06)) else {
        panic!("admitted");
    };
    assert_eq!(reason, "red: takes only small steps (estimate 6% > 5%)");
}

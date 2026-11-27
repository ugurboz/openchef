//! Ported from `internal/quota/quota_test.go`: merging, signals, health, exhaustion,
//! provider parsing, the probe policy (with a fake fetcher) and credential files.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
    clippy::float_cmp
)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use openchef_quota::{
    BoxFuture, ClaudeCredentials, Credentials, Error, Fetcher, Health, Observation, Prober,
    Source, State, Tracker, Window, codex_credentials, from_signal, health_of, parse_chatgpt,
    parse_claude, reset_at,
};
use openchef_runtime::{QuotaSignal, QuotaWindow};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio_util::sync::CancellationToken;

const T0: OffsetDateTime = datetime!(2026-11-26 10:00 UTC);

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

fn window(name: &str, utilization: f64, resets_at: Option<OffsetDateTime>) -> Window {
    Window {
        name: name.to_owned(),
        utilization,
        resets_at,
        duration: None,
    }
}

fn obs(at: OffsetDateTime, source: Source, windows: Vec<Window>) -> Observation {
    Observation {
        pool: "claude-max".to_owned(),
        source,
        at,
        windows,
        exhausted: false,
    }
}

#[test]
fn merge_within_and_across_windows() {
    let mut tr = Tracker::new();
    let reset = T0 + 3 * HOUR;
    tr.observe(obs(
        T0,
        Source::Stream,
        vec![window("five_hour", 0.50, Some(reset))],
    ));
    // A later but lower reading of the same window (another source, rounding) does not
    // lower utilization; the reset differs by seconds only.
    tr.observe(obs(
        T0 + MINUTE,
        Source::Probe,
        vec![window(
            "five_hour",
            0.40,
            Some(reset + Duration::from_secs(30)),
        )],
    ));
    let s = tr.state("claude-max", T0).unwrap();
    assert!(
        s.windows.len() == 1 && s.windows[0].utilization == 0.50 && s.sources.len() == 2,
        "same window: {s:?}"
    );
    // A later reset is a new window: it replaces the old one.
    tr.observe(obs(
        T0 + 2 * MINUTE,
        Source::Stream,
        vec![window("five_hour", 0.05, Some(reset + 5 * HOUR))],
    ));
    let s = tr.state("claude-max", T0).unwrap();
    assert_eq!(s.windows[0].utilization, 0.05, "new window: {s:?}");
    // A stale observation of the replaced window is ignored.
    tr.observe(obs(
        T0 + 3 * MINUTE,
        Source::Probe,
        vec![window("five_hour", 0.90, Some(reset))],
    ));
    let s = tr.state("claude-max", T0).unwrap();
    assert_eq!(s.windows[0].utilization, 0.05, "old window came back: {s:?}");
    // Past its reset a window no longer counts.
    let s = tr.state("claude-max", T0 + 9 * HOUR).unwrap();
    assert!(s.windows.is_empty(), "expired window kept: {s:?}");
}

#[test]
fn from_signal_canonicalizes_and_clamps() {
    let signal = QuotaSignal {
        exhausted: true,
        windows: vec![
            QuotaWindow {
                name: "seven_day".to_owned(),
                utilization: 1.3,
                resets_at: Some(T0 + HOUR),
                duration: None,
            },
            QuotaWindow {
                name: "five_hour".to_owned(),
                utilization: 0.2,
                resets_at: None,
                duration: None,
            },
        ],
    };
    let o = from_signal("p", T0, &signal);
    assert!(
        o.source == Source::Stream
            && o.exhausted
            && o.windows[0].name == "weekly"
            && o.windows[0].utilization == 1.0,
        "{o:?}"
    );
}

#[test]
fn health() {
    let at = |u: f64, age: Duration| {
        let s = State {
            windows: vec![window("weekly", 0.1, None), window("five_hour", u, None)],
            updated: Some(T0 - age),
            ..State::default()
        };
        health_of(&s, T0)
    };
    let twenty = 20 * MINUTE;
    let cases = [
        (0.10, Duration::ZERO, Health::Green),
        (0.599, Duration::ZERO, Health::Green),
        (0.60, Duration::ZERO, Health::Yellow),
        (0.849, Duration::ZERO, Health::Yellow),
        (0.85, Duration::ZERO, Health::Red),
        (0.989, Duration::ZERO, Health::Red),
        (0.99, Duration::ZERO, Health::Exhausted),
        (0.10, twenty, Health::Yellow),
        (0.70, twenty, Health::Red),
        (0.90, twenty, Health::Red),
        (1.0, 48 * HOUR, Health::Exhausted),
    ];
    for (u, age, want) in cases {
        assert_eq!(at(u, age), want, "u={u:.3} age={age:?}");
    }
    assert_eq!(health_of(&State::default(), T0), Health::Unknown, "no data");
    let reported = State {
        exhausted: true,
        windows: vec![window("", 0.2, None)],
        ..State::default()
    };
    assert_eq!(
        health_of(&reported, T0),
        Health::Exhausted,
        "reported exhaustion"
    );
}

#[test]
fn exhaustion_ends_at_reset() {
    let mut tr = Tracker::new();
    let reset = T0 + HOUR;
    let mut o = obs(
        T0,
        Source::Stream,
        vec![window("five_hour", 1.0, Some(reset))],
    );
    o.exhausted = true;
    tr.observe(o);
    let s = tr.state("claude-max", T0).unwrap();
    assert!(s.exhausted && reset_at(&s) == Some(reset), "{s:?}");
    let s = tr.state("claude-max", T0 + 2 * HOUR).unwrap();
    assert!(!s.exhausted, "still exhausted after the reset: {s:?}");
}

const CLAUDE_BODY: &str = r#"{
  "five_hour": {"utilization": 55.0, "resets_at": "2026-11-26T14:00:00.123456+00:00", "limit_dollars": null},
  "seven_day": {"utilization": 12.5, "resets_at": "2026-11-30T09:00:00+00:00"},
  "seven_day_opus": null, "tangelo": null, "extra_usage": {"is_enabled": false}
}"#;

const CHATGPT_BODY: &str = r#"{
  "plan_type": "plus",
  "rate_limit": {
    "allowed": true, "limit_reached": false,
    "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_after_seconds": 600, "reset_at": 1795701600},
    "secondary_window": {"used_percent": 7, "limit_window_seconds": 604800, "reset_after_seconds": 9000, "reset_at": 1796200000}
  },
  "credits": {"has_credits": false}
}"#;

#[test]
fn parse_claude_reads_both_windows() {
    let o = parse_claude(CLAUDE_BODY.as_bytes(), T0).unwrap();
    assert!(
        o.windows.len() == 2
            && o.windows[0].name == "five_hour"
            && o.windows[0].utilization == 0.55
            && o.windows[0].resets_at.unwrap().hour() == 14
            && o.windows[1].name == "weekly"
            && o.windows[1].utilization == 0.125,
        "{o:?}"
    );
    for (name, body) in [
        ("not json", "<html>"),
        ("no windows", r#"{"tangelo": null}"#),
        (
            "no utilization",
            r#"{"five_hour": {"resets_at": "2026-11-26T14:00:00Z"}}"#,
        ),
        (
            "bad time",
            r#"{"five_hour": {"utilization": 1, "resets_at": "tomorrow"}}"#,
        ),
        ("utilization a str", r#"{"five_hour": {"utilization": "55%"}}"#),
    ] {
        assert!(
            matches!(
                parse_claude(body.as_bytes(), T0),
                Err(Error::ShapeChanged { .. })
            ),
            "{name} accepted"
        );
    }
}

#[test]
fn parse_chatgpt_reads_windows_by_length() {
    let o = parse_chatgpt(CHATGPT_BODY.as_bytes(), T0).unwrap();
    assert!(
        !o.exhausted
            && o.windows.len() == 2
            && o.windows[0].name == "five_hour"
            && o.windows[0].utilization == 0.42
            && o.windows[1].name == "weekly"
            && o.windows[1].duration == Some(7 * 24 * HOUR),
        "{o:?}"
    );
    let free = r#"{"rate_limit": {"limit_reached": true, "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1795701600}, "secondary_window": null}}"#;
    let o = parse_chatgpt(free.as_bytes(), T0).unwrap();
    assert!(o.exhausted && o.windows.len() == 1, "free plan: {o:?}");
    assert!(
        parse_chatgpt(br#"{"rate_limit": {"allowed": true}}"#, T0).is_err(),
        "missing fields accepted"
    );
}

/// A fetcher answering with a settable status code and a fixed body, the way the Go
/// test's TLS server did.
struct FakeFetcher {
    status: Arc<AtomicU16>,
    body: &'static str,
}

impl Fetcher for FakeFetcher {
    fn fetch(&self, _cancel: CancellationToken) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        let code = self.status.load(Ordering::SeqCst);
        let body = self.body.as_bytes().to_vec();
        Box::pin(async move {
            if code == 200 {
                Ok(body)
            } else {
                Err(Error::Status {
                    host: "api.example".to_owned(),
                    code,
                })
            }
        })
    }
}

fn test_prober(status: &Arc<AtomicU16>, body: &'static str) -> Prober {
    let fetcher = FakeFetcher {
        status: Arc::clone(status),
        body,
    };
    Prober::new("claude-max", "claude", Box::new(fetcher), parse_claude)
}

#[tokio::test]
async fn prober_policy() {
    let cancel = CancellationToken::new;
    let mut now = T0;
    let status = Arc::new(AtomicU16::new(200));
    let mut p = test_prober(&status, CLAUDE_BODY);

    let o = p.run(now, cancel()).await.unwrap();
    assert!(
        o.pool == "claude-max" && o.source == Source::Probe && o.windows.len() == 2,
        "first run: {o:?}"
    );
    let second = p.run(now, cancel()).await;
    assert!(
        matches!(second, Err(Error::Muted { .. })),
        "second run within 30 s = {second:?}"
    );

    // 429: back off for 5 min, then double.
    status.store(429, Ordering::SeqCst);
    now += MINUTE;
    let limited = p.run(now, cancel()).await;
    assert!(
        matches!(limited, Err(Error::Status { code: 429, .. })),
        "429 = {limited:?}"
    );
    now += 4 * MINUTE;
    assert!(
        matches!(p.status(now), Err(Error::Muted { .. })),
        "4 min after 429 = {:?}",
        p.status(now)
    );
    now += 2 * MINUTE;
    assert!(
        p.status(now).is_ok(),
        "after the backoff = {:?}",
        p.status(now)
    );

    // Three server errors mute it for 30 min.
    status.store(503, Ordering::SeqCst);
    for _ in 0..3 {
        now += MINUTE;
        let failed = p.run(now, cancel()).await;
        assert!(matches!(failed, Err(Error::Status { code: 503, .. })));
    }
    assert!(
        matches!(p.status(now), Err(Error::Muted { .. })),
        "after three 5xx = {:?}",
        p.status(now)
    );
    now += 31 * MINUTE;

    // The kill switch.
    status.store(200, Ordering::SeqCst);
    p.set_enabled(false);
    let off = p.run(now, cancel()).await;
    assert!(matches!(off, Err(Error::Disabled)), "disabled = {off:?}");
    p.set_enabled(true);
    let on = p.run(now, cancel()).await;
    assert!(on.is_ok(), "re-enabled = {on:?}");
}

#[tokio::test]
async fn prober_backoff_doubles_up_to_an_hour() {
    let status = Arc::new(AtomicU16::new(429));
    let mut p = test_prober(&status, CLAUDE_BODY);
    let mut now = T0;
    let mut waits = Vec::new();
    for _ in 0..6 {
        let failed = p.run(now, CancellationToken::new()).await;
        assert!(matches!(failed, Err(Error::Status { code: 429, .. })));
        let Err(Error::Muted { until }) = p.status(now) else {
            panic!("not muted after 429");
        };
        waits.push((until - now).whole_minutes());
        now = until;
    }
    assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
}

#[tokio::test]
async fn prober_mutes_on_rejected_login() {
    for code in [401, 403] {
        let status = Arc::new(AtomicU16::new(code));
        let mut p = test_prober(&status, CLAUDE_BODY);
        assert!(p.run(T0, CancellationToken::new()).await.is_err());
        let Err(Error::Muted { until }) = p.status(T0) else {
            panic!("{code} did not mute");
        };
        assert_eq!(until - T0, 30 * MINUTE, "{code}");
    }
}

#[tokio::test]
async fn prober_disables_on_shape_change() {
    let status = Arc::new(AtomicU16::new(200));
    let mut p = test_prober(&status, r#"{"five_hour": {"utilization": "a lot"}}"#);
    let first = p.run(T0, CancellationToken::new()).await;
    assert!(
        matches!(first, Err(Error::ShapeChanged { .. })),
        "run = {first:?}; want ShapeChanged"
    );
    let later = p.run(T0 + HOUR, CancellationToken::new()).await;
    assert!(
        matches!(later, Err(Error::ShapeChanged { .. })),
        "still disabled = {later:?}"
    );
    assert!(matches!(p.status(T0 + HOUR), Err(Error::ShapeChanged { .. })));
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[tokio::test]
async fn credentials() {
    let home = tempfile::tempdir().unwrap();
    let env = |_: &str| None;
    let codex = codex_credentials(home.path(), env);
    let missing = codex.read().await;
    assert!(
        matches!(missing, Err(Error::NoCredentials { .. })),
        "missing auth.json = {missing:?}"
    );
    let auth = home.path().join(".codex").join("auth.json");
    write(
        &auth,
        r#"{"tokens": {"access_token": "tok", "account_id": "acc"}}"#,
    );
    let login = codex.read().await.unwrap();
    assert_eq!(login.token.expose(), "tok");
    assert_eq!(login.account.as_deref(), Some("acc"));
    write(&auth, r#"{"OPENAI_API_KEY": "sk-x", "tokens": null}"#);
    let api_key = codex.read().await;
    assert!(
        matches!(api_key, Err(Error::NoCredentials { .. })),
        "API-key login = {api_key:?}"
    );

    // Claude: the credentials file (the keychain is only consulted on macOS, so the
    // test reads the file directly to stay independent of the machine).
    let dir = home.path().join("claude");
    let file = dir.join(".credentials.json");
    let expires = |at: OffsetDateTime| (at.unix_timestamp_nanos() / 1_000_000).to_string();
    write(
        &file,
        &format!(
            r#"{{"claudeAiOauth": {{"accessToken": "c-tok", "expiresAt": {}}}}}"#,
            expires(T0 + HOUR)
        ),
    );
    let claude = ClaudeCredentials::from_dir(&dir).with_clock(|| T0);
    let login = claude.read().await.unwrap();
    assert_eq!(login.token.expose(), "c-tok");
    write(
        &file,
        &format!(
            r#"{{"claudeAiOauth": {{"accessToken": "c-tok", "expiresAt": {}}}}}"#,
            expires(T0 - HOUR)
        ),
    );
    let expired = claude.read().await;
    match expired {
        Err(e @ Error::NoCredentials { .. }) => {
            assert!(e.to_string().contains("expired"), "expired = {e}");
        }
        other => panic!("expired = {other:?}"),
    }
}

#[tokio::test]
async fn credential_dirs_follow_the_environment() {
    let home = tempfile::tempdir().unwrap();
    let custom = home.path().join("elsewhere");
    write(
        &custom.join("auth.json"),
        r#"{"tokens": {"access_token": "env-tok"}}"#,
    );
    let codex = codex_credentials(home.path(), |k: &str| {
        (k == "CODEX_HOME").then(|| custom.to_string_lossy().into_owned())
    });
    let login = codex.read().await.unwrap();
    assert_eq!(login.token.expose(), "env-tok");
    assert_eq!(login.account, None);
}

#[test]
fn tokens_never_print() {
    let token = openchef_quota::Token::new("sekret-token".to_owned());
    assert!(!format!("{token:?}").contains("sekret"));
}

#[test]
fn serde_matches_the_go_json() {
    let o = Observation {
        pool: "p".to_owned(),
        source: Source::Probe,
        at: T0,
        windows: vec![Window {
            name: "five_hour".to_owned(),
            utilization: 0.5,
            resets_at: Some(T0 + HOUR),
            duration: Some(5 * HOUR),
        }],
        exhausted: false,
    };
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "pool": "p", "source": "L2", "at": "2026-11-26T10:00:00Z",
            "windows": [{"name": "five_hour", "utilization": 0.5,
                         "resetsAt": "2026-11-26T11:00:00Z", "duration": 18000}]
        })
    );
    let back: Observation = serde_json::from_value(v).unwrap();
    assert_eq!(back, o);
    let bare: Window = serde_json::from_str(r#"{"name": "weekly", "utilization": 0}"#).unwrap();
    assert_eq!(bare.resets_at, None);
    assert_eq!(bare.duration, None);
    assert_eq!(Health::Exhausted.as_str(), "exhausted");
}

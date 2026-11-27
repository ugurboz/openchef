//! Ported from `internal/runtime/runtime_test.go` and `rawlog/rawlog_test.go`.

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::Path;

use openchef_core::{Effort, PermissionMode};
use openchef_runtime::rawlog::{Dir, RawLog, read_file};
use openchef_runtime::{
    Decision, Error, EventBody, Failure, FailureKind, nearest_effort, parse_mode,
};

#[test]
fn effort_nearest() {
    use Effort::{High, Low, Max, Medium};
    let cases = [
        (Some(High), &[Low, Medium, High, Max][..], Some(High)),
        (Some(Max), &[Low, Medium, High][..], Some(High)),
        (Some(Medium), &[Low, High][..], Some(Low)),
        (Some(Low), &[Medium, High][..], Some(Medium)),
        (Some(High), &[][..], None),
        (None, &[Low][..], None),
    ];
    for (wanted, supported, want) in cases {
        assert_eq!(
            nearest_effort(wanted, supported),
            want,
            "{wanted:?} of {supported:?}"
        );
    }
}

#[test]
fn parse_mode_accepts_legacy_default() {
    assert_eq!(parse_mode("default").unwrap(), PermissionMode::Manual);
    assert_eq!(
        parse_mode("acceptEdits").unwrap(),
        PermissionMode::AcceptEdits
    );
    assert!(parse_mode("yolo").is_err());
}

#[test]
fn terminal_kinds() {
    assert!(EventBody::TurnFailed(Failure::new(FailureKind::Crashed, "x")).is_terminal());
    assert!(!EventBody::Text("x".into()).is_terminal());
}

#[test]
fn protocol_error_truncates_line() {
    let e = Error::Protocol {
        runtime: "codex",
        reason: "bad".into(),
        line: "x".repeat(500),
    };
    let msg = e.to_string();
    assert!(msg.ends_with("…)") && msg.len() < 300, "{msg}");
}

#[test]
fn decision_helpers() {
    assert!(Decision::allow().allow);
    let d = Decision::deny("no");
    assert!(!d.allow && d.message == "no");
}

#[test]
fn rawlog_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s").join("raw.jsonl");
    let log = RawLog::open(&path).unwrap();
    log.line(Dir::Send, br#"{"a":1}"#);
    log.line(Dir::Recv, b"not json");
    log.note(Dir::Stderr, "warning: x");
    assert_eq!(log.error(), None);
    let records = read_file(&path).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].msg.as_ref().unwrap()["a"], 1);
    assert_eq!(records[1].text.as_deref(), Some("not json"));
    assert_eq!(
        (records[2].dir, records[2].text.as_deref()),
        (Dir::Stderr, Some("warning: x"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn discard_log_is_safe() {
    let log = RawLog::discard();
    log.line(Dir::Send, b"{}");
    log.note(Dir::Note, "x");
    assert_eq!(log.error(), None);
}

#[test]
fn write_error_is_remembered_not_fatal() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = RawLog::to_writer(Box::new(Broken));
    log.line(Dir::Send, b"{}");
    log.line(Dir::Send, b"{}");
    assert!(log.error().unwrap().contains("disk full"));
}

#[test]
fn read_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_file(&dir.path().join("missing")).is_err());
    let bad = dir.path().join("bad.jsonl");
    std::fs::write(&bad, "{\"ms\":1,\"dir\":\"send\"}\nnot json\n").unwrap();
    assert!(read_file(&bad).unwrap_err().to_string().contains("line 2"));
}

/// Every recorded transcript in `testdata/` decodes.
#[test]
fn fixtures_are_readable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    let mut count = 0;
    for runtime in ["claude", "codex", "agy"] {
        for entry in std::fs::read_dir(root.join(runtime)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let records =
                    read_file(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!records.is_empty(), "{}", path.display());
                count += 1;
            }
        }
    }
    assert!(count >= 30, "found {count} fixtures");
}

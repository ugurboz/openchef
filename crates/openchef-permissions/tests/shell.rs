//! The splitter against the S4 corpus (D-031) and the Go `FuzzSplit` invariants: every
//! corpus case is identical to the Go oracle (`mvdan/sh`), or a documented difference
//! where the Rust splitter is stricter (fails closed) or more correct. A brush-parser
//! upgrade must pass this test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::time::{Duration, Instant};

use openchef_permissions::shell::{Simple, split};
use proptest::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct GoSimple {
    #[serde(rename = "Args", default)]
    args: Option<Vec<String>>,
    #[serde(rename = "Dynamic")]
    dynamic: bool,
    #[serde(rename = "Writes", default)]
    writes: Option<Vec<String>>,
    #[serde(rename = "Reads", default)]
    reads: Option<Vec<String>>,
}

impl From<GoSimple> for Simple {
    fn from(g: GoSimple) -> Self {
        Self {
            args: g.args.unwrap_or_default(),
            dynamic: g.dynamic,
            writes: g.writes.unwrap_or_default(),
            reads: g.reads.unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
struct Case {
    input: String,
    #[serde(default)]
    error: Option<String>,
    commands: Vec<GoSimple>,
}

/// Inputs where Rust deliberately differs, with the verdict from the S4 findings.
const KNOWN: &[(&str, &str)] = &[
    ("()0", "fail closed: malformed input mvdan accepts"),
    ("echo hi\0rm", "fail closed: NUL byte"),
    (
        "echo ${x:-$(rm y)}",
        "fail closed: substitution inside a parameter expansion",
    ),
    (
        "> out.log",
        "Rust correct: a bare redirection writes the file; Go reports nothing",
    ),
    (
        "select x in a; do rm; done",
        "fail closed: brush-parser has no `select`",
    ),
];

#[test]
fn matches_go_or_is_documented() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../testdata/s4-corpus.json")).unwrap();
    assert!(cases.len() > 140, "corpus truncated: {} cases", cases.len());
    let mut unexpected = Vec::new();
    for c in cases {
        let rust = split(&c.input);
        let go: Vec<Simple> = c.commands.into_iter().map(Simple::from).collect();
        let same = match (&rust, &c.error) {
            (Err(_), Some(_)) => true,
            (Ok(r), None) => *r == go,
            _ => false,
        };
        let known = KNOWN.iter().any(|(k, _)| *k == c.input);
        if same == known {
            unexpected.push(format!("{:?}: go={go:?} rust={rust:?}", c.input));
        }
    }
    assert!(unexpected.is_empty(), "{unexpected:#?}");
}

#[test]
fn hostile_nesting_fails_closed_without_crashing() {
    for line in [
        "(".repeat(100_000),
        "$(".repeat(5_000),
        "`".repeat(200),
        "x".repeat(70 * 1024),
    ] {
        assert!(split(&line).is_err(), "{} bytes accepted", line.len());
    }
    // Just under the guard still parses on the parser's own stack.
    // `((` would be arithmetic; nested subshells need the space.
    let deep = format!("{}true{}", "( ".repeat(60), " )".repeat(60));
    assert_eq!(split(&deep).unwrap().len(), 1);
}

const ALPHABET: &str = "abc xyz;&|()<>{}[]$`'\"\\!*?~=-0123456789#\n\t";

fn shellish() -> impl Strategy<Value = String> {
    let chars: Vec<char> = ALPHABET.chars().collect();
    prop::collection::vec(prop::sample::select(chars), 0..60).prop_map(|v| v.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("FUZZ_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(2000),
        ..ProptestConfig::default()
    })]

    /// Go `FuzzSplit`: no input makes the splitter panic or hang, and it never returns
    /// an empty static command.
    #[test]
    fn fuzz_split(input in shellish()) {
        let start = Instant::now();
        if let Ok(cmds) = split(&input) {
            for c in &cmds {
                prop_assert!(!c.args.is_empty() || c.dynamic, "empty static command for {input:?}");
            }
        }
        prop_assert!(start.elapsed() < Duration::from_secs(5), "{input:?} took {:?}", start.elapsed());
    }

    #[test]
    fn arbitrary_unicode_never_panics(input in "\\PC{0,80}") {
        let _result = split(&input);
    }
}

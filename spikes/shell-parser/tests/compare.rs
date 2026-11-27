//! Every corpus case is identical to the Go oracle, or a documented difference where
//! the Rust splitter is stricter (fails closed) or more correct.

use serde::Deserialize;
use shell_parser_spike::{Simple, split};

#[derive(Deserialize)]
struct Case {
    input: String,
    #[serde(default)]
    error: Option<String>,
    commands: Vec<Simple>,
}

/// Inputs where Rust deliberately differs, with the verdict from the findings.
const KNOWN: &[(&str, &str)] = &[
    ("()0", "fail closed: malformed input mvdan accepts"),
    ("echo hi\0rm", "fail closed: NUL byte"),
    ("echo ${x:-$(rm y)}", "fail closed: substitution inside a parameter expansion"),
    ("> out.log", "Rust correct: a bare redirection writes the file; Go reports nothing"),
    ("select x in a; do rm; done", "fail closed: brush-parser has no `select`"),
];

#[test]
fn matches_go_or_is_documented() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("../corpus.json")).unwrap();
    let mut unexpected = Vec::new();
    for c in &cases {
        let rust = split(&c.input);
        let same = match (&rust, &c.error) {
            (Err(_), Some(_)) => true,
            (Ok(r), None) => *r == c.commands,
            _ => false,
        };
        if same == KNOWN.iter().any(|(k, _)| *k == c.input) {
            unexpected.push(format!("{:?}: go={:?} rust={:?}", c.input, c.commands, rust));
        }
    }
    assert!(unexpected.is_empty(), "{unexpected:#?}");
}

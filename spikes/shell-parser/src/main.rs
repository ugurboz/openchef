//! Compares the Rust splitter with the Go oracle on `corpus.json` and prints a
//! Markdown report: `cargo run --release > report.md`.

use serde::Deserialize;
use shell_parser_spike::{Simple, split};

#[derive(Deserialize)]
struct Case {
    input: String,
    #[serde(default)]
    error: Option<String>,
    commands: Vec<Simple>,
}

fn show(cmds: &[Simple]) -> String {
    let parts: Vec<String> = cmds
        .iter()
        .map(|c| {
            let mut s = format!("`{}`", c.args.join(" ").replace('`', "'"));
            if c.dynamic {
                s.push_str(" dyn");
            }
            if !c.writes.is_empty() {
                s.push_str(&format!(" w={:?}", c.writes));
            }
            if !c.reads.is_empty() {
                s.push_str(&format!(" r={:?}", c.reads));
            }
            s
        })
        .collect();
    if parts.is_empty() { "(none)".to_owned() } else { parts.join(" ; ") }
}

fn main() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("../corpus.json")).expect("corpus.json");
    let (mut same, mut diff) = (0, Vec::new());
    for c in &cases {
        let rust = split(&c.input);
        let equal = match (&rust, &c.error) {
            (Err(_), Some(_)) => true,
            (Ok(r), None) => *r == c.commands,
            _ => false,
        };
        if equal {
            same += 1;
        } else {
            let r = match &rust {
                Ok(r) => show(r),
                Err(e) => format!("unparsable ({})", e.0),
            };
            let g = if c.error.is_some() { "unparsable".to_owned() } else { show(&c.commands) };
            diff.push(format!("| `{}` | {g} | {r} |", c.input.escape_debug().to_string().replace('|', "\\|").chars().take(80).collect::<String>()));
        }
    }
    println!("# S4 comparison: {} cases, {same} identical, {} different\n", cases.len(), diff.len());
    println!("| Input | Go (mvdan/sh) | Rust (brush-parser) |\n|---|---|---|");
    for d in &diff {
        println!("{}", d.replace('\n', " "));
    }
}

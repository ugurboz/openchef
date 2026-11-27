//! Probes deeply nested inputs, which could overflow the parser's stack.
use std::time::Instant;
fn main() {
    let n: usize = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(1000);
    for (name, input) in [
        ("subshells", format!("{}ls{}", "(".repeat(n), ")".repeat(n))),
        ("braces", format!("{}ls;{}", "{ ".repeat(n), " }".repeat(n))),
        ("substitutions", format!("{}ls{}", "$(".repeat(n), ")".repeat(n))),
        ("pipes", vec!["ls"; n].join(" | ")),
        ("and-lists", vec!["ls"; n].join(" && ")),
        ("long word", "a".repeat(n * 100)),
    ] {
        let t = Instant::now();
        let r = shell_parser_spike::split(&input);
        println!("{name:14} n={n:6} {:>8.1?} {}", t.elapsed(), match r { Ok(c) => format!("ok ({} commands)", c.len()), Err(e) => format!("unparsable: {}", e.0.chars().take(60).collect::<String>()) });
    }
}

//! No input makes the splitter panic or hang, and it never returns an empty static
//! command (the Go FuzzSplit invariants).

use std::time::{Duration, Instant};

use proptest::prelude::*;
use shell_parser_spike::split;

const ALPHABET: &str = r#"abc xyz;&|()<>{}[]$`'"\!*?~=-0123456789#
	"#;

fn shellish() -> impl Strategy<Value = String> {
    let chars: Vec<char> = ALPHABET.chars().collect();
    prop::collection::vec(prop::sample::select(chars), 0..60).prop_map(|v| v.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("FUZZ_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(5000), ..ProptestConfig::default() })]
    #[test]
    fn never_panics_or_hangs(input in shellish()) {
        let start = Instant::now();
        if let Ok(cmds) = split(&input) {
            for c in &cmds {
                prop_assert!(!(c.args.is_empty() && !c.dynamic), "empty static command for {input:?}");
            }
        }
        prop_assert!(start.elapsed() < Duration::from_secs(1), "{input:?} took {:?}", start.elapsed());
    }

    #[test]
    fn arbitrary_unicode_never_panics(input in "\\PC{0,80}") {
        let _ = split(&input);
    }
}

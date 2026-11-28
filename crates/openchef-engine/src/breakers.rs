//! Stagnation signals (§7.5 circuit breakers, D-040): the same verification failure
//! twice in a row, a worktree that returns to an earlier state, a finding reopened after
//! it was fixed. They are kept for one step run; a failover or a restart starts over.

use std::path::Path;

use crate::review::Finding;
use crate::verify::Outcome;

/// A step's loop state: the §7.2 retry budget and the stagnation signals.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// Feedback turns on this step, across attempts.
    pub(crate) retries: u32,
    last_failure: Option<u64>,
    states: Vec<String>,
    /// Blocking findings settled `fixed` in this step: id and file.
    fixed: Vec<(String, Option<String>)>,
}

impl Progress {
    /// Forgets the signals (a failover resets the worktree); the retries stay.
    pub(crate) fn restart(&mut self) {
        let retries = self.retries;
        *self = Self {
            retries,
            ..Self::default()
        };
    }

    /// Records a failed verification; why when it equals the previous one.
    pub(crate) fn failed(&mut self, outcome: &Outcome, worktree: &Path) -> Option<String> {
        let print = fingerprint(outcome, worktree);
        let again = self.last_failure == Some(print);
        self.last_failure = Some(print);
        again.then(|| {
            format!(
                "the same verification failure twice in a row ({})",
                outcome.summary()
            )
        })
    }

    /// A passing verification ends a run of failures.
    pub(crate) const fn passed(&mut self) {
        self.last_failure = None;
    }

    /// Records the worktree's state; why when it returns to a state before the previous
    /// one (oscillation). The same state as the previous one is no change, not a cycle.
    pub(crate) fn state(&mut self, tree: String) -> Option<String> {
        if self.states.last() == Some(&tree) {
            return None;
        }
        let earlier = self.states.len().saturating_sub(1);
        let cycle = self.states[..earlier].contains(&tree);
        self.states.push(tree);
        cycle.then(|| "the change returned to an earlier state (oscillation)".to_owned())
    }

    /// Records blocking findings settled `fixed`.
    pub(crate) fn fixed<'f>(&mut self, findings: impl IntoIterator<Item = &'f Finding>) {
        self.fixed
            .extend(findings.into_iter().map(|f| (f.id.clone(), f.file.clone())));
    }

    /// The findings of `found` that reopen one fixed earlier in the step (same id and
    /// file).
    pub(crate) fn reopened<'f>(&self, found: &'f [Finding]) -> Vec<&'f Finding> {
        found
            .iter()
            .filter(|f| f.severity.blocking())
            .filter(|f| {
                self.fixed
                    .iter()
                    .any(|(id, file)| *id == f.id && *file == f.file)
            })
            .collect()
    }
}

/// A verification failure's identity: the exit status and the output normalized so
/// timings, counters, addresses and the worktree's path do not tell two runs apart.
#[must_use]
pub fn fingerprint(outcome: &Outcome, worktree: &Path) -> u64 {
    let status = match (outcome.timed_out, outcome.exit_code) {
        (true, _) => "timeout".to_owned(),
        (false, Some(code)) => format!("exit {code}"),
        (false, None) => "signal".to_owned(),
    };
    let text = normalize(&outcome.output, worktree);
    fnv1a(format!("{status}\n{text}").as_bytes())
}

/// The output with ANSI escapes removed, the worktree path as `<worktree>`, digit runs as
/// `0` and whitespace collapsed.
fn normalize(output: &str, worktree: &Path) -> String {
    let path = worktree.to_string_lossy();
    let replaced = if path.is_empty() {
        output.to_owned()
    } else {
        output.replace(path.as_ref(), "<worktree>")
    };
    let mut out = String::with_capacity(replaced.len());
    let mut chars = replaced.chars().peekable();
    let mut space = false;
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters… final byte in @–~.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_ascii_digit() {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            out.push('0');
            space = false;
        } else if c.is_whitespace() {
            if !space && !out.is_empty() {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim_end().to_owned()
}

/// FNV-1a, 64 bit: stable across runs and builds, unlike the std hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;

    use super::{Progress, fingerprint};
    use crate::review;
    use crate::verify::Outcome;

    fn failed(output: &str, code: i32) -> Outcome {
        Outcome {
            passed: false,
            exit_code: Some(code),
            timed_out: false,
            output: output.into(),
            truncated: false,
            duration: Duration::from_millis(1234),
        }
    }

    #[test]
    fn fingerprint_ignores_noise() {
        let wt = Path::new("/home/u/.openchef/worktrees/t1");
        let a = failed(
            "\u{1b}[31mFAIL\u{1b}[0m /home/u/.openchef/worktrees/t1/src/a.rs:12\n  took 0.31s\n",
            101,
        );
        let b = failed("FAIL <worktree>/src/a.rs:40   took 2.07s", 101);
        assert_eq!(fingerprint(&a, wt), fingerprint(&b, wt));
        assert_ne!(
            fingerprint(&a, wt),
            fingerprint(&failed("FAIL <worktree>/src/b.rs:40 took 2.07s", 101), wt),
            "another file is another failure"
        );
        assert_ne!(
            fingerprint(&a, wt),
            fingerprint(&failed("FAIL <worktree>/src/a.rs:40 took 2.07s", 1), wt),
            "another exit status"
        );
    }

    #[test]
    fn same_failure_twice_in_a_row() {
        let wt = Path::new("/wt");
        let mut p = Progress::default();
        assert!(p.failed(&failed("missing x", 1), wt).is_none());
        assert!(p.failed(&failed("missing y", 1), wt).is_none());
        assert!(p.failed(&failed("missing y", 1), wt).is_some());
        p.passed();
        assert!(
            p.failed(&failed("missing y", 1), wt).is_none(),
            "a pass in between breaks the run"
        );
        p.retries = 2;
        p.restart();
        assert!(p.retries == 2 && p.failed(&failed("missing y", 1), wt).is_none());
    }

    #[test]
    fn oscillation_is_a_return_to_an_earlier_state() {
        let mut p = Progress::default();
        assert!(p.state("A".into()).is_none());
        assert!(p.state("A".into()).is_none(), "no change is not a cycle");
        assert!(p.state("B".into()).is_none());
        assert!(p.state("A".into()).is_some(), "A → B → A");
        let mut r = Progress::default();
        for _ in 0..3 {
            assert!(r.state("S".into()).is_none(), "no change, however often");
        }
        let mut q = Progress::default();
        for s in ["A", "B", "C"] {
            assert!(q.state(s.into()).is_none());
        }
        assert!(q.state("B".into()).is_some(), "A → B → C → B");
    }

    #[test]
    fn reopened_findings_match_id_and_file() {
        let doc = json!({"findings": [
            {"id": "F1", "severity": "blocker", "file": "a.rs", "claim": "c", "evidence": "e", "fix": "f"},
            {"id": "F2", "severity": "major", "file": "b.rs", "claim": "c", "evidence": "e", "fix": "f"},
            {"id": "F3", "severity": "nit", "file": "a.rs", "claim": "c", "evidence": "e", "fix": "f"}
        ]});
        let found = review::parse(Some(&doc), "").unwrap().findings;
        let mut p = Progress::default();
        p.fixed(&found[..1]);
        let mut moved = found.clone();
        moved[0].file = Some("other.rs".into());
        assert_eq!(p.reopened(&found).len(), 1);
        assert!(
            p.reopened(&moved).is_empty(),
            "another file is another finding"
        );
    }
}

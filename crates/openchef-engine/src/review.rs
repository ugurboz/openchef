//! Step review (§7.5 phase 4, D-038): the findings document, its JSON Schema, strict
//! validation with the "located or it is a nit" rule, the reviewer's and the fix turn's
//! prompts, and the implementer's FIXED / REBUT answer.

use std::collections::BTreeSet;

use openchef_core::Step;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest diff shown to a reviewer, in characters.
pub const MAX_DIFF: usize = 60_000;
/// The most findings one review may report.
const MAX_FINDINGS: usize = 30;

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Breaks the step's goal or the repository.
    Blocker,
    /// A real defect that should be fixed now.
    Major,
    /// Style or a suggestion; never costs a turn.
    Nit,
}

impl Severity {
    /// `blocker`, `major` or `nit`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Major => "major",
            Self::Nit => "nit",
        }
    }

    /// Whether the finding costs a fix turn.
    #[must_use]
    pub const fn blocking(self) -> bool {
        matches!(self, Self::Blocker | Self::Major)
    }
}

/// One finding of a reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// Unique within the review (`F1`, `F2`, …).
    pub id: String,
    /// How bad it is.
    pub severity: Severity,
    /// The file, relative to the repository root.
    #[serde(default)]
    pub file: Option<String>,
    /// The line, when one applies.
    #[serde(default)]
    pub line: Option<u32>,
    /// A named contract from the plan or the step, when the defect is not in one place.
    #[serde(default)]
    pub contract: Option<String>,
    /// What is wrong.
    pub claim: String,
    /// Why the reviewer believes it: the code, a failing input, the rule broken.
    pub evidence: String,
    /// The change that would fix it.
    pub fix: String,
}

/// A reviewer's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// The findings; empty when the diff is fine.
    pub findings: Vec<Finding>,
}

/// The JSON Schema of a reviewer's structured output.
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["findings"],
        "properties": {
            "findings": {
                "type": "array",
                "maxItems": MAX_FINDINGS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "severity", "file", "line", "contract", "claim", "evidence", "fix"],
                    "properties": {
                        "id": { "type": "string" },
                        "severity": { "enum": ["blocker", "major", "nit"] },
                        "file": { "type": ["string", "null"] },
                        "line": { "type": ["integer", "null"], "minimum": 1 },
                        "contract": { "type": ["string", "null"] },
                        "claim": { "type": "string" },
                        "evidence": { "type": "string" },
                        "fix": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// The diff, cut to [`MAX_DIFF`] characters with a note when it is longer.
#[must_use]
pub fn bounded_diff(diff: &str) -> String {
    if diff.chars().count() <= MAX_DIFF {
        return diff.to_owned();
    }
    let cut: String = diff.chars().take(MAX_DIFF).collect();
    format!("{cut}\n… (the diff is longer; read the remaining files in the worktree)")
}

/// The reviewer's instructions for one round.
#[must_use]
pub fn reviewer_prompt(step: &Step, diff: &str, open: &[Finding], round: u32) -> String {
    let verify = step.verify.as_deref().unwrap_or("none");
    let earlier = if open.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = open
            .iter()
            .map(|f| format!("- {} [{}] {}", f.id, f.severity.as_str(), f.claim))
            .collect();
        format!(
            "\nFindings of the previous round that the implementer was asked to fix:\n{}\n\
             Report again only those still present, with the same ids.\n",
            list.join("\n")
        )
    };
    format!(
        "You are the reviewer (round {round}). Another agent implemented the step below and \
         its verification passed. Read the diff and the repository (you cannot change \
         anything) and report defects.\n\
         \n\
         Rules:\n\
         - blocker: the step's goal is not met or something breaks; major: a real defect to \
           fix now; nit: style or a suggestion.\n\
         - A blocker or major must name its `file` (and `line` when one applies) or a \
           `contract` of the step; otherwise it counts as a nit.\n\
         - `evidence` quotes the code or gives the failing input; `fix` says what to change.\n\
         - Do not report what verification already proves, and do not invent requirements \
           the step does not state.\n\
         - No findings is a valid answer.\n\
         {earlier}\n\
         Answer with the JSON findings only.\n\
         \n\
         Step {}: {}\n{}\n\nVerification: `{verify}`\n\nDiff:\n{diff}\n",
        step.index, step.title, step.prompt
    )
}

/// Reads a reviewer's answer and validates it; located-or-nit is applied here, so the
/// returned findings are the ones OpenChef acts on.
///
/// # Errors
///
/// Every problem found, for one repair turn.
pub fn parse(structured: Option<&Value>, text: &str) -> Result<Review, Vec<String>> {
    let mut review: Review = if let Some(v) = structured {
        serde_json::from_value(v.clone()).map_err(|e| vec![format!("schema: {e}")])?
    } else {
        let body = crate::plan::json_body(text)
            .ok_or_else(|| vec!["the answer contains no JSON object".to_owned()])?;
        serde_json::from_str(body).map_err(|e| vec![format!("schema: {e}")])?
    };
    let mut problems = Vec::new();
    if review.findings.len() > MAX_FINDINGS {
        problems.push(format!("more than {MAX_FINDINGS} findings"));
    }
    let mut ids = BTreeSet::new();
    for f in &mut review.findings {
        if f.id.trim().is_empty() || !ids.insert(f.id.clone()) {
            problems.push(format!("finding id {:?} is empty or repeated", f.id));
        }
        if f.claim.trim().is_empty() {
            problems.push(format!("finding {} has no claim", f.id));
        }
        let located = f.file.as_deref().is_some_and(|p| !p.trim().is_empty())
            || f.contract.as_deref().is_some_and(|c| !c.trim().is_empty());
        if f.severity.blocking() && !located {
            f.severity = Severity::Nit; // §7.5: only located defects cost a turn
        }
    }
    if problems.is_empty() {
        Ok(review)
    } else {
        Err(problems)
    }
}

/// The repair turn's instructions after an invalid answer.
#[must_use]
pub fn repair_prompt(problems: &[String]) -> String {
    format!(
        "Your findings are not valid:\n- {}\n\nAnswer again with the corrected JSON only.",
        problems.join("\n- ")
    )
}

/// The fix turn's instructions: the blockers and majors to address.
#[must_use]
pub fn fix_prompt(reviewer: &str, findings: &[Finding]) -> String {
    let list: Vec<String> = findings
        .iter()
        .map(|f| {
            let place = match (&f.file, f.line, &f.contract) {
                (Some(file), Some(line), _) => format!("{file}:{line}"),
                (Some(file), None, _) => file.clone(),
                (None, _, Some(c)) => format!("contract: {c}"),
                (None, _, None) => "unlocated".to_owned(),
            };
            format!(
                "{} [{}] {place}\n  claim: {}\n  evidence: {}\n  suggested fix: {}",
                f.id,
                f.severity.as_str(),
                f.claim,
                f.evidence,
                f.fix
            )
        })
        .collect();
    format!(
        "A reviewer ({reviewer}, another provider) found problems in your change:\n\n{}\n\n\
         Fix each one, or rebut it if it is wrong. End your answer with one line per finding: \
         `FIXED <id>` or `REBUT <id>: <evidence that the finding is wrong>`. Verification \
         runs again after your answer.",
        list.join("\n\n")
    )
}

/// The ids the implementer reports fixed (`FIXED <id>` lines).
#[must_use]
pub fn fixed_claims(answer: &str) -> Vec<String> {
    answer
        .lines()
        .filter_map(|l| {
            let id = l
                .trim()
                .trim_start_matches(['-', '*', ' '])
                .strip_prefix("FIXED ")?;
            let id = id.trim().trim_matches('`').to_owned();
            (!id.is_empty()).then_some(id)
        })
        .collect()
}

/// The ids the implementer rebutted, with their evidence (`REBUT <id>: …` lines).
#[must_use]
pub fn rebuttals(answer: &str) -> Vec<(String, String)> {
    answer
        .lines()
        .filter_map(|l| {
            let rest = l
                .trim()
                .trim_start_matches(['-', '*', ' '])
                .strip_prefix("REBUT ")?;
            let (id, why) = rest.split_once(':').unwrap_or((rest, ""));
            let id = id.trim().trim_matches('`').to_owned();
            (!id.is_empty()).then(|| (id, why.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Severity, bounded_diff, fixed_claims, parse, rebuttals, schema};

    fn finding(id: &str, severity: &str, file: Option<&str>) -> serde_json::Value {
        json!({"id": id, "severity": severity, "file": file, "line": null, "contract": null,
               "claim": "off by one", "evidence": "i <= n", "fix": "use <"})
    }

    #[test]
    fn unlocated_blockers_become_nits() {
        let doc = json!({"findings": [
            finding("F1", "blocker", Some("src/a.rs")),
            finding("F2", "major", None),
            finding("F3", "nit", None),
        ]});
        let r = parse(Some(&doc), "").unwrap_or_else(|e| panic!("{e:?}"));
        let sev: Vec<Severity> = r.findings.iter().map(|f| f.severity).collect();
        assert_eq!(sev, [Severity::Blocker, Severity::Nit, Severity::Nit]);
        let mut contract = finding("F4", "major", None);
        contract["contract"] = json!("the API stays backwards compatible");
        let r =
            parse(Some(&json!({"findings": [contract]})), "").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            r.findings[0].severity,
            Severity::Major,
            "a named contract locates it"
        );
    }

    #[test]
    fn invalid_reviews_list_problems() {
        let doc = json!({"findings": [finding("F1", "nit", None), finding("F1", "nit", None)]});
        assert!(parse(Some(&doc), "").unwrap_err()[0].contains("repeated"));
        assert!(parse(Some(&json!({"findings": [], "summary": "x"})), "").is_err());
        assert!(parse(None, "looks good to me").is_err());
        assert_eq!(
            parse(None, "```json\n{\"findings\": []}\n```").map(|r| r.findings.len()),
            Ok(0)
        );
    }

    #[test]
    fn rebuttal_lines() {
        let answer =
            "Done.\nFIXED F1\n- REBUT F2: the bound is exclusive already (line 4)\nREBUT `F3`";
        assert_eq!(fixed_claims(answer), ["F1"]);
        assert_eq!(
            rebuttals(answer),
            [
                (
                    "F2".to_owned(),
                    "the bound is exclusive already (line 4)".to_owned()
                ),
                ("F3".to_owned(), String::new())
            ]
        );
    }

    #[test]
    fn bounds() {
        let long = "x".repeat(super::MAX_DIFF + 5);
        assert!(bounded_diff(&long).ends_with("worktree)"));
        assert_eq!(
            schema()["properties"]["findings"]["items"]["properties"]["severity"]["enum"][2],
            "nit"
        );
    }
}

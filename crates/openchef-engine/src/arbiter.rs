//! The arbiter's documents (§7.5 phase 4.4, D-039): one batched turn settles the
//! findings an implementer rebutted, with a verdict per finding.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::review::Finding;

/// What the arbiter decides about one disputed finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ruling {
    /// The finding stands: the implementer gets one last fix turn.
    Uphold,
    /// The rebuttal is right: the finding is dropped.
    Dismiss,
}

/// The arbiter's verdict on one finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// The finding's id.
    pub id: String,
    /// The ruling.
    pub verdict: Ruling,
    /// Why, for the report.
    pub reason: String,
}

/// The arbiter's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdicts {
    /// One per disputed finding.
    pub verdicts: Vec<Verdict>,
}

/// A disputed finding with the implementer's rebuttal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispute {
    /// The reviewer's finding.
    pub finding: Finding,
    /// The reviewer's agent name.
    pub reviewer: String,
    /// The implementer's evidence that it is wrong.
    pub rebuttal: String,
}

/// The JSON Schema of the arbiter's structured output.
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdicts"],
        "properties": {
            "verdicts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "verdict", "reason"],
                    "properties": {
                        "id": {"type": "string"},
                        "verdict": {"enum": ["uphold", "dismiss"]},
                        "reason": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Reads the arbiter's answer: exactly one verdict for each of `ids`, with a reason.
///
/// # Errors
///
/// Every problem found, for one repair turn.
pub fn parse(
    structured: Option<&Value>,
    text: &str,
    ids: &[String],
) -> Result<Verdicts, Vec<String>> {
    let v: Verdicts = if let Some(v) = structured {
        serde_json::from_value(v.clone()).map_err(|e| vec![format!("schema: {e}")])?
    } else {
        let body = crate::plan::json_body(text)
            .ok_or_else(|| vec!["the answer contains no JSON object".to_owned()])?;
        serde_json::from_str(body).map_err(|e| vec![format!("schema: {e}")])?
    };
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for verdict in &v.verdicts {
        if !ids.contains(&verdict.id) {
            problems.push(format!("verdict for unknown finding {:?}", verdict.id));
        } else if !seen.insert(verdict.id.clone()) {
            problems.push(format!("two verdicts for {}", verdict.id));
        }
        if verdict.reason.trim().is_empty() {
            problems.push(format!("the verdict on {} gives no reason", verdict.id));
        }
    }
    for id in ids.iter().filter(|id| !seen.contains(*id)) {
        problems.push(format!("no verdict for {id}"));
    }
    if problems.is_empty() {
        Ok(v)
    } else {
        Err(problems)
    }
}

/// The arbiter's instructions: every dispute of the step at once.
#[must_use]
pub fn prompt(step_title: &str, diff: &str, disputes: &[Dispute]) -> String {
    let list: Vec<String> = disputes
        .iter()
        .map(|d| {
            let f = &d.finding;
            let place = match (&f.file, f.line, &f.contract) {
                (Some(file), Some(line), _) => format!("{file}:{line}"),
                (Some(file), None, _) => file.clone(),
                (None, _, Some(c)) => format!("contract: {c}"),
                (None, _, None) => "unlocated".to_owned(),
            };
            format!(
                "{} [{}] {place}\n  reviewer ({}): {}\n  reviewer's evidence: {}\n  \
                 implementer's rebuttal: {}",
                f.id,
                f.severity.as_str(),
                d.reviewer,
                f.claim,
                f.evidence,
                d.rebuttal
            )
        })
        .collect();
    format!(
        "You are the arbiter. A reviewer reported the findings below on a step's change; \
         the implementer rebutted each one. Read the diff and the repository (you cannot \
         change anything) and rule on every finding: `uphold` when the defect is real, \
         `dismiss` when the rebuttal is right. Judge the evidence, not who argues better. \
         Answer with the JSON verdicts only, one per finding, each with its reason.\n\
         \n\
         Step: {step_title}\n\nDisputed findings:\n\n{}\n\nDiff:\n{diff}\n",
        list.join("\n\n")
    )
}

/// The repair turn's instructions after an invalid answer.
#[must_use]
pub fn repair_prompt(problems: &[String]) -> String {
    format!(
        "Your verdicts are not valid:\n- {}\n\nAnswer again with the corrected JSON only.",
        problems.join("\n- ")
    )
}

/// The implementer's last fix turn for upheld findings: no rebuttal any more.
#[must_use]
pub fn upheld_prompt(arbiter: &str, upheld: &[(Finding, String)]) -> String {
    let list: Vec<String> = upheld
        .iter()
        .map(|(f, reason)| {
            format!(
                "{} [{}] {}\n  arbiter: {reason}",
                f.id,
                f.severity.as_str(),
                f.claim
            )
        })
        .collect();
    format!(
        "An arbiter ({arbiter}) upheld these findings over your rebuttal:\n\n{}\n\n\
         Fix them now; they can no longer be rebutted. End your answer with one line per \
         finding you fixed: `FIXED <id>`. Verification runs again after your answer.",
        list.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Ruling, parse};

    #[test]
    fn one_reasoned_verdict_per_finding() {
        let ids = vec!["F1".to_owned(), "F2".to_owned()];
        let ok = json!({"verdicts": [
            {"id": "F1", "verdict": "uphold", "reason": "the loop is off by one"},
            {"id": "F2", "verdict": "dismiss", "reason": "the test covers it"}
        ]});
        let v = parse(Some(&ok), "", &ids).unwrap();
        assert_eq!(v.verdicts[0].verdict, Ruling::Uphold);

        let bad = json!({"verdicts": [
            {"id": "F1", "verdict": "uphold", "reason": ""},
            {"id": "F1", "verdict": "dismiss", "reason": "x"},
            {"id": "F9", "verdict": "dismiss", "reason": "x"}
        ]});
        let problems = parse(Some(&bad), "", &ids).unwrap_err();
        for want in [
            "gives no reason",
            "two verdicts for F1",
            "unknown finding \"F9\"",
            "no verdict for F2",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(want)),
                "{want}: {problems:?}"
            );
        }
        assert!(parse(None, "I uphold everything", &ids).is_err());
        let wrong = json!({"verdicts": [{"id": "F1", "verdict": "maybe", "reason": "x"}]});
        assert!(
            parse(Some(&wrong), "", &ids).is_err(),
            "a closed set of rulings"
        );
    }
}

//! The plan council's documents (§7.5 phase 2, D-039): the importance it runs at, the
//! critic's and the revision turn's prompts, and the rule that a plan finding is located
//! by a repository file or a step of the plan. The depth table is `[debate]` (D-040).

use openchef_core::Importance;

use crate::plan::Plan;
use crate::review::{Finding, Review, Severity};

/// The importance the council uses: the task's floor, raised by the highest step
/// importance the planner proposed.
#[must_use]
pub fn importance(floor: Importance, plan: &Plan) -> Importance {
    plan.steps
        .iter()
        .map(|s| s.importance)
        .fold(floor, Importance::at_least)
}

/// The step a `contract` names (`step 2`, `Step 2`), when it is one of the plan's.
fn step_of(contract: &str, steps: usize) -> Option<usize> {
    let n: usize = contract
        .trim()
        .strip_prefix("step ")
        .or_else(|| contract.trim().strip_prefix("Step "))?
        .trim()
        .parse()
        .ok()?;
    (1..=steps).contains(&n).then_some(n)
}

/// Applies located-or-nit to a plan: a blocker or major must name a file or a step of
/// the plan (`contract: "step N"`); anything else is a nit (§7.5).
#[must_use]
pub fn locate(mut review: Review, plan: &Plan) -> Review {
    for f in &mut review.findings {
        let in_file = f.file.as_deref().is_some_and(|p| !p.trim().is_empty());
        let at_step = f
            .contract
            .as_deref()
            .is_some_and(|c| step_of(c, plan.steps.len()).is_some());
        if f.severity.blocking() && !in_file && !at_step {
            f.severity = Severity::Nit;
        }
    }
    review
}

/// The plan as the critics read it.
fn plan_text(plan: &Plan) -> String {
    serde_json::to_string_pretty(plan).unwrap_or_else(|_| format!("{plan:?}"))
}

/// A critic's instructions for one round; `own` lists its blocking findings of the
/// previous round, which the planner was asked to address.
#[must_use]
pub fn critic_prompt(goal: &str, plan: &Plan, own: &[Finding], round: u32) -> String {
    let earlier = if own.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = own
            .iter()
            .map(|f| format!("- {} [{}] {}", f.id, f.severity.as_str(), f.claim))
            .collect();
        format!(
            "\nYour findings of the previous round, which the planner was asked to address:\n{}\n\
             The plan below is the revision. Report again only those still present, with the \
             same ids.\n",
            list.join("\n")
        )
    };
    format!(
        "You are a plan critic (round {round}). Another agent wrote the plan below for the \
         goal. Read it and the repository (you cannot change anything) and report what would \
         make the work fail or go wrong before anyone starts it.\n\
         \n\
         Rules:\n\
         - blocker: following the plan cannot meet the goal or breaks something; major: a \
           real gap to fix in the plan now (a missing step, a wrong order, a verification \
           that proves nothing); nit: wording or a suggestion.\n\
         - A blocker or major names a `file` of the repository or a step of the plan as \
           `contract` (\"step 2\"); otherwise it counts as a nit.\n\
         - `evidence` quotes the plan or the code; `fix` says how the plan should change.\n\
         - Do not ask for more than the goal needs. No findings is a valid answer.\n\
         {earlier}\n\
         Answer with the JSON findings only.\n\
         \n\
         Goal:\n{goal}\n\nPlan:\n{}\n",
        plan_text(plan)
    )
}

/// The planner's revision turn: the critics' blocking findings, then the whole plan
/// again.
#[must_use]
pub fn revision_prompt(findings: &[(String, Finding)]) -> String {
    let list: Vec<String> = findings
        .iter()
        .map(|(critic, f)| {
            let place = match (&f.file, &f.contract) {
                (Some(file), _) => file.clone(),
                (None, Some(c)) => c.clone(),
                (None, None) => "unlocated".to_owned(),
            };
            format!(
                "{} [{}] {place} (critic {critic})\n  claim: {}\n  evidence: {}\n  suggested \
                 change: {}",
                f.id,
                f.severity.as_str(),
                f.claim,
                f.evidence,
                f.fix
            )
        })
        .collect();
    format!(
        "Critics of other providers read your plan and found problems:\n\n{}\n\n\
         Revise the plan where they are right; you may keep it where they are wrong (they \
         read the revision and decide). Answer with the whole revised plan as JSON, in the \
         same schema, and nothing else.",
        list.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use openchef_core::Importance;
    use serde_json::json;

    use super::{critic_prompt, importance, locate, revision_prompt};
    use crate::plan::Plan;
    use crate::review::{self, Severity};

    fn plan(importances: &[&str]) -> Plan {
        let steps: Vec<serde_json::Value> = importances
            .iter()
            .map(|i| {
                json!({"title": "s", "prompt": "p", "files": [], "verify": null,
                       "size": "small", "importance": i, "reason": "r"})
            })
            .collect();
        serde_json::from_value(json!({"title": "T", "steps": steps})).unwrap()
    }

    #[test]
    fn importance_is_raised_by_the_plan() {
        let p = plan(&["normal", "high"]);
        assert_eq!(importance(Importance::Low, &p), Importance::High);
        assert_eq!(importance(Importance::Critical, &p), Importance::Critical);
    }

    #[test]
    fn plan_findings_are_located_by_file_or_step() {
        let p = plan(&["normal", "normal"]);
        let f = |id: &str, file: Option<&str>, contract: Option<&str>| {
            json!({"id": id, "severity": "blocker", "file": file, "contract": contract,
                   "claim": "c", "evidence": "e", "fix": "f"})
        };
        let doc = json!({"findings": [
            f("F1", Some("src/a.rs"), None),
            f("F2", None, Some("step 2")),
            f("F3", None, Some("step 3")),
            f("F4", None, Some("the API")),
        ]});
        let r = locate(review::parse(Some(&doc), "").unwrap(), &p);
        let sev: Vec<Severity> = r.findings.iter().map(|f| f.severity).collect();
        assert_eq!(
            sev,
            [
                Severity::Blocker,
                Severity::Blocker,
                Severity::Nit,
                Severity::Nit
            ],
            "a step beyond the plan or a free-text contract locates nothing"
        );
    }

    #[test]
    fn prompts_carry_the_plan_and_findings() {
        let p = plan(&["high"]);
        let first = critic_prompt("ship it", &p, &[], 1);
        assert!(
            first.contains("round 1") && first.contains("ship it") && first.contains("\"high\"")
        );
        let doc = json!({"findings": [{"id": "F1", "severity": "major", "contract": "step 1",
                                       "claim": "no test", "evidence": "verify is null", "fix": "add one"}]});
        let found = review::parse(Some(&doc), "").unwrap().findings;
        let again = critic_prompt("ship it", &p, &found, 2);
        assert!(again.contains("F1 [major] no test") && again.contains("same ids"));
        let rev = revision_prompt(&[("gpt".into(), found[0].clone())]);
        assert!(rev.contains("step 1 (critic gpt)") && rev.contains("whole revised plan"));
    }
}

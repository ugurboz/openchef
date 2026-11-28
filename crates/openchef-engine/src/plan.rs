//! The plan document (§7.5 phase 1, D-037): what a planner returns, its JSON Schema
//! for the CLIs' structured output, and the strict validation OpenChef applies in every
//! case (a CLI's own schema check is never trusted alone).

use std::path::{Component, Path};

use openchef_core::{Importance, MAX_STEPS, Step, StepStatus, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The longest step prompt accepted, in characters.
const MAX_PROMPT: usize = 8000;
/// The longest title accepted, in characters.
const MAX_TITLE: usize = 120;

/// How big a step is, as the planner estimates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    /// Minutes of agent work.
    Small,
    /// Tens of minutes.
    Medium,
    /// Long; the planner should rather split it.
    Large,
}

/// One planned step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// A short imperative title.
    pub title: String,
    /// Self-contained instructions for the implementer.
    pub prompt: String,
    /// Files the step will touch, relative to the repository root.
    #[serde(default)]
    pub files: Vec<String>,
    /// The shell command that proves the step works; `null` when none applies.
    #[serde(default)]
    pub verify: Option<String>,
    /// The planner's estimate.
    pub size: Size,
    /// How much a mistake costs.
    pub importance: Importance,
    /// Why this step exists.
    pub reason: String,
}

/// A planner's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The task's title.
    pub title: String,
    /// The steps, in order.
    pub steps: Vec<PlanStep>,
}

/// The JSON Schema the planner's structured output must match.
#[must_use]
pub fn schema() -> Value {
    let importance: Vec<&str> = Importance::ALL.iter().map(|i| i.as_str()).collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "steps"],
        "properties": {
            "title": { "type": "string", "maxLength": MAX_TITLE },
            "steps": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_STEPS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "prompt", "files", "verify", "size", "importance", "reason"],
                    "properties": {
                        "title": { "type": "string", "maxLength": MAX_TITLE },
                        "prompt": { "type": "string", "maxLength": MAX_PROMPT },
                        "files": { "type": "array", "items": { "type": "string" } },
                        "verify": { "type": ["string", "null"] },
                        "size": { "enum": ["small", "medium", "large"] },
                        "importance": { "enum": importance },
                        "reason": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// The planner's instructions for a goal.
#[must_use]
pub fn planner_prompt(goal: &str, verify: Option<&str>) -> String {
    let verify = verify.map_or_else(String::new, |v| {
        format!("\nThe user's verification command for the whole task is `{v}`; give each step the narrowest command that proves it, or this one.\n")
    });
    format!(
        "You are the planner. Read the repository (you cannot change anything) and split the goal \
         below into 1 to {MAX_STEPS} steps that other agents will implement one after another, \
         each in a fresh session that sees only its own step's prompt.\n\
         \n\
         Rules:\n\
         - Each step must be independently verifiable and leave the repository working.\n\
         - Prompts are self-contained: name the files, the behaviour and the constraints.\n\
         - `files` lists paths relative to the repository root that the step will change.\n\
         - `verify` is a shell command run in the repository root that fails when the step is \
           wrong (tests, a build, a type check); `null` only when nothing can check it.\n\
         - `importance`: low (cheap to get wrong), normal, high (needs the strongest agents), \
           critical (security, data loss, money).\n\
         - Prefer fewer, smaller steps; never more than {MAX_STEPS}.\n\
         {verify}\n\
         Answer with the JSON plan only.\n\
         \n\
         Goal:\n{goal}\n"
    )
}

/// The repair turn's instructions after an invalid answer.
#[must_use]
pub fn repair_prompt(problems: &[String]) -> String {
    format!(
        "Your plan is not valid:\n- {}\n\nAnswer again with the corrected JSON plan only, \
         matching the schema exactly.",
        problems.join("\n- ")
    )
}

/// Reads a planner's answer: the CLI's structured result when there is one, otherwise
/// the final text (a fenced JSON block is accepted), then validates it.
///
/// # Errors
///
/// Every problem found, so a repair turn can fix them all at once.
pub fn parse(structured: Option<&Value>, text: &str) -> Result<Plan, Vec<String>> {
    let plan: Plan = if let Some(v) = structured {
        serde_json::from_value(v.clone()).map_err(|e| vec![format!("schema: {e}")])?
    } else {
        let body =
            json_body(text).ok_or_else(|| vec!["the answer contains no JSON object".to_owned()])?;
        serde_json::from_str(body).map_err(|e| vec![format!("schema: {e}")])?
    };
    let problems = problems(&plan);
    if problems.is_empty() {
        Ok(plan)
    } else {
        Err(problems)
    }
}

/// The JSON object in `text`: a fenced block's body, or the outermost braces.
fn json_body(text: &str) -> Option<&str> {
    if let Some(start) = text.find("```") {
        let rest = &text[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            return Some(rest[..end].trim());
        }
    }
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    (start < end).then(|| &text[start..=end])
}

/// Every rule a plan breaks.
#[must_use]
pub fn problems(plan: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |ok: bool, problem: String| {
        if !ok {
            out.push(problem);
        }
    };
    check(
        !plan.title.trim().is_empty(),
        "the plan has no title".into(),
    );
    check(
        plan.title.chars().count() <= MAX_TITLE,
        format!("the title is longer than {MAX_TITLE} characters"),
    );
    check(
        (1..=MAX_STEPS).contains(&plan.steps.len()),
        format!(
            "the plan has {} steps; want 1 to {MAX_STEPS}",
            plan.steps.len()
        ),
    );
    for (i, s) in plan.steps.iter().enumerate() {
        let n = i + 1;
        check(!s.title.trim().is_empty(), format!("step {n} has no title"));
        check(
            s.title.chars().count() <= MAX_TITLE,
            format!("step {n}: the title is longer than {MAX_TITLE} characters"),
        );
        check(
            !s.prompt.trim().is_empty(),
            format!("step {n} has no prompt"),
        );
        check(
            s.prompt.chars().count() <= MAX_PROMPT,
            format!("step {n}: the prompt is longer than {MAX_PROMPT} characters"),
        );
        check(
            s.verify.as_deref().is_none_or(|v| !v.trim().is_empty()),
            format!("step {n}: verify is empty; use null for none"),
        );
        for f in &s.files {
            check(
                inside(f),
                format!("step {n}: file {f:?} is not a relative path inside the repository"),
            );
        }
    }
    out
}

/// A relative path that stays inside the repository.
fn inside(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

impl Plan {
    /// The plan as fresh pending steps of `task`.
    #[must_use]
    pub fn steps(&self, task: &TaskId, now: OffsetDateTime) -> Vec<Step> {
        (1_u32..)
            .zip(&self.steps)
            .map(|(index, s)| Step {
                task_id: task.clone(),
                index,
                title: s.title.trim().to_owned(),
                prompt: s.prompt.clone(),
                files: s.files.clone(),
                verify: s.verify.clone(),
                importance: s.importance,
                status: StepStatus::Pending,
                retries: 0,
                verified_commit: None,
                updated_at: now,
            })
            .collect()
    }

    /// A stored plan as a document again, for `plan edit` (size and reason are not
    /// stored; they read `medium` and empty).
    #[must_use]
    pub fn from_steps(title: &str, steps: &[Step]) -> Self {
        Self {
            title: title.to_owned(),
            steps: steps
                .iter()
                .map(|s| PlanStep {
                    title: s.title.clone(),
                    prompt: s.prompt.clone(),
                    files: s.files.clone(),
                    verify: s.verify.clone(),
                    size: Size::Medium,
                    importance: s.importance,
                    reason: String::new(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse, problems, schema};

    fn step() -> serde_json::Value {
        json!({"title": "Add the parser", "prompt": "Write src/parse.rs", "files": ["src/parse.rs"],
               "verify": "cargo test", "size": "small", "importance": "normal", "reason": "core"})
    }

    #[test]
    fn valid_plans_parse() {
        let doc = json!({"title": "Parser", "steps": [step()]});
        let plan = parse(Some(&doc), "").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(plan.steps.len(), 1);
        let fenced = format!("Here it is:\n```json\n{doc}\n```\nDone.");
        assert_eq!(parse(None, &fenced), Ok(plan.clone()));
        assert_eq!(parse(None, &format!("plan: {doc} ok")), Ok(plan));
    }

    #[test]
    fn invalid_plans_list_every_problem() {
        let mut bad = step();
        bad["files"] = json!(["../etc/passwd", "/abs", ""]);
        bad["verify"] = json!("  ");
        let doc = json!({"title": "", "steps": [bad]});
        let errs = parse(Some(&doc), "").unwrap_err();
        assert_eq!(errs.len(), 5, "{errs:?}");
        let eight: Vec<_> = (0..8).map(|_| step()).collect();
        let errs = parse(Some(&json!({"title": "t", "steps": eight})), "").unwrap_err();
        assert!(errs[0].contains("8 steps"), "{errs:?}");
        let extra = json!({"title": "t", "steps": [step()], "notes": "x"});
        assert!(parse(Some(&extra), "").unwrap_err()[0].contains("unknown field"));
        let mut imp = step();
        imp["importance"] = json!("urgent");
        assert!(parse(Some(&json!({"title": "t", "steps": [imp]})), "").is_err());
        assert!(parse(None, "no json here").is_err());
        let ok = parse(Some(&json!({"title": "t", "steps": [step()]})), "")
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(problems(&ok), Vec::<String>::new());
    }

    #[test]
    fn schema_matches_the_document() {
        let s = schema();
        assert_eq!(s["properties"]["steps"]["maxItems"], 7);
        assert_eq!(
            s["properties"]["steps"]["items"]["properties"]["importance"]["enum"][3],
            "critical"
        );
    }
}

//! Validates agent answers against the JSON Schema a turn asked for (§7.5, D-017),
//! ported from Go `internal/schema`. Claude and Codex enforce schemas natively;
//! runtimes without that (agy) prompt for JSON, extract it from the reply and
//! validate it here.

use jsonschema::{Retrieve, Uri, Validator};
use serde_json::Value;

use crate::Error;

/// Refuses every external reference: a turn schema must be self-contained, and
/// compiling it must never read files or the network.
struct NoRetrieve;

impl Retrieve for NoRetrieve {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("external reference {uri} is not allowed").into())
    }
}

/// A compiled JSON Schema.
pub struct Schema {
    validator: Validator,
}

impl std::fmt::Debug for Schema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schema").finish_non_exhaustive()
    }
}

impl Schema {
    /// Compiles `raw`, refusing invalid schemas and external references.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidSchema`] when `raw` is not a valid, self-contained schema.
    pub fn compile(raw: &Value) -> Result<Self, Error> {
        jsonschema::options()
            .with_retriever(NoRetrieve)
            .build(raw)
            .map(|validator| Self { validator })
            .map_err(|e| Error::InvalidSchema {
                reason: e.to_string(),
            })
    }

    /// Checks one document; the error says what is wrong, for a repair prompt.
    ///
    /// # Errors
    ///
    /// The first violation, as text.
    pub fn validate(&self, doc: &Value) -> Result<(), String> {
        self.validator.validate(doc).map_err(|e| e.to_string())
    }
}

/// The JSON value an agent answered with: the whole text, else the last ```` ```json ````
/// (or bare ```` ``` ````) fence, else the outermost `{…}` or `[…]` span. It does not
/// validate against a schema.
#[must_use]
pub fn extract(text: &str) -> Option<Value> {
    let t = text.trim();
    let container = |s: &str| {
        (s.starts_with('{') || s.starts_with('['))
            .then(|| serde_json::from_str(s).ok())
            .flatten()
    };
    if let Some(v) = container(t) {
        return Some(v);
    }
    if let Some(v) = last_fence(t).and_then(|f| serde_json::from_str(f).ok()) {
        return Some(v);
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(i), Some(j)) = (t.find(open), t.rfind(close))
            && j > i
            && let Ok(v) = serde_json::from_str(&t[i..=j])
        {
            return Some(v);
        }
    }
    None
}

fn last_fence(t: &str) -> Option<&str> {
    let end = t.rfind("```")?;
    let start = t[..end].rfind("```")?;
    let mut body = &t[start + 3..end];
    if let Some(nl) = body.find('\n')
        && !body[..nl].contains(['{', '['])
    {
        body = &body[nl + 1..]; // drop the language tag line
    }
    Some(body.trim())
}

/// Appended to a prompt to ask for an answer that satisfies `raw`.
#[must_use]
pub fn instruction(raw: &Value) -> String {
    format!(
        "\n\nAnswer with a single JSON value that satisfies this JSON Schema, and nothing else: \
         no prose, no code fence.\nJSON Schema:\n{raw}"
    )
}

/// The follow-up prompt when an answer failed validation.
#[must_use]
pub fn repair(problem: &str) -> String {
    format!(
        "Your previous answer was not valid for the required JSON Schema ({problem}). Reply again \
         with only the corrected JSON value: no prose, no code fence."
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Schema, extract, instruction, repair};

    fn plan() -> Value {
        json!({"type": "object", "properties": {"steps": {"type": "array", "items": {"type": "string"}, "minItems": 1}},
            "required": ["steps"], "additionalProperties": false})
    }

    #[test]
    fn compile_and_validate() {
        let s = Schema::compile(&plan()).unwrap();
        assert!(s.validate(&json!({"steps": ["a", "b"]})).is_ok());
        for bad in [
            json!({"steps": []}),
            json!({"steps": ["a"], "x": 1}),
            json!({}),
            json!([1]),
            json!("nope"),
        ] {
            assert!(s.validate(&bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn compile_rejects_bad_and_external_schemas() {
        for raw in [
            json!({"type": "nonsense"}),
            json!({"$ref": "https://example.com/s.json"}),
            json!({"$ref": "file:///etc/passwd"}),
        ] {
            assert!(Schema::compile(&raw).is_err(), "{raw} compiled");
        }
        assert!(
            Schema::compile(&json!({"$defs": {"s": {"type": "string"}}, "$ref": "#/$defs/s"}))
                .is_ok(),
            "local $ref"
        );
    }

    #[test]
    fn extract_cases() {
        let cases = [
            (r#"{"steps":["a"]}"#, json!({"steps": ["a"]})),
            ("  [1,2]\n", json!([1, 2])),
            (
                "Here it is:\n```json\n{\"steps\":[\"a\"]}\n```\nDone.",
                json!({"steps": ["a"]}),
            ),
            ("```\n{\"a\":1}\n```", json!({"a": 1})),
            (
                "first ```json\n{\"a\":1}\n``` then ```json\n{\"a\":2}\n```",
                json!({"a": 2}),
            ),
            (
                "The plan is {\"steps\":[\"x\"]} as requested.",
                json!({"steps": ["x"]}),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(extract(input), Some(want), "{input:?}");
        }
        for input in ["", "no json here", "{broken", r#""just a string""#] {
            assert_eq!(extract(input), None, "{input:?}");
        }
    }

    #[test]
    fn prompts() {
        let s = instruction(&plan());
        assert!(s.contains(&plan().to_string()) && s.contains("JSON Schema"));
        assert!(repair("missing steps").contains("missing steps"));
    }
}

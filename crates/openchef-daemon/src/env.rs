//! The agent CLI environment, ported from Go `internal/app/env.go`.

use openchef_config::{Billing, Pool};

/// Removed from subscription-backed sessions so a CLI cannot silently bill an API
/// account or be redirected to another provider (§13): keys and tokens, base-URL
/// overrides, and the switches that send Claude Code to Bedrock or Vertex or Gemini to
/// Vertex.
const BILLING_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// The session environment for `pool`: `from` without API keys, except the key an API
/// pool names explicitly. A missing pool keeps no key at all.
pub(crate) fn session_env(from: &[(String, String)], pool: Option<&Pool>) -> Vec<(String, String)> {
    from.iter()
        .filter(|(k, _)| {
            let up = k.to_uppercase();
            if !BILLING_VARS.contains(&up.as_str()) {
                return true;
            }
            pool.is_some_and(|p| {
                p.billing == Billing::Api
                    && p.api_key_env
                        .as_deref()
                        .is_some_and(|named| named.eq_ignore_ascii_case(k))
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use openchef_config::{Billing, EffortProfile, Pool, Provider};

    use super::session_env;

    fn pool(billing: Billing, key: Option<&str>) -> Pool {
        Pool {
            provider: Provider::Anthropic,
            billing,
            plan_tier: None,
            max_concurrent: 2,
            effort_profile: EffortProfile::Auto,
            api_key_env: key.map(str::to_owned),
            daily_budget_usd: None,
            monthly_budget_usd: None,
        }
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn session_env_drops_billing_keys() {
        let from = kv(&[
            ("PATH", "/bin"),
            ("ANTHROPIC_API_KEY", "sk"),
            ("openai_api_key", "sk2"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("HOME", "/h"),
        ]);
        let sub = session_env(&from, Some(&pool(Billing::Subscription, None)));
        assert_eq!(sub, kv(&[("PATH", "/bin"), ("HOME", "/h")]));
        let api = session_env(&from, Some(&pool(Billing::Api, Some("ANTHROPIC_API_KEY"))));
        assert_eq!(
            api,
            kv(&[
                ("PATH", "/bin"),
                ("ANTHROPIC_API_KEY", "sk"),
                ("HOME", "/h")
            ])
        );
        assert_eq!(
            session_env(&from, None),
            kv(&[("PATH", "/bin"), ("HOME", "/h")])
        );
    }
}

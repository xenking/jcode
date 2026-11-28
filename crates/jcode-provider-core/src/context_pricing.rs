use crate::RuntimeKey;
use serde::{Deserialize, Serialize};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const ANTHROPIC_PRICING: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing#long-context-pricing";

/// Published long-context pricing for a request, not a context limit or a budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LongContextPricing {
    Cutoff {
        above_input_tokens: u64,
        source: String,
    },
    NoCutoff {
        source: String,
    },
    Unknown,
}

impl LongContextPricing {
    /// Requires input for a single request, including cached input. Session totals,
    /// output tokens, and local context-window occupancy are not billing inputs.
    pub fn is_long_context(&self, request_input_tokens: u64) -> Option<bool> {
        match self {
            Self::Cutoff {
                above_input_tokens, ..
            } => Some(request_input_tokens > *above_input_tokens),
            Self::NoCutoff { .. } => Some(false),
            Self::Unknown => None,
        }
    }
}

/// Look up pricing for the actual route, model ID and resolved API base. OAuth
/// does not establish which allowance or rate card a request uses. A key route
/// alone is also insufficient because its base can point to a billing proxy.
pub fn long_context_pricing(
    runtime: &RuntimeKey,
    model: &str,
    api_base: Option<&str>,
) -> LongContextPricing {
    match (runtime, api_base.map(|base| base.trim_end_matches('/'))) {
        (RuntimeKey::OpenAIApiKey, Some(OPENAI_API_BASE)) => match model {
            "gpt-6.2-astra" | "gpt-6.2-sol" | "gpt-6-astra" | "gpt-6.1-sol" | "gpt-6.1-terra"
            | "gpt-6-sol" | "gpt-6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => {
                LongContextPricing::Cutoff {
                    above_input_tokens: 272_000,
                    source: format!("https://developers.openai.com/api/docs/models/{model}"),
                }
            }
            _ => LongContextPricing::Unknown,
        },
        (
            RuntimeKey::AnthropicApiKey,
            Some("https://api.anthropic.com" | "https://api.anthropic.com/v1"),
        ) => match model.strip_suffix("[1m]").unwrap_or(model) {
            "claude-haiku-5-5" => LongContextPricing::Cutoff {
                above_input_tokens: 100_000,
                source: ANTHROPIC_PRICING.to_owned(),
            },
            "claude-fable-5-2" | "claude-fable-5-1" | "claude-fable-5" | "claude-opus-5-5"
            | "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
            | "claude-sonnet-5-5" | "claude-sonnet-5" | "claude-sonnet-4-6" => {
                LongContextPricing::NoCutoff {
                    source: ANTHROPIC_PRICING.to_owned(),
                }
            }
            _ => LongContextPricing::Unknown,
        },
        _ => LongContextPricing::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_api_cutoffs_apply_strictly_above_total_input() {
        for (runtime, model, base, cutoff, source) in [
            (
                RuntimeKey::OpenAIApiKey,
                "gpt-6.2-astra",
                "https://api.openai.com/v1/",
                272_000,
                "https://developers.openai.com/api/docs/models/gpt-6.2-astra",
            ),
            (
                RuntimeKey::OpenAIApiKey,
                "gpt-6-astra",
                "https://api.openai.com/v1",
                272_000,
                "https://developers.openai.com/api/docs/models/gpt-6-astra",
            ),
            (
                RuntimeKey::OpenAIApiKey,
                "gpt-6.1-sol",
                "https://api.openai.com/v1",
                272_000,
                "https://developers.openai.com/api/docs/models/gpt-6.1-sol",
            ),
            (
                RuntimeKey::AnthropicApiKey,
                "claude-haiku-5-5",
                "https://api.anthropic.com/v1",
                100_000,
                "https://platform.claude.com/docs/en/about-claude/pricing#long-context-pricing",
            ),
        ] {
            let pricing = long_context_pricing(&runtime, model, Some(base));
            assert_eq!(pricing.is_long_context(cutoff - 1), Some(false));
            assert_eq!(pricing.is_long_context(cutoff), Some(false));
            assert_eq!(pricing.is_long_context(cutoff + 1), Some(true));
            assert_eq!(
                serde_json::to_value(pricing).unwrap(),
                serde_json::json!({"kind":"cutoff", "above_input_tokens":cutoff, "source":source}),
            );
        }
    }

    #[test]
    fn unknown_routes_and_model_ids_do_not_inherit_the_api_cutoff() {
        for (runtime, model, base) in [
            (
                RuntimeKey::OpenAIOAuth,
                "gpt-6.2-astra",
                Some("https://chatgpt.com/backend-api/codex"),
            ),
            (RuntimeKey::OpenAIOAuth, "gpt-6-astra", None),
            (RuntimeKey::OpenAIOAuth, "gpt-6.1-sol", None),
            (RuntimeKey::OpenAIOAuth, "gpt-5.6-sol", None),
            (RuntimeKey::OpenAIApiKey, "gpt-6.2-astra", None),
            (
                RuntimeKey::OpenAIApiKey,
                "gpt-6.2-astra",
                Some("https://api.openai.com.example/v1"),
            ),
            (
                RuntimeKey::OpenAiCompatible { profile_id: None },
                "gpt-6.2-astra",
                Some("https://api.openai.com/v1"),
            ),
            (
                RuntimeKey::OpenAIApiKey,
                "gpt-6.2-astra-future",
                Some("https://api.openai.com/v1"),
            ),
            (
                RuntimeKey::ClaudeOAuth,
                "claude-haiku-5-5",
                Some("https://api.anthropic.com"),
            ),
        ] {
            let pricing = long_context_pricing(&runtime, model, base);
            assert_eq!(pricing.is_long_context(900_000), None);
            assert_eq!(
                serde_json::to_value(pricing).unwrap(),
                serde_json::json!({"kind":"unknown"}),
            );
        }
    }

    #[test]
    fn published_flat_context_pricing_is_not_an_unknown_cutoff() {
        let pricing = long_context_pricing(
            &RuntimeKey::AnthropicApiKey,
            "claude-opus-5-5[1m]",
            Some("https://api.anthropic.com"),
        );
        assert_eq!(pricing.is_long_context(900_000), Some(false));
        assert_eq!(
            serde_json::to_value(pricing).unwrap(),
            serde_json::json!({
                "kind":"no-cutoff",
                "source":"https://platform.claude.com/docs/en/about-claude/pricing#long-context-pricing"
            }),
        );
    }
}

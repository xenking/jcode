//! System prompt text per model family or exact model (`[[agents.model_prompts]]`).
//!
//! Like opencode's per-family prompts, but configured rather than hardcoded:
//! each rule matches the model ID by glob and either appends to or replaces
//! the default instructions. The match is by model, never by agent role, so
//! it composes with agent profiles instead of competing with them.

use crate::config::{AgentsConfig, ModelPromptConfig, ModelPromptMode};

/// The model prompt rules that apply to one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPrompt {
    /// Replacement for the default instructions (the last matching
    /// `replace` rule wins).
    pub replace: Option<String>,
    /// Texts added after the default instructions, in config order.
    pub append: Vec<String>,
    /// Indices of the matching rules. Two models with the same key get the
    /// same prompt, so a model switch inside a family keeps the prefix.
    pub key: String,
}

impl ModelPrompt {
    pub fn is_empty(&self) -> bool {
        self.replace.is_none() && self.append.is_empty()
    }
}

fn rule_text(rule: &ModelPromptConfig) -> Option<String> {
    let text = match rule.instructions_file.as_deref() {
        Some(path) => {
            let path = crate::agent_profile::expand_home(path);
            match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    crate::logging::warn(&format!(
                        "[model_prompt] instructions_file {} is unreadable: {error}",
                        path.display()
                    ));
                    return None;
                }
            }
        }
        None => rule.instructions.clone()?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Resolve the rules for `provider` / `model`.
pub fn resolve(agents: &AgentsConfig, provider: &str, model: &str) -> ModelPrompt {
    let mut prompt = ModelPrompt::default();
    let mut matched = Vec::new();
    for (index, rule) in agents.matching_model_prompts(provider, model) {
        let Some(text) = rule_text(rule) else {
            continue;
        };
        matched.push(index.to_string());
        match rule.mode {
            ModelPromptMode::Append => prompt.append.push(text),
            ModelPromptMode::Replace => prompt.replace = Some(text),
        }
    }
    prompt.key = matched.join(",");
    prompt
}

#[cfg(test)]
#[path = "model_prompt_tests.rs"]
mod tests;

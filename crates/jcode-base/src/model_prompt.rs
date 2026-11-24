//! System prompt text per model family or exact model (`[[agents.model_prompts]]`).
//!
//! Like opencode's per-family prompts, but configured rather than hardcoded:
//! each rule matches the model ID by glob and either appends to or replaces
//! the default instructions. The match is by model, never by agent role, so
//! it composes with agent profiles instead of competing with them.

use crate::config::{ModelPromptConfig, ModelPromptMode};

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

/// Case-insensitive glob with `*` (any run) and `?` (one character).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

fn rule_matches(rule: &ModelPromptConfig, provider: &str, model: &str) -> bool {
    let routed = format!("{}:{model}", provider.trim());
    rule.patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern.contains(':') {
            glob_match(pattern, &routed)
        } else {
            glob_match(pattern, model)
        }
    })
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
pub fn resolve(rules: &[ModelPromptConfig], provider: &str, model: &str) -> ModelPrompt {
    let mut prompt = ModelPrompt::default();
    let mut matched = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        if !rule_matches(rule, provider, model) {
            continue;
        }
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

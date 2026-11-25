//! What `[agents]` says about the workers a session may spawn: which models
//! they may run on, how much effort and speed they may use, and which model
//! prompt rules apply to them. Consumers ask these questions instead of
//! reading the allowlists and cap maps themselves.

use crate::{AgentsConfig, ModelPromptConfig, WorkerModelSlot};
use jcode_message_types::SpeedMode;

/// The model strings a spawn tool may advertise to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerModelChoices {
    /// Exact `provider:model` routes the agent may pick from.
    pub models: Vec<String>,
    /// True when `spawn_allowed_models` makes these a hard boundary, false for
    /// the legacy opt-in `swarm_model_allowlist`.
    pub hard_boundary: bool,
}

impl WorkerModelChoices {
    fn new(models: &[String], hard_boundary: bool) -> Self {
        Self {
            models: models.to_vec(),
            hard_boundary,
        }
    }
}

impl AgentsConfig {
    /// The saved default model for a worker slot, if one is set.
    pub fn worker_model(&self, slot: WorkerModelSlot) -> Option<&str> {
        match slot {
            WorkerModelSlot::Swarm => self.swarm_model.as_deref(),
            WorkerModelSlot::Memory => self.memory_model.as_deref(),
        }
    }

    /// Models a spawn tool may advertise, or `None` when agents may not choose.
    pub fn worker_model_choices(&self) -> Option<WorkerModelChoices> {
        if !self.spawn_allowed_models.is_empty() {
            return Some(WorkerModelChoices::new(&self.spawn_allowed_models, true));
        }
        (self.swarm_model_selection && !self.swarm_model_allowlist.is_empty())
            .then(|| WorkerModelChoices::new(&self.swarm_model_allowlist, false))
    }

    /// Effort caps that apply to a worker on `model` through `provider_key`
    /// or `provider_name`. The caller keeps the strictest.
    pub fn worker_effort_caps(
        &self,
        provider_key: Option<&str>,
        provider_name: &str,
        model: &str,
    ) -> Vec<&String> {
        Self::effort_cap_keys(provider_key, provider_name, model)
            .iter()
            .filter_map(|key| self.swarm_effort_caps.get(key))
            .collect()
    }

    /// Speed caps that name `model`: the provider prefix of each entry, or
    /// `None` for a bare model entry. The caller decides which prefixes cover
    /// its provider. Empty when no cap names the model.
    pub fn worker_speed_caps(&self, model: &str) -> Vec<(Option<&str>, SpeedMode)> {
        self.swarm_speed_caps
            .iter()
            .filter_map(|(key, mode)| Some((Self::speed_cap_prefix(key, model)?, *mode)))
            .collect()
    }

    /// `[[agents.model_prompts]]` rules that match `provider` / `model`, with
    /// their position in the config.
    pub fn matching_model_prompts(
        &self,
        provider: &str,
        model: &str,
    ) -> Vec<(usize, &ModelPromptConfig)> {
        self.model_prompts
            .iter()
            .enumerate()
            .filter(|(_, rule)| Self::prompt_rule_matches(rule, provider, model))
            .collect()
    }

    /// Every config key a cap for this worker may be stored under: the bare
    /// model, `provider_key:model` and `provider_name:model`, each for the pinned
    /// and unpinned spelling of the model.
    fn effort_cap_keys(
        provider_key: Option<&str>,
        provider_name: &str,
        model: &str,
    ) -> Vec<String> {
        let unpinned = model.split_once('@').map_or(model, |(model, _)| model);
        [model, unpinned]
            .into_iter()
            .flat_map(|model| {
                [
                    Some(model.to_string()),
                    provider_key.map(|key| format!("{key}:{model}")),
                    Some(format!("{}:{model}", provider_name.to_ascii_lowercase())),
                ]
            })
            .flatten()
            .collect()
    }

    /// `Some(None)` for a bare model key, `Some(Some(prefix))` for `prefix:model`,
    /// `None` when the key names another model.
    fn speed_cap_prefix<'a>(key: &'a str, model: &str) -> Option<Option<&'a str>> {
        if key == model {
            return Some(None);
        }
        key.strip_suffix(&format!(":{model}")).map(Some)
    }

    fn prompt_pattern_matches(pattern: &str, provider: &str, model: &str) -> bool {
        let pattern = pattern.trim();
        if pattern.contains(':') {
            Self::glob_match(pattern, &format!("{}:{model}", provider.trim()))
        } else {
            Self::glob_match(pattern, model)
        }
    }

    fn prompt_rule_matches(rule: &ModelPromptConfig, provider: &str, model: &str) -> bool {
        rule.patterns
            .iter()
            .any(|pattern| Self::prompt_pattern_matches(pattern, provider, model))
    }

    /// Case-insensitive glob with `*` (any run) and `?` (one character).
    fn glob_match(pattern: &str, text: &str) -> bool {
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
}

#[cfg(test)]
mod tests {
    use crate::AgentsConfig;

    #[test]
    fn glob_matches_families_and_exact_ids() {
        assert!(AgentsConfig::glob_match("claude-*", "claude-opus-5-5"));
        assert!(AgentsConfig::glob_match("CLAUDE-*", "claude-sonnet-5-5"));
        assert!(AgentsConfig::glob_match("gpt-6*-sol", "gpt-6.2-sol"));
        assert!(AgentsConfig::glob_match("gpt-6.?-sol", "gpt-6.2-sol"));
        assert!(!AgentsConfig::glob_match("gpt-6*-sol", "gpt-6.1-terra"));
        assert!(AgentsConfig::glob_match(
            "*deepseek*",
            "deepseek-v4.1-flash"
        ));
        assert!(AgentsConfig::glob_match("gpt-6-luna", "gpt-6-luna"));
        assert!(!AgentsConfig::glob_match("gpt-6-luna", "gpt-6-luna-mini"));
        assert!(AgentsConfig::glob_match("*", ""));
    }
}

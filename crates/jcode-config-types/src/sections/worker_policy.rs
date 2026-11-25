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
            return Some(WorkerModelChoices {
                models: self.spawn_allowed_models.clone(),
                hard_boundary: true,
            });
        }
        (self.swarm_model_selection && !self.swarm_model_allowlist.is_empty()).then(|| {
            WorkerModelChoices {
                models: self.swarm_model_allowlist.clone(),
                hard_boundary: false,
            }
        })
    }

    /// Effort caps that apply to a worker on `model` through `provider_key`
    /// or `provider_name`. The caller keeps the strictest.
    pub fn worker_effort_caps(
        &self,
        provider_key: Option<&str>,
        provider_name: &str,
        model: &str,
    ) -> Vec<&String> {
        let unpinned = model.split_once('@').map_or(model, |(model, _)| model);
        [model, unpinned]
            .into_iter()
            .flat_map(|model| {
                let route = provider_key.map(|key| format!("{key}:{model}"));
                let provider_route = format!("{}:{model}", provider_name.to_ascii_lowercase());
                [
                    self.swarm_effort_caps.get(model),
                    route
                        .as_ref()
                        .and_then(|key| self.swarm_effort_caps.get(key)),
                    self.swarm_effort_caps.get(&provider_route),
                ]
                .into_iter()
                .flatten()
            })
            .collect()
    }

    /// Speed caps that name `model`: the provider prefix of each entry, or
    /// `None` for a bare model entry. The caller decides which prefixes cover
    /// its provider.
    pub fn worker_speed_caps(&self, model: &str) -> Vec<(Option<&str>, SpeedMode)> {
        let suffix = format!(":{model}");
        self.swarm_speed_caps
            .iter()
            .filter_map(|(key, mode)| {
                if key == model {
                    Some((None, *mode))
                } else {
                    key.strip_suffix(&suffix)
                        .map(|prefix| (Some(prefix), *mode))
                }
            })
            .collect()
    }

    /// Whether any worker speed cap is configured.
    pub fn has_worker_speed_caps(&self) -> bool {
        !self.swarm_speed_caps.is_empty()
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
            .filter(|(_, rule)| prompt_rule_matches(rule, provider, model))
            .collect()
    }
}

fn prompt_rule_matches(rule: &ModelPromptConfig, provider: &str, model: &str) -> bool {
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

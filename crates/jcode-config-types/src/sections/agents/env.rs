//! Environment overrides, persisted-preference setters and `/config` labels
//! for the `[agents]` section.
//!
//! They live beside [`AgentsConfig`] so no other module has to know its field
//! names: callers say what they want changed or shown.

use super::{AgentsConfig, SwarmSpawnMode};
use crate::env::parse_bool as parse_env_bool;

/// Which worker model default `/agents global` is changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerModelSlot {
    Swarm,
    Memory,
}

/// Where remote memory embeddings come from. Unset fields mean the backend's
/// own defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEmbedding {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub dim: Option<usize>,
}

/// Labels the `/config` summary prints for the agent models block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsSummary {
    pub swarm_model: String,
    pub swarm_spawn_mode: &'static str,
    pub swarm_root_effort: &'static str,
    pub memory_jev_provider: String,
    pub memory_sidecar: &'static str,
}

impl AgentsConfig {
    /// Apply `JCODE_*` agent environment overrides.
    pub fn apply_env_overrides(&mut self) {
        self.apply_swarm_env();
        self.apply_memory_env();
        self.apply_embedding_env();
    }

    #[expect(
        clippy::collapsible_if,
        reason = "Environment override parsing is intentionally explicit and grouped by config area"
    )]
    fn apply_swarm_env(&mut self) {
        if let Ok(v) = std::env::var("JCODE_SWARM_MODEL") {
            let trimmed = v.trim();
            self.swarm_model = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(v) = std::env::var("JCODE_SWARM_EFFORT") {
            let trimmed = v.trim();
            self.swarm_effort = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(value) = std::env::var("JCODE_SWARM_ROOT_EFFORT") {
            let value = value.trim();
            self.swarm_root_effort = (!value.is_empty()).then(|| value.to_string());
        }
        if let Ok(v) = std::env::var("JCODE_SWARM_SPAWN_MODE") {
            if let Some(parsed) = SwarmSpawnMode::parse(&v) {
                self.swarm_spawn_mode = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SWARM_MAX_CONCURRENT_AGENTS") {
            if let Ok(parsed) = v.trim().parse::<usize>() {
                self.swarm_max_concurrent_agents = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SWARM_STALL_AFTER_SECS") {
            if let Ok(parsed) = v.trim().parse::<u64>() {
                self.swarm_stall_after_secs = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SWARM_STALL_NUDGE") {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => self.swarm_stall_nudge = true,
                "0" | "false" | "no" | "off" => self.swarm_stall_nudge = false,
                _ => {}
            }
        }
    }

    #[expect(
        clippy::collapsible_if,
        reason = "Environment override parsing is intentionally explicit and grouped by config area"
    )]
    fn apply_memory_env(&mut self) {
        if let Ok(v) = std::env::var("JCODE_MEMORY_JEV_PROVIDER") {
            self.memory_jev_provider = v.trim().to_ascii_lowercase();
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_AUTO_RECALL") {
            if let Ok(enabled) = v.trim().parse::<bool>() {
                self.memory_auto_recall = enabled;
            }
        }
        for (key, target) in [
            ("JCODE_MEMORY_JEV_MODEL", &mut self.memory_jev_model),
            ("JCODE_BROWSER_JEV_MODEL", &mut self.browser_jev_model),
            ("JCODE_VOICE_JEV_MODEL", &mut self.voice_jev_model),
        ] {
            if let Ok(value) = std::env::var(key) {
                let value = value.trim();
                *target = (!value.is_empty()).then(|| value.to_string());
            }
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_MODEL") {
            let trimmed = v.trim();
            self.memory_model = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(v) = std::env::var("JCODE_TITLE_MODEL") {
            let trimmed = v.trim();
            self.title_model = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_SIDECAR_ENABLED") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.memory_sidecar_enabled = parsed;
            }
        }
    }

    #[expect(
        clippy::collapsible_if,
        reason = "Environment override parsing is intentionally explicit and grouped by config area"
    )]
    fn apply_embedding_env(&mut self) {
        if let Ok(v) = std::env::var("JCODE_MEMORY_EMBEDDING_BACKEND") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.memory_embedding_backend = trimmed.to_string();
            }
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_EMBEDDING_MODEL") {
            let trimmed = v.trim();
            self.memory_embedding_model = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_EMBEDDING_BASE_URL") {
            let trimmed = v.trim();
            self.memory_embedding_base_url = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Ok(v) = std::env::var("JCODE_MEMORY_EMBEDDING_DIM") {
            if let Ok(parsed) = v.trim().parse::<usize>() {
                self.memory_embedding_dim = Some(parsed);
            }
        }
    }

    /// Persisted worker-model default. `None` clears it.
    pub fn set_worker_model(&mut self, slot: WorkerModelSlot, model: Option<String>) {
        match slot {
            WorkerModelSlot::Swarm => self.swarm_model = model,
            WorkerModelSlot::Memory => self.memory_model = model,
        }
    }

    /// Settings for the OpenAI-compatible embeddings backend, or `None` when
    /// the local backend is selected.
    pub fn remote_embedding(&self) -> Option<RemoteEmbedding> {
        self.memory_embedding_backend
            .eq_ignore_ascii_case("openai")
            .then(|| RemoteEmbedding {
                model: self.memory_embedding_model.clone(),
                base_url: self.memory_embedding_base_url.clone(),
                dim: self.memory_embedding_dim,
            })
    }

    /// Per-name agent profile overrides.
    pub fn profiles(&self) -> &std::collections::BTreeMap<String, crate::AgentProfileConfig> {
        &self.profiles
    }

    /// What the `/config` summary prints for the agent models block.
    pub fn summary(&self) -> AgentsSummary {
        AgentsSummary {
            swarm_model: self
                .swarm_model
                .as_deref()
                .unwrap_or("(inherit current session)")
                .to_string(),
            swarm_spawn_mode: self.swarm_spawn_mode.as_str(),
            swarm_root_effort: self.root_effort_for_swarm(),
            memory_jev_provider: self.memory_jev_provider.clone(),
            memory_sidecar: if self.memory_sidecar_enabled {
                "enabled"
            } else {
                "disabled"
            },
        }
    }
}

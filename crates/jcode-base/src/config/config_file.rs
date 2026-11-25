use super::*;
use crate::storage::jcode_dir;
use std::path::PathBuf;

/// Root of the shared config, as far as this module cares.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DesktopRoot {
    desktop: DesktopSection,
}

/// `[desktop]` tables written by the Desktop client.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DesktopSection {
    models: DesktopModelsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DesktopModelsSection {
    /// Routed model specs (`openai-oauth:gpt-5.6-sol`) the Desktop picker hides.
    hidden: Vec<String>,
}

/// Route specs the Desktop model picker hides.
///
/// Desktop owns `[desktop.models] hidden` and writes it to the shared config
/// file, so the terminal `/model` picker reads the list from the same file
/// rather than from [`Config`]: one setting hides a model in every client, and
/// a `Config::save` never rewrites a section this crate does not own.
///
/// A missing, malformed, or Desktop-less config hides nothing.
pub fn desktop_hidden_models() -> Vec<String> {
    let Some(path) = Config::path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    desktop_hidden_models_from(&text)
}

/// Parse the hidden list out of one config document.
fn desktop_hidden_models_from(text: &str) -> Vec<String> {
    let Ok(section) = toml::from_str::<DesktopRoot>(text) else {
        return Vec::new();
    };
    let mut hidden: Vec<String> = section
        .desktop
        .models
        .hidden
        .into_iter()
        .map(|spec| spec.trim().to_string())
        .filter(|spec| !spec.is_empty())
        .collect();
    hidden.sort();
    hidden.dedup();
    hidden
}

impl Config {
    /// Persist only the update preference, without baking in environment overrides.
    pub fn set_update_channel(channel: UpdateChannel) -> anyhow::Result<()> {
        let mut config = Self::load_for_update()?;
        config.features.update_channel = channel;
        config.save()
    }

    /// Get the config file path
    pub fn path() -> Option<PathBuf> {
        jcode_dir().ok().map(|d| d.join("config.toml"))
    }

    /// Load config from file, with environment variable overrides
    pub fn load() -> Self {
        let mut config = Self::load_from_file().unwrap_or_default();
        config.apply_env_overrides();
        config.agents.apply_swarm_pool();
        config
    }

    /// Load config from file, with environment variable overrides.
    ///
    /// Unlike [`Self::load`], this returns TOML/read errors to callers that need
    /// to distinguish a malformed config from an absent config.
    pub fn load_strict() -> anyhow::Result<Self> {
        let mut config = Self::load_from_file_strict()?.unwrap_or_default();
        config.apply_env_overrides();
        config.agents.apply_swarm_pool();
        Ok(config)
    }

    /// Load the on-disk config for a read-modify-write operation.
    ///
    /// Unlike [`Self::load`], this never converts a parse error into defaults.
    /// Saving those defaults would destroy the user's existing config. It also
    /// deliberately skips environment overrides so transient process settings
    /// are not baked into the file as a side effect of changing one preference.
    pub(crate) fn load_for_update() -> anyhow::Result<Self> {
        Ok(Self::load_from_file_strict()?.unwrap_or_default())
    }

    /// Load config from file only (no env overrides)
    fn load_from_file() -> Option<Self> {
        match Self::load_from_file_strict() {
            Ok(config) => config,
            Err(e) => {
                crate::logging::error(&format!("Failed to parse config file: {}", e));
                None
            }
        }
    }

    /// Load config from file only (no env overrides), preserving parse/read errors.
    fn load_from_file_strict() -> anyhow::Result<Option<Self>> {
        let Some(path) = Self::path() else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path.display(), e))?;
        let mut config = toml::from_str::<Self>(&content).map_err(|e| {
            anyhow::anyhow!("Failed to parse config file {}: {}", path.display(), e)
        })?;
        config.display.apply_legacy_compat();
        // An explicit discovery opt-out is authoritative. Older saves wrote the
        // default endpoint too, so table shape cannot establish user intent.
        Ok(Some(config))
    }

    /// Save config to file
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("No config path"))?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Self::invalidate_cache();
        Ok(())
    }

    /// Whether `[auth]` trusts this (normalized) source id.
    pub(crate) fn trusts_source(&self, source_id: &str) -> bool {
        self.auth.trusts_source(source_id)
    }

    /// Whether `[auth]` trusts this `source|path` entry.
    pub(crate) fn trusts_path_entry(&self, entry: &str) -> bool {
        self.auth.trusts_path_entry(entry)
    }

    /// Change the saved `[auth]` trust lists. Saves only when `change` reports
    /// that it altered them, and returns whether it did.
    pub(crate) fn update_trust(
        change: impl FnOnce(&mut AuthConfig) -> bool,
    ) -> anyhow::Result<bool> {
        let mut cfg = Self::load_for_update()?;
        let changed = change(&mut cfg.auth);
        if changed {
            cfg.save()?;
        }
        Ok(changed)
    }

    /// Mark the process-cached config as stale and notify dependent caches.
    pub fn invalidate_cache() {
        super::invalidate_config_cache();
    }

    /// Persist a worker-model default (`/agents global`). None clears it.
    pub fn set_agent_model_default(target: &str, model: Option<&str>) -> anyhow::Result<()> {
        let value = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let mut cfg = Self::load_for_update()?;
        match target {
            "swarm" => cfg.agents.set_worker_model(WorkerModelSlot::Swarm, value),
            "review" => cfg.autoreview.model = value,
            "judge" => cfg.autojudge.model = value,
            "memory" => cfg.agents.set_worker_model(WorkerModelSlot::Memory, value),
            "ambient" => cfg.ambient.model = value,
            other => anyhow::bail!("Unknown agent model target: {other}"),
        }
        cfg.save()
    }

    /// Persist the Anthropic cache duration for clients and the shared daemon.
    pub fn set_anthropic_cache_ttl_1h(enabled: bool) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.anthropic_cache_ttl_1h = enabled;
        cfg.save()
    }

    /// Update the copilot premium mode in the config file.
    /// Reloads, patches, and saves so it doesn't clobber other fields.
    pub fn set_copilot_premium(mode: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.copilot_premium = mode.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved copilot_premium to config: {}",
            mode.unwrap_or("(none)")
        ));
        Ok(())
    }

    /// Update just the default model and provider in the config file.
    /// This reloads, patches, and saves so it doesn't clobber other fields.
    pub fn set_default_model(model: Option<&str>, provider: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.default_model = model.map(|s| s.to_string());
        cfg.provider.default_provider = provider.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved default model: {}, provider: {}",
            model.unwrap_or("(none)"),
            provider.unwrap_or("(auto)")
        ));
        // The first auto-switch account is the default, so the order follows
        // whichever path changed it. Listeners log their own failures: a
        // failure there must not undo the save.
        if let Some(route) = provider {
            super::notify_default_route_changed(route);
        }
        Ok(())
    }

    /// Update just the default provider in the config file.
    pub fn set_default_provider(provider: Option<&str>) -> anyhow::Result<()> {
        let cfg = Self::load_for_update()?;
        Self::set_default_model(cfg.provider.default_model.as_deref(), provider)
    }

    /// Update just the default model in the config file.
    pub fn set_default_model_only(model: Option<&str>) -> anyhow::Result<()> {
        let cfg = Self::load_for_update()?;
        Self::set_default_model(model, cfg.provider.default_provider.as_deref())
    }

    /// Update the persisted OpenAI reasoning effort preference.
    pub fn set_openai_reasoning_effort(value: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.openai_reasoning_effort = value.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved openai_reasoning_effort to config: {}",
            value.unwrap_or("(none)")
        ));
        Ok(())
    }

    /// Update the persisted Anthropic reasoning effort preference.
    pub fn set_anthropic_reasoning_effort(value: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.anthropic_reasoning_effort = value.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved anthropic_reasoning_effort to config: {}",
            value.unwrap_or("(none)")
        ));
        Ok(())
    }

    /// Update the persisted OpenAI transport preference.
    pub fn set_openai_transport(value: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.openai_transport = value.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved openai_transport to config: {}",
            value.unwrap_or("(none)")
        ));
        Ok(())
    }

    /// Update the persisted OpenAI service tier preference.
    pub fn set_openai_service_tier(value: Option<&str>) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.provider.openai_service_tier = value.map(|s| s.to_string());
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved openai_service_tier to config: {}",
            value.unwrap_or("(none)")
        ));
        Ok(())
    }

    fn save_display_preference(preference: DisplayPreference) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.display.apply(preference);
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved display.{} to config: {}",
            preference.key(),
            preference.value_label()
        ));
        Ok(())
    }

    /// Update the persisted default alignment preference.
    pub fn set_display_centered(centered: bool) -> anyhow::Result<()> {
        Self::save_display_preference(DisplayPreference::Centered(centered))
    }

    /// Update the persisted reasoning display mode preference.
    pub fn set_reasoning_display(mode: ReasoningDisplayMode) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.display.set_reasoning_display(mode);
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved display.reasoning_display to config: {}",
            mode.label()
        ));
        Ok(())
    }

    /// Update the persisted compact-notifications preference.
    pub fn set_compact_notifications(compact: bool) -> anyhow::Result<()> {
        Self::save_display_preference(DisplayPreference::CompactNotifications(compact))
    }

    /// Save the todo header mode and keep the legacy pin flag in sync.
    pub fn set_todo_header_mode(mode: TodoHeaderMode) -> anyhow::Result<()> {
        let mut cfg = Self::load_for_update()?;
        cfg.display.set_todo_header_mode(mode);
        cfg.save()?;
        crate::logging::info(&format!(
            "Saved display.todo_header to config: {}",
            mode.as_str()
        ));
        Ok(())
    }

    /// Update the persisted show-agentgrep-output preference.
    pub fn set_show_agentgrep_output(show: bool) -> anyhow::Result<()> {
        Self::save_display_preference(DisplayPreference::ShowAgentgrepOutput(show))
    }

    /// Update the persisted LaTeX rendering mode.
    pub fn set_latex_rendering(mode: LatexRenderingMode) -> anyhow::Result<()> {
        Self::save_display_preference(DisplayPreference::LatexRendering(mode))
    }

    /// Update the persisted tool-call-details preference.
    pub fn set_tool_call_details(show: bool) -> anyhow::Result<()> {
        Self::save_display_preference(DisplayPreference::ToolCallDetails(show))
    }

    /// One-time migration: flip a persisted legacy `swarm_spawn_mode =
    /// "visible"` to the current `"inline"` default.
    ///
    /// Historically `visible` was the default, and any full-config
    /// `Config::save()` (model switches, display toggles, ...) baked that
    /// then-default into the user's config.toml. When the default changed to
    /// `inline`, those users stayed pinned to `visible` forever. This rewrites
    /// exactly that one line (preserving the rest of the file byte-for-byte)
    /// and drops a marker so it runs at most once. A user who explicitly sets
    /// `visible` after the migration is never flipped again.
    ///
    /// Returns `true` when it rewrote the config. Best-effort: errors are
    /// logged and swallowed.
    pub fn migrate_legacy_swarm_spawn_mode_once() -> bool {
        migrate_line_once(LineMigration {
            marker: "swarm-spawn-mode-inline",
            marker_text: "swarm_spawn_mode default migration: visible -> inline\n",
            key: "swarm_spawn_mode",
            rewrite: |value| {
                let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                matches!(value, "visible" | "headed").then(|| "\"inline\"".to_string())
            },
            migrated: "Migrated legacy swarm_spawn_mode \"visible\" to \"inline\" in config.toml",
        })
    }

    /// One-time migration: flip a persisted `idle_animation = true` to `false`.
    ///
    /// The idle animation is being turned off for everyone. Users who toggled
    /// it on earlier (or had the old `true` default baked in by a full
    /// `Config::save()`) get flipped off once. This rewrites exactly that one
    /// line (preserving the rest of the file byte-for-byte) and drops a marker
    /// so it runs at most once. A user who explicitly re-enables it after the
    /// migration is never flipped again.
    ///
    /// Returns `true` when it rewrote the config. Best-effort: errors are
    /// logged and swallowed.
    pub fn migrate_idle_animation_off_once() -> bool {
        migrate_line_once(LineMigration {
            marker: "idle-animation-off",
            marker_text: "idle_animation forced migration: true -> false\n",
            key: "idle_animation",
            rewrite: |value| {
                let value = value.split('#').next().unwrap_or("");
                (value.trim() == "true").then(|| "false".to_string())
            },
            migrated: "Migrated idle_animation \"true\" to \"false\" in config.toml",
        })
    }
}

/// A one-time rewrite of a single top-level `key = value` line in config.toml.
struct LineMigration<F: Fn(&str) -> Option<String>> {
    /// File under `~/.jcode/migrations/` recording that the migration ran.
    marker: &'static str,
    marker_text: &'static str,
    key: &'static str,
    /// Given the text after `=`, the replacement value, or `None` to leave the
    /// line alone.
    rewrite: F,
    migrated: &'static str,
}

/// Rewrite the first matching `key = value` line, keeping every other byte of
/// the file, and drop a marker so the migration runs at most once. Returns
/// whether it rewrote the config. Best-effort: errors are logged and
/// swallowed.
fn migrate_line_once<F: Fn(&str) -> Option<String>>(migration: LineMigration<F>) -> bool {
    let Ok(dir) = jcode_dir() else {
        return false;
    };
    let marker = dir.join("migrations").join(migration.marker);
    if marker.exists() {
        return false;
    }
    let write_marker = || {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, migration.marker_text);
    };

    let path = dir.join("config.toml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        // No config file (fresh install): nothing to migrate.
        write_marker();
        return false;
    };

    let mut changed = false;
    let migrated: Vec<String> = content
        .lines()
        .map(|line| {
            if changed {
                return line.to_string();
            }
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix(migration.key) else {
                return line.to_string();
            };
            let Some(value) = rest.trim_start().strip_prefix('=') else {
                return line.to_string();
            };
            match (migration.rewrite)(value) {
                Some(replacement) => {
                    changed = true;
                    let indent = &line[..line.len() - trimmed.len()];
                    format!("{indent}{} = {replacement}", migration.key)
                }
                None => line.to_string(),
            }
        })
        .collect();

    if !changed {
        write_marker();
        return false;
    }

    let mut new_content = migrated.join("\n");
    if content.ends_with('\n') {
        new_content.push('\n');
    }
    match std::fs::write(&path, new_content) {
        Ok(()) => {
            Config::invalidate_cache();
            write_marker();
            crate::logging::info(migration.migrated);
            true
        }
        Err(err) => {
            crate::logging::warn(&format!(
                "{} migration failed to write config: {err}",
                migration.key
            ));
            false
        }
    }
}

#[cfg(test)]
mod issue_1056_tests {
    use super::{Config, desktop_hidden_models, desktop_hidden_models_from};

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => crate::env::set_var(self.key, value),
                None => crate::env::remove_var(self.key),
            }
            Config::invalidate_cache();
        }
    }

    #[test]
    fn update_channel_save_is_persistent_and_preserves_unrelated_preferences() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("JCODE_HOME", home.path());
        let _channel = EnvGuard::set("JCODE_UPDATE_CHANNEL", "main");
        let path = home.path().join("config.toml");
        std::fs::write(
            &path,
            "[features]\nmemory = false\n[display]\ncentered = true\n",
        )
        .unwrap();
        Config::set_update_channel(super::UpdateChannel::Preview).unwrap();
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.features.update_channel, super::UpdateChannel::Preview);
        assert!(!saved.features.memory);
        assert!(saved.display.centered);
        Config::set_update_channel(super::UpdateChannel::Stable).unwrap();
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.features.update_channel, super::UpdateChannel::Stable);
        std::fs::write(&path, "[broken").unwrap();
        assert!(Config::set_update_channel(super::UpdateChannel::Preview).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
    }

    #[test]
    fn effort_update_preserves_profile_with_capitalized_bearer_auth() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("JCODE_HOME", home.path());
        let path = home.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[provider]
openai_reasoning_effort = "low"

[providers.mistral]
type = "openai-compatible"
base_url = "https://api.mistral.ai/v1"
auth = "Bearer"
api_key_env = "MISTRAL_API_KEY"
disable_reasoning_heuristics = true

[[providers.mistral.models]]
id = "mistral-medium-latest"
reasoning = true
reasoning_effort = "max"
"#,
        )
        .unwrap();

        Config::set_openai_reasoning_effort(Some("high")).unwrap();

        let saved = std::fs::read_to_string(path).unwrap();
        assert!(saved.contains("[providers.mistral]"));
        assert!(saved.contains("mistral-medium-latest"));
        let parsed = Config::load_strict().unwrap();
        assert_eq!(
            parsed.provider.openai_reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(parsed.providers["mistral"].models.len(), 1);
    }

    #[test]
    fn effort_update_refuses_to_overwrite_malformed_config() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("JCODE_HOME", home.path());
        let path = home.path().join("config.toml");
        let original = "[providers.broken]\nauth = \"invalid-auth-mode\"\n";
        std::fs::write(&path, original).unwrap();

        let error = Config::set_openai_reasoning_effort(Some("high"))
            .expect_err("a malformed config must block mutation");

        assert!(error.to_string().contains("Failed to parse config file"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    /// Desktop owns `[desktop.models] hidden`; the terminal picker reads the
    /// same section out of the same file.
    #[test]
    fn desktop_hidden_models_reads_the_section_desktop_writes() {
        assert_eq!(
            desktop_hidden_models_from(
                "# my settings\nmodel = \"openai:gpt-5\"\n\n[provider]\ndefault_provider = \"openai\"\n\n[desktop.models]\nhidden = [\"openai-oauth:gpt-5.6-sol\", \" b:y \", \"b:y\"]\n\n[desktop.appearance]\ntheme = \"dark\"\n"
            ),
            ["b:y", "openai-oauth:gpt-5.6-sol"]
        );
        // Nothing hidden: no section, an empty list, or a config we cannot read.
        assert!(desktop_hidden_models_from("model = \"x\"\n").is_empty());
        assert!(desktop_hidden_models_from("[desktop.models]\nhidden = []\n").is_empty());
        assert!(desktop_hidden_models_from("[desktop.models]\nhidden = [").is_empty());
    }

    #[test]
    fn desktop_hidden_models_follow_the_config_file() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("JCODE_HOME", home.path());
        let path = home.path().join("config.toml");
        assert!(desktop_hidden_models().is_empty());
        std::fs::write(
            &path,
            "[desktop.models]\nhidden = [\"claude-oauth:claude-opus-5-5\"]\n",
        )
        .unwrap();
        assert_eq!(desktop_hidden_models(), ["claude-oauth:claude-opus-5-5"]);
        std::fs::remove_file(&path).unwrap();
        assert!(desktop_hidden_models().is_empty());
    }
}

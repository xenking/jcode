use super::{
    AgentsConfig, AmbientConfig, Config, DiffDisplayMode, DisplayConfig, HookCommands,
    LatexRenderingMode, McpToolsMode, ProviderConfig, SessionPickerResumeAction, SwarmSpawnMode,
    ToolConfig, config_env_fingerprint, populate_context_limits_from_config_ref,
};
use std::ffi::OsString;
use std::path::Path;

fn restore_env_var(key: &str, previous: Option<OsString>) {
    if let Some(previous) = previous {
        crate::env::set_var(key, previous);
    } else {
        crate::env::remove_var(key);
    }
}

#[test]
fn jarvis_speech_endpoint_override_and_cache_fingerprint() {
    let _guard = crate::storage::lock_test_env();
    struct Restore(Option<OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            restore_env_var("JCODE_JARVIS_SPEECH_ENDPOINT", self.0.take());
        }
    }
    let _restore = Restore(std::env::var_os("JCODE_JARVIS_SPEECH_ENDPOINT"));
    crate::env::remove_var("JCODE_JARVIS_SPEECH_ENDPOINT");
    let before = config_env_fingerprint();
    crate::env::set_var(
        "JCODE_JARVIS_SPEECH_ENDPOINT",
        "http://127.0.0.1:8765/v1/audio/speech",
    );
    assert_ne!(before, config_env_fingerprint());
    let mut config: Config =
        toml::from_str("[jarvis.speech]\nendpoint = 'http://old.example/tts'").unwrap();
    config.apply_env_overrides();
    assert_eq!(
        config.jarvis.speech.endpoint.as_deref(),
        Some("http://127.0.0.1:8765/v1/audio/speech")
    );
    crate::env::set_var("JCODE_JARVIS_SPEECH_ENDPOINT", "");
    config.apply_env_overrides();
    assert_eq!(config.jarvis.speech.endpoint.as_deref(), Some(""));
}

struct GeminiConfigEnv {
    _home: tempfile::TempDir,
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl GeminiConfigEnv {
    fn new() -> Self {
        let previous = [
            "JCODE_HOME",
            "JCODE_GEMINI_FORCE_OAUTH",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_PROJECT_ID",
        ]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
        let home = tempfile::tempdir().unwrap();
        crate::env::set_var("JCODE_HOME", home.path());
        for key in [
            "JCODE_GEMINI_FORCE_OAUTH",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_PROJECT_ID",
        ] {
            crate::env::remove_var(key);
        }
        Config::invalidate_cache();
        Self {
            _home: home,
            previous,
        }
    }
}

impl Drop for GeminiConfigEnv {
    fn drop(&mut self) {
        for (key, previous) in self.previous.drain(..) {
            restore_env_var(key, previous);
        }
        Config::invalidate_cache();
    }
}

#[test]
fn swarm_stall_watchdog_defaults_and_parses() {
    let defaults = Config::default().agents;
    assert_eq!(defaults.swarm_stall_after_secs, 600);
    assert!(!defaults.swarm_stall_nudge);

    let cfg: Config =
        toml::from_str("[agents]\nswarm_stall_after_secs = 0\nswarm_stall_nudge = true\n")
            .expect("stall watchdog settings should parse");
    assert_eq!(cfg.agents.swarm_stall_after_secs, 0);
    assert!(cfg.agents.swarm_stall_nudge);
}

#[test]
fn swarm_max_concurrent_agents_defaults_to_safe_live_worker_budget() {
    // Keep enough parallelism for deep fan-out without allowing recursive ad hoc
    // spawns to grow until the 1000-member hard cap exhausts machine memory.
    assert_eq!(Config::default().agents.swarm_max_concurrent_agents, 32);
}

#[test]
fn mermaid_feature_defaults_on_and_parses_false() {
    assert!(Config::default().features.mermaid);

    let cfg: Config =
        toml::from_str("[features]\nmermaid = false\n").expect("features.mermaid should parse");
    assert!(!cfg.features.mermaid);
}

#[test]
fn mermaid_environment_override_uses_standard_boolean_values() {
    let _guard = crate::storage::lock_test_env();
    let previous = std::env::var_os("JCODE_ENABLE_MERMAID");
    crate::env::set_var("JCODE_ENABLE_MERMAID", "off");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert!(!cfg.features.mermaid);

    restore_env_var("JCODE_ENABLE_MERMAID", previous);
}

#[test]
fn auto_poke_feature_defaults_on_and_parses_false() {
    assert!(Config::default().features.auto_poke);

    let cfg: Config =
        toml::from_str("[features]\nauto_poke = false\n").expect("features.auto_poke should parse");
    assert!(!cfg.features.auto_poke);
}

#[test]
fn todo_gate_diagnostics_defaults_off_and_requires_explicit_opt_in() {
    assert!(!Config::default().features.todo_gate_diagnostics);
    let omitted: Config = toml::from_str("[features]\nauto_poke = true\n").unwrap();
    assert!(!omitted.features.todo_gate_diagnostics);
    for enabled in [true, false] {
        let cfg: Config =
            toml::from_str(&format!("[features]\ntodo_gate_diagnostics = {enabled}\n")).unwrap();
        assert_eq!(cfg.features.todo_gate_diagnostics, enabled);
    }
}

#[test]
fn todo_gate_digest_defaults_off_and_requires_explicit_opt_in() {
    assert!(!Config::default().features.todo_gate_digest);
    let omitted: Config = toml::from_str("[features]\nauto_poke = true\n").unwrap();
    assert!(!omitted.features.todo_gate_digest);
    let enabled: Config = toml::from_str("[features]\ntodo_gate_digest = true\n").unwrap();
    assert!(enabled.features.todo_gate_digest);
    assert!(enabled.features.auto_poke);
    let disabled: Config = toml::from_str("[features]\ntodo_gate_digest = false\n").unwrap();
    assert!(!disabled.features.todo_gate_digest);
}

#[test]
fn auto_poke_toggle_key_defaults_parses_and_reports_disabled() {
    assert_eq!(Config::default().keybindings.auto_poke_toggle, "ctrl+p");

    let remapped: Config = toml::from_str("[keybindings]\nauto_poke_toggle = \"alt+p\"\n")
        .expect("keybindings.auto_poke_toggle should parse");
    assert_eq!(remapped.keybindings.auto_poke_toggle, "alt+p");

    let disabled: Config = toml::from_str("[keybindings]\nauto_poke_toggle = \"\"\n")
        .expect("an empty auto-poke toggle should parse");
    assert!(disabled.keybindings.auto_poke_toggle.is_empty());
    assert!(
        disabled
            .display_string()
            .contains("- Auto-poke toggle: `disabled`")
    );
}

#[test]
fn auto_poke_environment_override_uses_standard_boolean_values() {
    let _guard = crate::storage::lock_test_env();
    let previous = std::env::var_os("JCODE_AUTO_POKE");
    crate::env::set_var("JCODE_AUTO_POKE", "off");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert!(!cfg.features.auto_poke);

    restore_env_var("JCODE_AUTO_POKE", previous);
}

#[path = "config_latex_tests.rs"]
mod latex;

#[test]
fn swarm_max_concurrent_agents_parses_and_allows_zero_for_unbounded() {
    let cfg: Config = toml::from_str("[agents]\nswarm_max_concurrent_agents = 64\n")
        .expect("swarm_max_concurrent_agents should parse");
    assert_eq!(cfg.agents.swarm_max_concurrent_agents, 64);

    let cfg: Config = toml::from_str("[agents]\nswarm_max_concurrent_agents = 0\n")
        .expect("zero should parse (disables the configurable live-agent guard)");
    assert_eq!(cfg.agents.swarm_max_concurrent_agents, 0);
}

#[test]
fn swarm_spawn_mode_parses_supported_values() {
    let cfg: Config = toml::from_str("[agents]\nswarm_spawn_mode = \"headless\"\n")
        .expect("headless swarm_spawn_mode should parse");
    assert_eq!(cfg.agents.swarm_spawn_mode, SwarmSpawnMode::Headless);

    let cfg: Config = toml::from_str("[agents]\nswarm_spawn_mode = \"auto\"\n")
        .expect("auto swarm_spawn_mode should parse");
    assert_eq!(cfg.agents.swarm_spawn_mode, SwarmSpawnMode::Auto);

    let cfg: Config = toml::from_str("[agents]\nswarm_spawn_mode = \"visible\"\n")
        .expect("visible swarm_spawn_mode should parse");
    assert_eq!(cfg.agents.swarm_spawn_mode, SwarmSpawnMode::Visible);
}

#[test]
fn retired_swarm_strip_settings_are_ignored() {
    for layout in ["vertical", "horizontal", "obsolete"] {
        let cfg: Config = toml::from_str(&format!(
            "[agents]\nswarm_strip_layout = {layout:?}\nswarm_gallery_max_pct = 40\nswarm_spawn_mode = \"headless\"\n"
        ))
        .expect("retired swarm settings should be ignored in existing configs");
        assert_eq!(cfg.agents.swarm_spawn_mode, SwarmSpawnMode::Headless);

        let serialized = toml::to_string(&cfg).expect("config should serialize");
        assert!(!serialized.contains("swarm_strip_layout"));
        assert!(!serialized.contains("swarm_gallery_max_pct"));
    }
}

#[test]
fn swarm_spawn_mode_rejects_invalid_values() {
    let result = toml::from_str::<Config>("[agents]\nswarm_spawn_mode = \"background\"\n");
    assert!(result.is_err());
}

#[test]
fn swarm_spawn_mode_as_str_round_trips() {
    for mode in [
        SwarmSpawnMode::Visible,
        SwarmSpawnMode::Headless,
        SwarmSpawnMode::Auto,
    ] {
        assert_eq!(SwarmSpawnMode::parse(mode.as_str()), Some(mode));
    }
}

#[test]
fn test_env_override_swarm_spawn_mode() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_SWARM_SPAWN_MODE");
    crate::env::set_var("JCODE_SWARM_SPAWN_MODE", "headless");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert_eq!(cfg.agents.swarm_spawn_mode, SwarmSpawnMode::Headless);

    restore_env_var("JCODE_SWARM_SPAWN_MODE", prev);
}

#[test]
fn test_env_override_swarm_model() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_SWARM_MODEL");
    crate::env::set_var("JCODE_SWARM_MODEL", "claude-opus-4-6");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert_eq!(cfg.agents.swarm_model.as_deref(), Some("claude-opus-4-6"));

    // Empty value clears the override back to "inherit".
    crate::env::set_var("JCODE_SWARM_MODEL", "  ");
    let mut cfg = Config::default();
    cfg.agents.swarm_model = Some("preset".to_string());
    cfg.apply_env_overrides();
    assert_eq!(cfg.agents.swarm_model, None);

    restore_env_var("JCODE_SWARM_MODEL", prev);
}

#[test]
fn spawn_allowed_models_is_a_hard_worker_allowlist() {
    // Unset: no restriction, so the configured swarm_model is trusted.
    let agents = Config::default().agents;
    assert!(agents.spawn_allowed_models.is_empty());
    assert!(agents.validate_spawn_allowed_model(None).is_ok());
    assert!(
        agents
            .validate_spawn_allowed_model(Some("claude:claude-fable-5"))
            .is_ok()
    );
    assert!(agents.worker_may_use_model("claude:claude-fable-5"));

    let cfg: Config = toml::from_str(
        "[agents]\nspawn_allowed_models = [\"opencode-go:deepseek-v4.1-flash\", \"claude:claude-sonnet-5-5\", \"claude:claude-opus-5-5\"]\n",
    )
    .unwrap();
    let agents = cfg.agents;
    assert_eq!(agents.spawn_allowed_models.len(), 3);
    // No model at all means "use agents.swarm_model", which is configured.
    assert!(agents.validate_spawn_allowed_model(None).is_ok());
    assert!(agents.validate_spawn_allowed_model(Some("   ")).is_ok());
    for allowed in [
        "opencode-go:deepseek-v4.1-flash",
        "claude:claude-sonnet-5-5",
        "claude:claude-opus-5-5",
    ] {
        assert!(
            agents.validate_spawn_allowed_model(Some(allowed)).is_ok(),
            "{allowed}"
        );
        assert!(agents.worker_may_use_model(allowed), "{allowed}");
    }
    for denied in [
        "claude:claude-fable-5",
        "claude:claude-astra-5",
        "claude-fable-5",
        // A bare model id is not a configured entry: resolving it could pick a
        // provider the user never allowed.
        "claude-opus-5-5",
        // A provider swap for an allowed model is still outside the list.
        "openrouter:z-ai/glm-5.2",
    ] {
        let error = agents
            .validate_spawn_allowed_model(Some(denied))
            .expect_err(denied);
        assert!(error.contains(denied), "{error}");
        // The rejection must tell the agent what it may use instead.
        for allowed in &agents.spawn_allowed_models {
            assert!(error.contains(allowed), "{error} must list {allowed}");
        }
        assert!(!agents.worker_may_use_model(denied), "{denied}");
    }
}

#[test]
fn swarm_model_selection_defaults_off_even_with_an_allowlist() {
    let agents = Config::default().agents;
    assert!(!agents.swarm_model_selection);
    assert!(agents.swarm_model_allowlist.is_empty());
    assert!(agents.validate_swarm_model_request(None).is_ok());
    for model in [
        "inherit",
        "coordinator",
        "",
        "  ",
        "claude-api:claude-opus-4-8",
    ] {
        assert!(agents.validate_swarm_model_request(Some(model)).is_err());
    }
    let cfg: Config =
        toml::from_str("[agents]\nswarm_model_allowlist = [\"claude-api:claude-opus-4-8\"]\n")
            .unwrap();
    assert!(
        cfg.agents
            .validate_swarm_model_request(Some("claude-api:claude-opus-4-8"))
            .is_err()
    );
}

#[test]
fn swarm_model_selection_requires_opt_in_and_exact_route_match() {
    let cfg: Config = toml::from_str(
        "[agents]\nswarm_model_selection = true\nswarm_model_allowlist = [\"claude:claude-opus-4-8\"]\n"
    ).unwrap();
    assert!(cfg.agents.validate_swarm_model_request(None).is_ok());
    assert!(
        cfg.agents
            .validate_swarm_model_request(Some("claude:claude-opus-4-8"))
            .is_ok()
    );
    for model in [
        "claude-api:claude-opus-4-8",
        "claude-opus-4-8",
        "Claude:claude-opus-4-8",
        "claude:claude-opus-4-8 ",
        "inherit",
        "",
        "  ",
    ] {
        assert!(
            cfg.agents
                .validate_swarm_model_request(Some(model))
                .is_err(),
            "{model:?}"
        );
    }
    let cfg: Config = toml::from_str("[agents]\nswarm_model_selection = true\n").unwrap();
    assert!(
        cfg.agents
            .validate_swarm_model_request(Some("claude:claude-opus-4-8"))
            .is_err()
    );
}

#[test]
fn swarm_effort_parses_from_toml_and_env_override() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_SWARM_EFFORT");
    restore_env_var("JCODE_SWARM_EFFORT", None);

    // Public config-file interface (#1165).
    let cfg: Config =
        toml::from_str("[agents]\nswarm_model = \"claude-opus-5\"\nswarm_effort = \"medium\"\n")
            .expect("config with swarm_effort parses");
    assert_eq!(cfg.agents.swarm_effort.as_deref(), Some("medium"));
    assert_eq!(Config::default().agents.swarm_effort, None);

    crate::env::set_var("JCODE_SWARM_EFFORT", "low");
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(cfg.agents.swarm_effort.as_deref(), Some("low"));

    crate::env::set_var("JCODE_SWARM_EFFORT", " ");
    let mut cfg = Config::default();
    cfg.agents.swarm_effort = Some("preset".to_string());
    cfg.apply_env_overrides();
    assert_eq!(cfg.agents.swarm_effort, None);

    restore_env_var("JCODE_SWARM_EFFORT", prev);
}

#[test]
fn swarm_effort_caps_are_preserved_in_config() {
    let cfg: Config =
        toml::from_str("[agents]\nswarm_effort_caps = { \"openai:gpt-6.1-sol\" = \"xhigh\" }\n")
            .unwrap();
    let json = serde_json::to_value(&cfg.agents).unwrap();
    assert_eq!(json["swarm_effort_caps"]["openai:gpt-6.1-sol"], "xhigh");
}

#[test]
fn wake_mode_defaults_parses_and_env_overrides() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_WAKE_MODE");
    assert_eq!(
        Config::default().server.wake_mode,
        crate::config::WakeMode::Internal
    );
    let parsed: Config = toml::from_str("[server]\nwake_mode = \"external\"\n").unwrap();
    assert_eq!(parsed.server.wake_mode, crate::config::WakeMode::External);

    crate::env::set_var("JCODE_WAKE_MODE", "external");
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(cfg.server.wake_mode, crate::config::WakeMode::External);
    restore_env_var("JCODE_WAKE_MODE", prev);
}

#[test]
fn spawn_hook_defaults_to_none_and_parses_from_toml() {
    assert_eq!(Config::default().terminal.spawn_hook, None);

    let cfg: Config = toml::from_str("[terminal]\nspawn_hook = \"tmux new-window\"\n")
        .expect("spawn_hook should parse");
    assert_eq!(cfg.terminal.spawn_hook.as_deref(), Some("tmux new-window"));
}

#[test]
fn terminal_preferred_defaults_to_none_and_parses_from_toml() {
    assert_eq!(Config::default().terminal.preferred, None);

    let cfg: Config =
        toml::from_str("[terminal]\npreferred = \"ghostty\"\n").expect("preferred should parse");
    assert_eq!(cfg.terminal.preferred.as_deref(), Some("ghostty"));
}

#[test]
fn hooks_config_defaults_and_parses_from_toml() {
    let defaults = Config::default().hooks;
    assert_eq!(defaults.turn_start, None);
    assert_eq!(defaults.turn_end, None);
    assert_eq!(defaults.session_start, None);
    assert_eq!(defaults.session_end, None);
    assert_eq!(defaults.pre_tool, None);
    assert_eq!(defaults.post_tool, None);
    assert_eq!(defaults.pre_tool_timeout_ms, 5000);

    let cfg: Config = toml::from_str(
        "[hooks]\nturn_start = \"notify-start\"\nturn_end = \"notify-turn\"\npre_tool = \"~/bin/policy\"\npre_tool_timeout_ms = 1500\n",
    )
    .expect("hooks config should parse");
    assert_eq!(
        cfg.hooks.turn_start.as_ref().and_then(HookCommands::first),
        Some("notify-start")
    );
    assert_eq!(
        cfg.hooks.turn_end.as_ref().and_then(HookCommands::first),
        Some("notify-turn")
    );
    assert_eq!(
        cfg.hooks.pre_tool.as_ref().and_then(HookCommands::first),
        Some("~/bin/policy")
    );
    assert_eq!(cfg.hooks.pre_tool_timeout_ms, 1500);

    let cfg: Config = toml::from_str(
        "[hooks]\nturn_end = [\"notify-one --direct\", \"notify-two 'quoted arg'\"]\npre_tool = [\"policy-a\", \"policy-b\"]\n",
    )
    .expect("hook arrays should parse");
    assert_eq!(
        cfg.hooks
            .turn_end
            .as_ref()
            .expect("turn_end commands")
            .iter()
            .collect::<Vec<_>>(),
        vec!["notify-one --direct", "notify-two 'quoted arg'"]
    );
    assert_eq!(
        cfg.hooks
            .pre_tool
            .as_ref()
            .expect("pre_tool commands")
            .iter()
            .collect::<Vec<_>>(),
        vec!["policy-a", "policy-b"]
    );

    let serialized = toml::to_string(&cfg).expect("hook command arrays should serialize");
    let round_trip: Config = toml::from_str(&serialized).expect("serialized hooks should parse");
    assert_eq!(round_trip.hooks.turn_end, cfg.hooks.turn_end);
    assert_eq!(round_trip.hooks.pre_tool, cfg.hooks.pre_tool);
}

#[test]
fn test_env_override_lifecycle_hooks() {
    let _guard = crate::storage::lock_test_env();
    let prev_turn_end = std::env::var_os("JCODE_HOOK_TURN_END");
    let prev_timeout = std::env::var_os("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS");

    crate::env::set_var("JCODE_HOOK_TURN_END", "my-notifier --fast");
    crate::env::set_var("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS", "250");
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(
        cfg.hooks.turn_end.as_ref().and_then(HookCommands::first),
        Some("my-notifier --fast")
    );
    assert_eq!(cfg.hooks.pre_tool_timeout_ms, 250);

    // Empty env value disables a config-file hook.
    crate::env::set_var("JCODE_HOOK_TURN_END", " ");
    let mut cfg = Config::default();
    cfg.hooks.turn_end = Some(HookCommands::one("from-config"));
    cfg.apply_env_overrides();
    assert_eq!(cfg.hooks.turn_end, None);

    crate::env::set_var(
        "JCODE_HOOK_TURN_END",
        r#"["notify-one --direct", "notify-two 'quoted arg'"]"#,
    );
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(
        cfg.hooks.turn_end.unwrap().iter().collect::<Vec<_>>(),
        vec!["notify-one --direct", "notify-two 'quoted arg'"]
    );

    restore_env_var("JCODE_HOOK_TURN_END", prev_turn_end);
    restore_env_var("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS", prev_timeout);
}

#[test]
fn test_env_override_spawn_hook() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_SPAWN_HOOK");
    crate::env::set_var("JCODE_SPAWN_HOOK", "kitty @ launch --type=tab --");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(
        cfg.terminal.spawn_hook.as_deref(),
        Some("kitty @ launch --type=tab --")
    );

    // Empty env value disables a config-file hook.
    crate::env::set_var("JCODE_SPAWN_HOOK", "  ");
    let mut cfg = Config::default();
    cfg.terminal.spawn_hook = Some("tmux new-window".to_string());
    cfg.apply_env_overrides();
    assert_eq!(cfg.terminal.spawn_hook, None);

    restore_env_var("JCODE_SPAWN_HOOK", prev);
}

#[test]
fn test_env_override_focus_hook() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_FOCUS_HOOK");
    crate::env::set_var("JCODE_FOCUS_HOOK", "niri-focus-jcode");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(cfg.terminal.focus_hook.as_deref(), Some("niri-focus-jcode"));

    // Empty env value disables a config-file hook.
    crate::env::set_var("JCODE_FOCUS_HOOK", "");
    let mut cfg = Config::default();
    cfg.terminal.focus_hook = Some("wmctrl -a".to_string());
    cfg.apply_env_overrides();
    assert_eq!(cfg.terminal.focus_hook, None);

    restore_env_var("JCODE_FOCUS_HOOK", prev);
}

#[test]
fn test_memory_sidecar_enabled_defaults_true() {
    // The LLM precision-judge path is the only reliably productive memory mode,
    // so memory uses it by default. Users opt into the no-LLM hybrid path
    // explicitly by setting this false.
    let cfg = Config::default();
    assert!(cfg.agents.memory_sidecar_enabled);
}

#[test]
fn test_env_override_memory_sidecar() {
    let _guard = crate::storage::lock_test_env();
    let prev_model = std::env::var_os("JCODE_MEMORY_MODEL");
    let prev_enabled = std::env::var_os("JCODE_MEMORY_SIDECAR_ENABLED");
    crate::env::set_var("JCODE_MEMORY_MODEL", "claude-haiku-4");
    crate::env::set_var("JCODE_MEMORY_SIDECAR_ENABLED", "true");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert_eq!(cfg.agents.memory_model.as_deref(), Some("claude-haiku-4"));
    assert!(cfg.agents.memory_sidecar_enabled);

    restore_env_var("JCODE_MEMORY_MODEL", prev_model);
    restore_env_var("JCODE_MEMORY_SIDECAR_ENABLED", prev_enabled);
}

#[test]
fn tool_config_defaults_to_full_toolset() {
    let config = ToolConfig::default();
    let selection = config.selection();
    assert!(selection.allowed_tools.is_none());
    assert!(selection.disabled_tools.is_empty());
    assert_eq!(config.mcp_tools, McpToolsMode::Auto);
    assert_eq!(config.mcp_tools_token_threshold, 8_000);
}

#[test]
fn tool_config_deserializes_all_mcp_exposure_modes() {
    for (raw, expected) in [
        ("auto", McpToolsMode::Auto),
        ("eager", McpToolsMode::Eager),
        ("deferred", McpToolsMode::Deferred),
    ] {
        let config: Config = toml::from_str(&format!("[tools]\nmcp_tools = \"{raw}\"\n"))
            .expect("valid MCP exposure mode");
        assert_eq!(config.tools.mcp_tools, expected);
    }
}

#[test]
fn tool_config_mcp_exposure_env_overrides() {
    let _guard = crate::storage::lock_test_env();
    let previous_mode = std::env::var_os("JCODE_MCP_TOOLS");
    let previous_threshold = std::env::var_os("JCODE_MCP_TOOLS_TOKEN_THRESHOLD");
    crate::env::set_var("JCODE_MCP_TOOLS", "deferred");
    crate::env::set_var("JCODE_MCP_TOOLS_TOKEN_THRESHOLD", "4321");

    let mut config = Config::default();
    config.apply_env_overrides();

    assert_eq!(config.tools.mcp_tools, McpToolsMode::Deferred);
    assert_eq!(config.tools.mcp_tools_token_threshold, 4_321);
    restore_env_var("JCODE_MCP_TOOLS", previous_mode);
    restore_env_var("JCODE_MCP_TOOLS_TOKEN_THRESHOLD", previous_threshold);
}

#[test]
fn tool_config_explicit_enabled_uses_allow_list() {
    let cfg = ToolConfig {
        enabled: vec!["gmail".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();
    let allowed = selection
        .allowed_tools
        .expect("explicit enabled is an allow-list");

    assert!(allowed.contains("gmail"));
    assert!(!selection.disabled_tools.contains("gmail"));
}

#[test]
fn tool_config_all_enabled_sentinel_keeps_unrestricted_toolset() {
    let cfg = ToolConfig {
        enabled: vec!["*".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();

    assert!(selection.allowed_tools.is_none());
    assert!(!selection.disabled_tools.contains("gmail"));
}

#[test]
fn tool_config_explicit_disabled_overrides_all_enabled_sentinel() {
    let cfg = ToolConfig {
        enabled: vec!["*".to_string()],
        disabled: vec!["gmail".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();

    assert!(selection.allowed_tools.is_none());
    assert!(selection.disabled_tools.contains("gmail"));
}

#[test]
fn tool_config_acp_profile_is_the_full_toolset() {
    // Configs written before ACP parity still say `tool_profile = "acp"`.
    // That name must not restrict tools: ACP clients get everything the TUI
    // gets (todo, bg, swarm, skill_manage, web search, memory, ...).
    for profile in ["acp", "ACP", "full", ""] {
        let cfg = ToolConfig {
            profile: profile.to_string(),
            ..ToolConfig::default()
        };
        let selection = cfg.selection();
        assert!(
            selection.allowed_tools.is_none(),
            "profile {profile:?} must not be an allow-list"
        );
        assert!(selection.disabled_tools.is_empty());
    }
}

#[test]
fn tool_config_acp_profile_still_honors_explicit_lists() {
    let cfg = ToolConfig {
        profile: "acp".to_string(),
        disabled: vec!["swarm".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();
    assert!(selection.allowed_tools.is_none());
    assert!(selection.disabled_tools.contains("swarm"));
}

#[test]
fn acp_config_defaults_to_standard_profile_and_full_tools() {
    let cfg = Config::default();
    assert_eq!(cfg.acp.profile, "standard");
    assert_eq!(cfg.acp.tool_profile, "full");
}

#[test]
fn tool_config_minimal_profile_allows_core_coding_tools() {
    let cfg = ToolConfig {
        profile: "minimal".to_string(),
        ..ToolConfig::default()
    };
    let allowed = cfg
        .allowed_tools()
        .expect("minimal profile is an allow-list");

    assert!(allowed.contains("bash"));
    assert!(allowed.contains("read"));
    assert!(allowed.contains("write"));
    assert!(allowed.contains("apply_patch"));
    assert!(allowed.contains("agentgrep"));
    assert!(!allowed.contains("browser"));
    assert!(allowed.contains("edit"));
    assert!(!allowed.contains("replace"));
    assert!(!allowed.contains("swarm"));
}

#[test]
fn tool_config_explicit_enabled_and_disabled_lists_compose() {
    let cfg = ToolConfig {
        enabled: vec![
            "shell".to_string(),
            "read_file".to_string(),
            "browser".to_string(),
        ],
        disabled: vec!["browser".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();
    let allowed = selection
        .allowed_tools
        .expect("explicit enabled is an allow-list");

    assert!(allowed.contains("bash"));
    assert!(allowed.contains("read"));
    assert!(!allowed.contains("shell"));
    assert!(!allowed.contains("read_file"));
    assert!(!allowed.contains("browser"));
    assert!(selection.disabled_tools.contains("browser"));
}

#[test]
fn tool_config_none_profile_disables_all_tools() {
    let cfg = ToolConfig {
        profile: "none".to_string(),
        ..ToolConfig::default()
    };
    assert!(
        cfg.allowed_tools()
            .expect("none profile is empty")
            .is_empty()
    );
}

#[test]
fn tool_config_disabled_only_keeps_full_profile_with_deny_list() {
    let cfg = ToolConfig {
        disabled: vec!["browser".to_string(), "swarm".to_string()],
        ..ToolConfig::default()
    };
    let selection = cfg.selection();

    assert!(selection.allowed_tools.is_none());
    assert!(selection.disabled_tools.contains("browser"));
    assert!(selection.disabled_tools.contains("swarm"));
    assert!(!selection.disabled_tools.contains("gmail"));
}

#[test]
fn test_generated_default_config_has_expected_user_defaults() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    let path = Config::create_default_config_file().expect("create default config file");
    let content = std::fs::read_to_string(path).expect("read default config file");

    assert!(
        content.contains("openai_reasoning_effort = \"low\""),
        "generated default config should use low OpenAI reasoning effort"
    );
    assert!(
        content.contains("openai_service_tier = \"priority\""),
        "generated default config should enable OpenAI fast mode"
    );
    assert!(
        content.contains("[tools]") && content.contains("profile = \"full\""),
        "generated default config should document tool profiles"
    );
    assert!(
        content.contains("[acp]") && content.contains("tool_profile = \"full\""),
        "generated default config should document ACP profile settings"
    );
    assert!(
        content.contains("[agents]") && content.contains("swarm_spawn_mode = \"inline\""),
        "generated default config should document agent spawn defaults"
    );
    assert!(
        content.contains("memory_model = \"gpt-5.6-luna\"")
            && content.contains("reasoning effort \"none\""),
        "generated default config should document the Luna memory sidecar default"
    );

    // Effort keys come from the per-platform keybinding registry; the template
    // placeholders must always be substituted.
    assert!(
        !content.contains("@EFFORT_INCREASE@") && !content.contains("@EFFORT_DECREASE@"),
        "generated default config should substitute effort key placeholders"
    );
    let expected_increase = if cfg!(target_os = "macos") {
        "effort_increase = \"cmd+right\""
    } else {
        "effort_increase = \"alt+right\""
    };
    assert!(
        content.contains(expected_increase),
        "generated default config should use the platform effort_increase default"
    );

    // The generated file must always be valid TOML for the current Config schema.
    let parsed: Config =
        toml::from_str(&content).expect("generated default config should parse as Config");
    assert_eq!(parsed.agents.swarm_spawn_mode, SwarmSpawnMode::Inline);
    assert!(
        parsed.display.show_thinking,
        "freshly created user config should request model reasoning"
    );
    assert_eq!(
        parsed.display.reasoning_display(),
        jcode_config_types::ReasoningDisplayMode::Full,
        "freshly created user config should show the full reasoning trace"
    );

    if let Some(prev) = prev_home {
        crate::env::set_var("JCODE_HOME", prev);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[test]
fn global_config_cache_reloads_after_manual_file_edit() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
    Config::invalidate_cache();

    let path = Config::path().expect("config path");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("create config parent");
    std::fs::write(&path, "[display]\ncentered = false\n").expect("write initial config");

    assert!(!crate::config::config().display.centered);

    // Different length as well as mtime so the metadata fingerprint notices the
    // manual edit even on filesystems with coarse timestamp resolution.
    std::fs::write(&path, "[display]\ncentered = true\n# edited\n").expect("edit config");

    assert!(crate::config::config().display.centered);

    restore_env_var("JCODE_HOME", prev_home);
    Config::invalidate_cache();
}

#[test]
fn config_save_invalidates_global_config_cache() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
    Config::invalidate_cache();

    let mut cfg = Config::default();
    cfg.display.centered = false;
    cfg.save().expect("save initial config");
    assert!(!crate::config::config().display.centered);

    cfg.display.centered = true;
    cfg.save().expect("save updated config");
    assert!(crate::config::config().display.centered);

    restore_env_var("JCODE_HOME", prev_home);
    Config::invalidate_cache();
}

#[test]
fn config_env_fingerprint_ignores_runtime_only_jcode_vars() {
    let _guard = crate::storage::lock_test_env();
    let prev_runtime_provider = std::env::var_os("JCODE_RUNTIME_PROVIDER");
    let prev_active_provider = std::env::var_os("JCODE_ACTIVE_PROVIDER");
    let prev_display_centered = std::env::var_os("JCODE_DISPLAY_CENTERED");

    crate::env::remove_var("JCODE_RUNTIME_PROVIDER");
    crate::env::remove_var("JCODE_ACTIVE_PROVIDER");
    crate::env::remove_var("JCODE_DISPLAY_CENTERED");
    let baseline = config_env_fingerprint();

    crate::env::set_var("JCODE_RUNTIME_PROVIDER", "openai");
    crate::env::set_var("JCODE_ACTIVE_PROVIDER", "openai");
    assert_eq!(baseline, config_env_fingerprint());

    crate::env::set_var("JCODE_DISPLAY_CENTERED", "1");
    assert_ne!(baseline, config_env_fingerprint());

    restore_env_var("JCODE_RUNTIME_PROVIDER", prev_runtime_provider);
    restore_env_var("JCODE_ACTIVE_PROVIDER", prev_active_provider);
    restore_env_var("JCODE_DISPLAY_CENTERED", prev_display_centered);
}

// Changing the resume override must invalidate the cached configuration.
#[test]
fn config_cache_reloads_after_resume_open_elsewhere_changes() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let prev_resume = std::env::var_os("JCODE_RESUME_OPEN_ELSEWHERE");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
    crate::env::set_var("JCODE_RESUME_OPEN_ELSEWHERE", "off");
    Config::invalidate_cache();

    let before = crate::config::config().display.resume_open_elsewhere;
    crate::env::set_var("JCODE_RESUME_OPEN_ELSEWHERE", "replace");
    let after = crate::config::config().display.resume_open_elsewhere;

    // Restore process-wide state before checking the result, even on failure.
    restore_env_var("JCODE_HOME", prev_home);
    restore_env_var("JCODE_RESUME_OPEN_ELSEWHERE", prev_resume);
    Config::invalidate_cache();

    assert_eq!(before, jcode_config_types::ResumeOpenElsewhere::Off);
    assert_eq!(after, jcode_config_types::ResumeOpenElsewhere::Replace);
}

#[test]
fn config_env_fingerprint_tracks_every_apply_env_override_var() {
    let override_source = include_str!("config/env_overrides.rs");
    let mut missing = Vec::new();

    for line in override_source.lines() {
        let Some(start) = line.find("std::env::var(\"") else {
            continue;
        };
        let rest = &line[start + "std::env::var(\"".len()..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let key = &rest[..end];
        if !crate::config::CONFIG_ENV_KEYS.contains(&key) {
            missing.push(key.to_string());
        }
    }

    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "CONFIG_ENV_KEYS must include every env var read by Config::apply_env_overrides; missing: {missing:?}"
    );
}

#[test]
fn cached_external_auth_trust_observes_manual_revocation() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
    Config::invalidate_cache();

    let auth_file = dir.path().join("external-auth.json");
    std::fs::write(&auth_file, "{}\n").expect("write external auth file");
    crate::auth::external_trust::allow_source_for_path("test_source", &auth_file)
        .expect("trust external auth path");
    assert!(crate::auth::external_trust::source_allowed_for_path_cached(
        "test_source",
        &auth_file
    ));

    let path = Config::path().expect("config path");
    std::fs::write(
        &path,
        "[auth]\ntrusted_external_source_paths = []\n# manually revoked\n",
    )
    .expect("manually revoke external auth trust");

    assert!(
        !crate::auth::external_trust::source_allowed_for_path_cached("test_source", &auth_file)
    );

    restore_env_var("JCODE_HOME", prev_home);
    Config::invalidate_cache();
}

#[test]
fn test_ambient_visible_defaults_to_true() {
    assert!(AmbientConfig::default().visible);
}

#[test]
fn test_display_auto_server_reload_defaults_to_true() {
    assert!(DisplayConfig::default().auto_server_reload);
}

#[test]
fn test_display_alignment_defaults_to_left() {
    assert!(!DisplayConfig::default().centered);
}

#[test]
fn display_emoji_defaults_on_and_deserializes_off() {
    assert!(DisplayConfig::default().emoji);
    let cfg: Config = toml::from_str("[display]\nemoji = false\n").expect("config parses");
    assert!(!cfg.display.emoji);
}

#[test]
fn test_provider_failover_defaults_match_new_behavior() {
    let provider = Config::default().provider;
    assert_eq!(
        provider.cross_provider_failover,
        super::CrossProviderFailoverMode::Countdown
    );
    assert!(provider.same_provider_account_failover);
    assert!(provider.openai_auto_banked_reset);
    assert!(!provider.anthropic_auto_fable_reset);
}

#[test]
fn automatic_banked_reset_defaults_on_and_supports_explicit_opt_out() {
    let cfg: Config = toml::from_str("[provider]\nopenai_auto_banked_reset = false\n").unwrap();
    assert!(!cfg.provider.openai_auto_banked_reset);
    let cfg: Config = toml::from_str("[provider]\n").unwrap();
    assert!(cfg.provider.openai_auto_banked_reset);
    assert!(cfg.provider.auto_reset_only_when_no_routes);
    assert!(!cfg.provider.anthropic_auto_fable_reset);
    let cfg: Config = toml::from_str("[provider]\nanthropic_auto_fable_reset = true\n").unwrap();
    assert!(cfg.provider.anthropic_auto_fable_reset);
    assert!(cfg.provider.openai_auto_banked_reset);
}

#[test]
fn automatic_reset_routes_first_by_default_and_can_be_overridden() {
    let cfg: Config = toml::from_str("[provider]\n").unwrap();
    assert!(cfg.provider.auto_reset_only_when_no_routes);
    assert!(ProviderConfig::default().auto_reset_only_when_no_routes);
    let cfg: Config = toml::from_str(
        "[provider]\nopenai_auto_banked_reset = true\nanthropic_auto_fable_reset = true\nauto_reset_only_when_no_routes = false\n",
    ).unwrap();
    assert!(!cfg.provider.auto_reset_only_when_no_routes);
    assert!(cfg.provider.openai_auto_banked_reset);
    assert!(cfg.provider.anthropic_auto_fable_reset);
}

#[test]
fn test_provider_failover_disabled_aliases_parse_as_manual() {
    for value in ["off", "false", "disabled", "none"] {
        let cfg: Config = toml::from_str(&format!(
            "[provider]\ncross_provider_failover = \"{value}\"\n"
        ))
        .unwrap_or_else(|error| panic!("{value} should parse: {error}"));
        assert_eq!(
            cfg.provider.cross_provider_failover,
            super::CrossProviderFailoverMode::Manual
        );
        assert_eq!(
            super::CrossProviderFailoverMode::parse(value),
            Some(super::CrossProviderFailoverMode::Manual)
        );
    }
}

#[test]
fn test_native_scrollbars_default_to_enabled() {
    let display = DisplayConfig::default();
    assert!(display.native_scrollbars.chat);
    assert!(display.native_scrollbars.side_panel);
}

#[test]
fn test_copy_badge_alt_label_defaults_to_auto_and_deserializes() {
    assert!(DisplayConfig::default().copy_badge_alt_label.is_empty());

    let cfg: Config = toml::from_str(
        r#"
        [display]
        copy_badge_alt_label = "Option"
        "#,
    )
    .expect("config should deserialize");

    assert_eq!(cfg.display.copy_badge_alt_label, "Option");
}

#[test]
fn test_session_picker_resume_action_defaults_to_current_terminal() {
    assert_eq!(
        Config::default().keybindings.session_picker_enter,
        SessionPickerResumeAction::CurrentTerminal
    );
    assert_eq!(
        SessionPickerResumeAction::CurrentTerminal.alternate(),
        SessionPickerResumeAction::NewTerminal
    );
}

#[test]
fn test_session_picker_resume_action_deserializes_kebab_case() {
    let cfg: Config = toml::from_str(
        r#"
        [keybindings]
        session_picker_enter = "current-terminal"
        "#,
    )
    .expect("config should deserialize");

    assert_eq!(
        cfg.keybindings.session_picker_enter,
        SessionPickerResumeAction::CurrentTerminal
    );
}

#[test]
fn test_env_override_auto_server_reload() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_AUTO_SERVER_RELOAD");
    crate::env::set_var("JCODE_AUTO_SERVER_RELOAD", "false");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert!(!cfg.display.auto_server_reload);

    if let Some(prev) = prev {
        crate::env::set_var("JCODE_AUTO_SERVER_RELOAD", prev);
    } else {
        crate::env::remove_var("JCODE_AUTO_SERVER_RELOAD");
    }
}

#[test]
fn no_emoji_environment_override_disables_emoji() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_NO_EMOJI");
    crate::env::set_var("JCODE_NO_EMOJI", "1");
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert!(!cfg.display.emoji);

    crate::env::set_var("JCODE_NO_EMOJI", "false");
    cfg.display.emoji = false;
    cfg.apply_env_overrides();
    assert!(cfg.display.emoji);

    restore_env_var("JCODE_NO_EMOJI", prev);
}

#[test]
fn test_env_override_native_scrollbars() {
    let _guard = crate::storage::lock_test_env();
    let prev_chat = std::env::var_os("JCODE_CHAT_NATIVE_SCROLLBAR");
    let prev_side = std::env::var_os("JCODE_SIDE_PANEL_NATIVE_SCROLLBAR");
    crate::env::set_var("JCODE_CHAT_NATIVE_SCROLLBAR", "true");
    crate::env::set_var("JCODE_SIDE_PANEL_NATIVE_SCROLLBAR", "false");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert!(cfg.display.native_scrollbars.chat);
    assert!(!cfg.display.native_scrollbars.side_panel);

    if let Some(prev) = prev_chat {
        crate::env::set_var("JCODE_CHAT_NATIVE_SCROLLBAR", prev);
    } else {
        crate::env::remove_var("JCODE_CHAT_NATIVE_SCROLLBAR");
    }
    if let Some(prev) = prev_side {
        crate::env::set_var("JCODE_SIDE_PANEL_NATIVE_SCROLLBAR", prev);
    } else {
        crate::env::remove_var("JCODE_SIDE_PANEL_NATIVE_SCROLLBAR");
    }
}

#[test]
fn test_removed_pinned_diff_mode_falls_back_inline() {
    let cfg: Config = toml::from_str(
        "[display]\ndiff_mode = 'pinned'\ndiff_line_wrap = false\ncentered = true\n",
    )
    .expect("legacy pinned diff settings must not invalidate the config");
    assert_eq!(cfg.display.diff_mode, DiffDisplayMode::Inline);
    assert!(cfg.display.centered, "unrelated settings must survive");
}

#[test]
fn test_env_override_removed_pinned_diff_mode_is_ignored() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_DIFF_MODE");
    for removed in ["pinned", "pin"] {
        crate::env::set_var("JCODE_DIFF_MODE", removed);
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.display.diff_mode, DiffDisplayMode::Inline);

        cfg.display.diff_mode = DiffDisplayMode::File;
        cfg.apply_env_overrides();
        assert_eq!(cfg.display.diff_mode, DiffDisplayMode::File);
    }
    restore_env_var("JCODE_DIFF_MODE", prev);
}

#[test]
fn test_env_override_diff_mode_full_inline() {
    let _guard = crate::storage::lock_test_env();
    let prev = std::env::var_os("JCODE_DIFF_MODE");
    crate::env::set_var("JCODE_DIFF_MODE", "full-inline");

    let mut cfg = Config::default();
    cfg.apply_env_overrides();

    assert_eq!(cfg.display.diff_mode, DiffDisplayMode::FullInline);

    if let Some(prev) = prev {
        crate::env::set_var("JCODE_DIFF_MODE", prev);
    } else {
        crate::env::remove_var("JCODE_DIFF_MODE");
    }
}

#[path = "config_preference_tests.rs"]
mod preferences;
#[path = "config_reload_pin_tests.rs"]
mod reload_pins;

#[test]
fn populate_context_limits_from_config_ref_seeds_global_cache() {
    use super::{NamedProviderConfig, NamedProviderModelConfig};

    // Regression test for issue #366: a named OpenAI-compatible provider with a
    // per-model `context_window` must be honored by the global context-limit
    // resolution path, not just the provider instance's own context_window().
    let model_id = "issue366-custom-gateway-model";
    let mut cfg = Config::default();
    cfg.providers.insert(
        "issue366-gateway".to_string(),
        NamedProviderConfig {
            base_url: "https://gateway.example.test/v1".to_string(),
            models: vec![NamedProviderModelConfig {
                id: model_id.to_string(),
                reasoning: None,
                reasoning_effort: None,
                context_window: Some(1_000_000),
                input: Vec::new(),
            }],
            ..Default::default()
        },
    );

    populate_context_limits_from_config_ref(&cfg);

    assert_eq!(
        crate::provider::context_limit_for_model(model_id),
        Some(1_000_000),
        "global context-limit resolution should respect named provider context_window"
    );
}

#[test]
fn populate_context_limits_from_config_seeds_qualified_runtime_model_shapes() {
    use super::{NamedProviderConfig, NamedProviderModelConfig};

    // Regression test for issue #421: the runtime request model can be
    // provider-qualified (`cachyai-a2000:qwen...`) or a slash path served by
    // llama.cpp (`ornith-box-1:/opt/models/ornith-1.0-35b-Q4_K_M.gguf`). The
    // configured context_window must resolve for every shape, not just the
    // bare id, otherwise budgeting falls back to the 200K default and
    // over-sends context.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "issue421-gateway".to_string(),
        NamedProviderConfig {
            base_url: "http://10.15.15.53:8080/v1".to_string(),
            models: vec![
                NamedProviderModelConfig {
                    id: "issue421-qwen-128k".to_string(),
                    reasoning: None,
                    reasoning_effort: None,
                    context_window: Some(131_072),
                    input: Vec::new(),
                },
                NamedProviderModelConfig {
                    id: "/opt/models/issue421-ornith-35b-q4.gguf".to_string(),
                    reasoning: None,
                    reasoning_effort: None,
                    context_window: Some(131_072),
                    input: Vec::new(),
                },
            ],
            ..Default::default()
        },
    );

    populate_context_limits_from_config_ref(&cfg);

    // Bare id.
    assert_eq!(
        crate::provider::context_limit_for_model("issue421-qwen-128k"),
        Some(131_072)
    );
    // Profile-qualified spec, as persisted by session restore.
    assert_eq!(
        crate::provider::context_limit_for_model("issue421-gateway:issue421-qwen-128k"),
        Some(131_072),
        "profile-qualified model spec must resolve the configured context_window"
    );
    // Slash-path model id: the lookup reduces to the slash base.
    assert_eq!(
        crate::provider::context_limit_for_model("/opt/models/issue421-ornith-35b-q4.gguf"),
        Some(131_072),
        "slash-path model id must resolve the configured context_window"
    );
    // Profile-qualified slash-path spec, exactly as reported in issue #421.
    assert_eq!(
        crate::provider::context_limit_for_model(
            "issue421-gateway:/opt/models/issue421-ornith-35b-q4.gguf"
        ),
        Some(131_072),
        "profile-qualified slash-path spec must resolve the configured context_window"
    );
}

#[test]
fn migrate_legacy_swarm_spawn_mode_flips_visible_to_inline_once() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    let config_path = dir.path().join("config.toml");
    let original = "[display]\ncentered = true\n\n[agents]\nswarm_spawn_mode = \"visible\"\nswarm_max_concurrent_agents = 32\n";
    std::fs::write(&config_path, original).expect("write config");

    assert!(
        Config::migrate_legacy_swarm_spawn_mode_once(),
        "migration should rewrite a legacy visible spawn mode"
    );
    let migrated = std::fs::read_to_string(&config_path).expect("read config");
    assert!(
        migrated.contains("swarm_spawn_mode = \"inline\""),
        "spawn mode should be flipped to inline: {migrated}"
    );
    // The rest of the file is untouched.
    assert!(migrated.contains("centered = true"));
    assert!(migrated.contains("swarm_max_concurrent_agents = 32"));
    let parsed: Config = toml::from_str(&migrated).expect("migrated config parses");
    assert_eq!(parsed.agents.swarm_spawn_mode, SwarmSpawnMode::Inline);

    // Marker written: a later explicit "visible" survives future launches.
    std::fs::write(&config_path, "[agents]\nswarm_spawn_mode = \"visible\"\n")
        .expect("write config");
    assert!(
        !Config::migrate_legacy_swarm_spawn_mode_once(),
        "migration must run at most once"
    );
    let content = std::fs::read_to_string(&config_path).expect("read config");
    assert!(content.contains("swarm_spawn_mode = \"visible\""));

    restore_env_var("JCODE_HOME", prev_home);
}

#[test]
fn migrate_legacy_swarm_spawn_mode_noops_without_visible_value() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    // No config file at all: no migration, but the marker is written.
    assert!(!Config::migrate_legacy_swarm_spawn_mode_once());
    assert!(
        dir.path()
            .join("migrations")
            .join("swarm-spawn-mode-inline")
            .exists(),
        "marker should be written even when there is nothing to migrate"
    );

    // Explicit non-visible values are never rewritten (marker already set,
    // but check the matcher too with a fresh home).
    let dir2 = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir2.path());
    let config_path = dir2.path().join("config.toml");
    std::fs::write(&config_path, "[agents]\nswarm_spawn_mode = \"headless\"\n")
        .expect("write config");
    assert!(!Config::migrate_legacy_swarm_spawn_mode_once());
    let content = std::fs::read_to_string(&config_path).expect("read config");
    assert!(content.contains("swarm_spawn_mode = \"headless\""));

    restore_env_var("JCODE_HOME", prev_home);
}

#[test]
fn migrate_idle_animation_off_flips_true_to_false_once() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    let config_path = dir.path().join("config.toml");
    let original = "[display]\ncentered = true\nidle_animation = true\nanimation_fps = 60\n";
    std::fs::write(&config_path, original).expect("write config");

    assert!(
        Config::migrate_idle_animation_off_once(),
        "migration should rewrite an enabled idle animation"
    );
    let migrated = std::fs::read_to_string(&config_path).expect("read config");
    assert!(
        migrated.contains("idle_animation = false"),
        "idle animation should be flipped off: {migrated}"
    );
    // The rest of the file is untouched.
    assert!(migrated.contains("centered = true"));
    assert!(migrated.contains("animation_fps = 60"));
    let parsed: Config = toml::from_str(&migrated).expect("migrated config parses");
    assert!(!parsed.display.idle_animation);

    // Marker written: a later explicit re-enable survives future launches.
    std::fs::write(&config_path, "[display]\nidle_animation = true\n").expect("write config");
    assert!(
        !Config::migrate_idle_animation_off_once(),
        "migration must run at most once"
    );
    let content = std::fs::read_to_string(&config_path).expect("read config");
    assert!(content.contains("idle_animation = true"));

    restore_env_var("JCODE_HOME", prev_home);
}

#[test]
fn migrate_idle_animation_off_noops_without_enabled_value() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    // No config file at all: no migration, but the marker is written.
    assert!(!Config::migrate_idle_animation_off_once());
    assert!(
        dir.path()
            .join("migrations")
            .join("idle-animation-off")
            .exists(),
        "marker should be written even when there is nothing to migrate"
    );

    // Already-false values are never rewritten (fresh home to bypass marker).
    let dir2 = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir2.path());
    let config_path = dir2.path().join("config.toml");
    let original = "[display]\nidle_animation = false\n";
    std::fs::write(&config_path, original).expect("write config");
    assert!(!Config::migrate_idle_animation_off_once());
    let content = std::fs::read_to_string(&config_path).expect("read config");
    assert_eq!(content, original);

    restore_env_var("JCODE_HOME", prev_home);
}

/// The removed `[sponsors]` section must not break loading of older config files.
#[test]
fn legacy_sponsors_section_is_ignored() {
    let config: Config =
        toml::from_str("[sponsors]\nenabled = true\nendpoint = 'https://example.com'\n")
            .expect("legacy [sponsors] table must still parse");
    let rendered = toml::to_string_pretty(&config).expect("serialize");
    assert!(!rendered.contains("[sponsors]"));
}

#[test]
fn config_reload_generation_increments_on_cache_invalidation() {
    let before = crate::config::config_reload_generation();
    crate::config::invalidate_config_cache();
    let after = crate::config::config_reload_generation();
    assert!(
        after > before,
        "invalidate_config_cache must bump the reload generation ({before} -> {after})"
    );
}

#[test]
fn swarm_root_effort_config_defaults_and_validation() {
    let defaults = Config::default();
    assert_eq!(defaults.agents.root_effort_for_swarm(), "low");
    let cfg: Config =
        toml::from_str("[agents]\nswarm_root_effort = ' High '\nswarm_effort = 'medium'\n")
            .unwrap();
    assert_eq!(cfg.agents.root_effort_for_swarm(), "high");
    assert_eq!(cfg.agents.swarm_effort.as_deref(), Some("medium"));
    assert!(cfg.display_string().contains("Swarm root effort: high"));
    assert!(!cfg.display_string().contains("Deep swarm"));
    let serialized = toml::to_string(&cfg).unwrap();
    let round_trip: Config = toml::from_str(&serialized).unwrap();
    assert_eq!(round_trip.agents.root_effort_for_swarm(), "high");

    for level in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let cfg: Config =
            toml::from_str(&format!("[agents]\nswarm_root_effort = '{level}'")).unwrap();
        assert_eq!(cfg.agents.root_effort_for_swarm(), level);
    }
    for invalid in ["", "swarm", "swarm-deep", "turbo"] {
        let cfg: Config = toml::from_str(&format!(
            "[agents]\nswarm_root_effort = '{invalid}'\nswarm_effort = 'low'"
        ))
        .unwrap();
        assert_eq!(cfg.agents.root_effort_for_swarm(), "low");
        assert_eq!(cfg.agents.swarm_effort.as_deref(), Some("low"));
    }
    // Legacy configs that still set the removed deep key keep parsing.
    let legacy: Config = toml::from_str("[agents]\nswarm_deep_root_effort = 'high'\n").unwrap();
    assert_eq!(legacy.agents.root_effort_for_swarm(), "low");
}

#[test]
fn swarm_root_effort_env_overrides_and_shared_resolution() {
    let _guard = crate::storage::lock_test_env();
    let key = "JCODE_SWARM_ROOT_EFFORT";
    let previous = std::env::var_os(key);
    let fingerprint = config_env_fingerprint();
    crate::env::set_var(key, "high");
    assert_ne!(config_env_fingerprint(), fingerprint);
    let mut cfg = Config::default();
    cfg.apply_env_overrides();
    assert_eq!(cfg.agents.root_effort_for_swarm(), "high");
    assert_eq!(crate::prompt::swarm_root_reasoning_effort("low"), None);
    assert_eq!(
        crate::prompt::swarm_root_reasoning_effort("swarm-deep"),
        None
    );
    crate::env::set_var(key, "none");
    assert_eq!(
        crate::prompt::swarm_root_reasoning_effort("swarm"),
        Some("none")
    );
    crate::env::set_var(key, " ");
    cfg.apply_env_overrides();
    assert_eq!(cfg.agents.swarm_root_effort, None);
    restore_env_var(key, previous);
}

#[test]
fn anthropic_cache_preference_persists_and_preserves_other_settings() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().unwrap();
    crate::env::set_var("JCODE_HOME", dir.path());
    Config::invalidate_cache();
    let path = Config::path().unwrap();
    std::fs::write(&path, "[provider]\ndefault_model = 'keep-me'\n").unwrap();
    assert!(crate::config::config().provider.anthropic_cache_ttl_1h);
    for enabled in [false, true] {
        Config::set_anthropic_cache_ttl_1h(enabled).unwrap();
        Config::invalidate_cache();
        assert_eq!(Config::load().provider.anthropic_cache_ttl_1h, enabled);
        assert_eq!(crate::provider::anthropic::is_cache_ttl_1h(), enabled);
        assert_eq!(
            crate::config::config().provider.anthropic_cache_ttl_1h,
            enabled
        );
        assert_eq!(
            Config::load().provider.default_model.as_deref(),
            Some("keep-me")
        );
    }
    std::fs::write(&path, "[broken").unwrap();
    assert!(Config::set_anthropic_cache_ttl_1h(false).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
    restore_env_var("JCODE_HOME", prev_home);
    Config::invalidate_cache();
}

#[test]
fn cli_config_save_round_trips_desktop_tables() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
    Config::invalidate_cache();

    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[display]\ncentered = false\n\n[desktop.voice]\nglobal_hold = true\n\
         global_devices = [\"/dev/input/event3\"]\n\n[desktop.appearance]\ntheme = \"warm-neutral\"\n",
    )
    .unwrap();

    // A CLI read-modify-write must not drop Desktop-owned settings.
    Config::set_default_model(Some("gpt-test"), None).expect("save");
    let saved: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let desktop = saved["desktop"].as_table().expect("desktop table kept");
    assert_eq!(desktop["voice"]["global_hold"].as_bool(), Some(true));
    assert_eq!(
        desktop["voice"]["global_devices"][0].as_str(),
        Some("/dev/input/event3")
    );
    assert_eq!(
        desktop["appearance"]["theme"].as_str(),
        Some("warm-neutral")
    );

    // Configs without Desktop tables stay free of an empty [desktop] header.
    std::fs::write(&path, "[display]\ncentered = false\n").unwrap();
    Config::set_default_model(Some("gpt-test"), None).expect("save");
    assert!(!std::fs::read_to_string(&path).unwrap().contains("[desktop"));

    restore_env_var("JCODE_HOME", prev_home);
    Config::invalidate_cache();
}

#[test]
fn global_voice_devices_fall_back_to_desktop_selection() {
    let desktop: Config =
        toml::from_str("[desktop.voice]\nglobal_devices = [\"/dev/input/event3\", \"\"]\n")
            .unwrap();
    assert_eq!(
        desktop.global_voice_devices(),
        vec![std::path::PathBuf::from("/dev/input/event3")]
    );

    let explicit: Config = toml::from_str(
        "[dictation]\nglobal_devices = [\"/dev/input/event7\"]\n\n\
         [desktop.voice]\nglobal_devices = [\"/dev/input/event3\"]\n",
    )
    .unwrap();
    assert_eq!(
        explicit.global_voice_devices(),
        vec![std::path::PathBuf::from("/dev/input/event7")]
    );

    assert!(Config::default().global_voice_devices().is_empty());
}

#[path = "config_tests/default_provider.rs"]
mod default_provider;

/// Voice transcription lands in the composer for editing, never in the chat.
/// `send` stays available, but only when a user asks for it explicitly.
#[test]
fn dictation_mode_defaults_to_insert_and_parses_every_mode() {
    use crate::protocol::TranscriptMode;
    assert_eq!(Config::default().dictation.mode, TranscriptMode::Insert);
    for (name, want) in [
        ("insert", TranscriptMode::Insert),
        ("append", TranscriptMode::Append),
        ("replace", TranscriptMode::Replace),
        ("send", TranscriptMode::Send),
    ] {
        let parsed: Config = toml::from_str(&format!("[dictation]\nmode = \"{name}\"\n")).unwrap();
        assert_eq!(parsed.dictation.mode, want);
    }
}

#[test]
fn dictation_backend_defaults_to_auto_and_parses() {
    use super::DictationBackend;
    let config = Config::default();
    assert_eq!(config.dictation.backend, DictationBackend::Auto);
    assert!(config.dictation.base_url.is_empty());
    assert!(
        config.dictation.model.is_empty(),
        "empty means the backend default"
    );
    // A command may run as long as a dictation can, not just 90 seconds.
    assert_eq!(config.dictation.timeout_secs, 300);
    let parsed: Config = toml::from_str(
        "[dictation]\nbackend = \"soniox\"\nmodel = \"stt-rt-v6\"\nbase_url = \"ws://127.0.0.1:9/x\"\n",
    )
    .unwrap();
    assert_eq!(parsed.dictation.model, "stt-rt-v6");
    assert_eq!(parsed.dictation.backend, DictationBackend::Soniox);
    assert_eq!(parsed.dictation.base_url, "ws://127.0.0.1:9/x");
    for (name, want) in [
        ("nari", DictationBackend::Nari),
        ("subscription", DictationBackend::Subscription),
        ("command", DictationBackend::Command),
        ("elevenlabs", DictationBackend::ElevenLabs),
    ] {
        let parsed: Config =
            toml::from_str(&format!("[dictation]\nbackend = \"{name}\"\n")).unwrap();
        assert_eq!(parsed.dictation.backend, want);
    }
}

/// `display.overscroll_status` was removed; configs that still set it (users
/// had `"on"` written by older versions) must load without losing settings.
#[test]
fn removed_overscroll_status_key_still_loads_config() {
    let config = toml::from_str::<Config>(
        "[display]\ncentered = true\noverscroll_status = \"on\"\nusage_display = \"used\"\n",
    )
    .expect("legacy key must not break config");
    assert!(config.display.centered);
    assert_eq!(config.display.usage_display, "used");
}
#[test]
fn tool_auto_background_timeout_configuration() {
    assert_eq!(ToolConfig::default().auto_background_timeout_ms, 3_000);
    for contents in ["", "[tools]\n"] {
        let config: Config = toml::from_str(contents).unwrap();
        assert_eq!(config.tools.auto_background_timeout_ms, 3_000);
    }
    for timeout in [0_u64, 1, 1250, 60_000, u64::MAX / 2] {
        let config: Config = toml::from_str(&format!(
            "[tools]\nauto_background_timeout_ms = {timeout}\n"
        ))
        .unwrap();
        assert_eq!(config.tools.auto_background_timeout_ms, timeout);
        let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(roundtrip.tools.auto_background_timeout_ms, timeout);
    }
    for invalid in ["-1", "1.5", "\"3000\"", "true"] {
        assert!(
            toml::from_str::<Config>(&format!(
                "[tools]\nauto_background_timeout_ms = {invalid}\n"
            ))
            .is_err()
        );
    }
}
#[test]
fn browser_handoff_defaults_off_and_roundtrips() {
    for contents in ["", "[tools]\n"] {
        let config: Config = toml::from_str(contents).unwrap();
        assert!(!config.tools.browser_handoff);
    }
    for enabled in [false, true] {
        let config: Config =
            toml::from_str(&format!("[tools]\nbrowser_handoff = {enabled}\n")).unwrap();
        assert_eq!(config.tools.browser_handoff, enabled);
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.tools.browser_handoff, enabled);
    }
}

#[test]
fn routing_and_egress_sections_roundtrip_and_stay_out_of_default_saves() {
    let empty = toml::to_string(&Config::default()).unwrap();
    assert!(!empty.contains("[routing"), "{empty}");
    assert!(!empty.contains("[egress"), "{empty}");

    let config: Config = toml::from_str(
        r#"
        [egress.corp]
        url = "http://127.0.0.1:8899"

        [routing]
        mode = "shadow"

        [[routing.accounts]]
        id = "claude:team"
        priority = { fable = 1, default = 2 }
        egress = "corp"
        "#,
    )
    .unwrap();
    let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(restored.routing, config.routing);
    assert_eq!(restored.egress, config.egress);
    let policy =
        jcode_routing_policy::Policy::from_config(&restored.routing, &restored.egress).unwrap();
    assert_eq!(policy.accounts[0].egress.as_deref(), Some("corp"));
}

#[test]
fn invalid_routing_section_does_not_break_config_loading() {
    let config: Config = toml::from_str(
        "[provider]\ndefault_provider = \"claude\"\n[[routing.accounts]]\nid = \"claude:a\"\negress = \"missing\"\n",
    )
    .unwrap();
    assert_eq!(config.provider.default_provider.as_deref(), Some("claude"));
    assert!(jcode_routing_policy::Policy::from_config(&config.routing, &config.egress).is_err());
}

#[path = "config_tests_additional_tests.rs"]
mod additional_hibernation_environment_override_accepts_seconds_and_zero_but_not_invalid_values;

// Retired routing and cache keys still load: they are ignored and named in a
// note, never a parse error.
#[test]
fn retired_routing_and_cache_keys_still_load() {
    let config = toml::from_str::<Config>(
        r#"
        [cache]
        allow_human_wait = true
        max_pings_per_wait = 3

        [routing]
        mode = "auto"
        reserve_floor_pct = 10

        [[routing.accounts]]
        id = "claude:team"
        drain_before_reset_h = 12
        drain_limit_pct = 98

        [[routing.accounts]]
        id = "openai:codex"
        kind = "credits"
        pacing = "month-line"
        pacing_slack_pct = 2

        [[routing.budgets]]
        name = "codex-month"
        account = "openai:codex"
        meter = "ledger"
        window = "calendar-month"
        unit = "usd"
        limit = 600
        "#,
    )
    .expect("a config with retired keys loads");
    let policy = jcode_routing_policy::Policy::from_config(&config.routing, &config.egress)
        .expect("the routing section stays valid");
    assert_eq!(
        policy.budgets[0].window,
        jcode_routing_policy::Window::CALENDAR_MONTH
    );
    let notes = config.routing.retired_keys();
    assert_eq!(notes.len(), 1, "one warning names every key: {notes:?}");
    assert!(notes[0].contains("reserve_floor_pct") && notes[0].contains("pacing_slack_pct"));
    let cache = config.cache.retired_keys();
    assert_eq!(cache.len(), 2, "{cache:?}");
    assert!(
        cache
            .iter()
            .any(|note| note.contains("allow_human_wait") && note.contains("swarm"))
    );
    assert!(
        cache
            .iter()
            .any(|note| note.contains("max_pings_per_wait") && note.contains("rebuild"))
    );
    // Retired keys are never written back.
    let text = toml::to_string(&config).unwrap();
    assert!(!text.contains("drain_before_reset_h") && !text.contains("max_pings_per_wait"));
}

#[test]
fn agents_jev_models_parse_and_default_unset() {
    let config: AgentsConfig = toml::from_str(
        r#"
memory_jev_model = "typesafe/jev-1.14"
browser_jev_model = "typesafe/jev-1.14"
voice_jev_model = "typesafe/jev-1.14"
"#,
    )
    .unwrap();
    assert_eq!(
        config.memory_jev_model.as_deref(),
        Some("typesafe/jev-1.14")
    );
    assert_eq!(
        config.browser_jev_model.as_deref(),
        Some("typesafe/jev-1.14")
    );
    assert_eq!(config.voice_jev_model.as_deref(), Some("typesafe/jev-1.14"));
    let defaults = AgentsConfig::default();
    assert!(defaults.memory_jev_model.is_none());
    assert!(defaults.browser_jev_model.is_none());
    assert!(defaults.voice_jev_model.is_none());
}

#[test]
fn display_string_lists_every_section_once_in_order() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    crate::env::set_var("JCODE_HOME", home.path());
    let text = Config::default().display_string();
    crate::env::remove_var("JCODE_HOME");
    let headers = [
        "**Configuration**",
        "**Keybindings:**",
        "**Dictation:**",
        "**Display:**",
        "**Features:**",
        "**Tools:**",
        "**Provider:**",
        "**Agent models:**",
        "**Gateway:**",
        "**Ambient:**",
        "**Notifications:**",
        "*Edit the config file",
    ];
    let mut cursor = 0;
    for header in headers {
        let found = text[cursor..]
            .find(header)
            .unwrap_or_else(|| panic!("{header} missing or out of order"));
        cursor += found + header.len();
        assert_eq!(text.matches(header).count(), 1, "{header}");
    }
}

#[path = "config_tests/dictation_timeout.rs"]
mod dictation_timeout;

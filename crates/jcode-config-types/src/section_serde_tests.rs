//! Pins the user-facing TOML key names of the config sections that other
//! modules mutate through typed methods, so reorganising them cannot rename a
//! key a person has written in `config.toml`.

use super::{AgentsConfig, DisplayConfig};
use std::collections::BTreeSet;

fn keys<T: serde::Serialize>(value: &T) -> BTreeSet<String> {
    serde_json::to_value(value)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| name.to_string()).collect()
}

#[test]
fn display_section_keeps_its_key_names() {
    // `colors` is omitted while empty so a bare `[display.colors]` header never
    // gets written.
    let expected = names(&[
        "diff_mode",
        "show_diffs",
        "queue_mode",
        "auto_server_reload",
        "mouse_capture",
        "debug_socket",
        "emoji",
        "centered",
        "show_thinking",
        "compact_thinking",
        "reasoning_display",
        "diagram_mode",
        "markdown_spacing",
        "show_link_urls",
        "latex_rendering",
        "pin_images",
        "pin_todos",
        "todo_header",
        "idle_animation",
        "prompt_entry_animation",
        "disabled_animations",
        "performance",
        "animation_fps",
        "redraw_fps",
        "prompt_preview",
        "compact_notifications",
        "copy_badge_alt_label",
        "show_agentgrep_output",
        "show_bash_output",
        "tool_call_details",
        "native_scrollbars",
        "keybinding_hints",
        "theme",
        "active_sessions_manager",
        "external_sessions",
        "resume_project_scope",
        "resume_recent_close_seconds",
        "resume_time",
        "resume_open_elsewhere",
        "usage_display",
    ]);
    assert_eq!(keys(&DisplayConfig::default()), expected);
}

#[test]
fn agents_section_keeps_its_key_names() {
    let expected = names(&[
        "swarm_model",
        "swarm_model_selection",
        "swarm_model_allowlist",
        "spawn_allowed_models",
        "swarm_model_fallbacks",
        "swarm_effort",
        "swarm_effort_caps",
        "swarm_speed",
        "swarm_speed_caps",
        "swarm_pool",
        "swarm_root_effort",
        "swarm_spawn_mode",
        "memory_jev_provider",
        "memory_auto_recall",
        "memory_jev_model",
        "browser_jev_model",
        "voice_jev_model",
        "memory_jev_threshold",
        "memory_model",
        "title_model",
        "memory_sidecar_enabled",
        "memory_rerank_cadence",
        "memory_rerank_votes",
        "memory_rerank_min_agree",
        "memory_embedding_backend",
        "memory_embedding_model",
        "memory_embedding_base_url",
        "memory_embedding_dim",
        "swarm_max_concurrent_agents",
        "swarm_stall_after_secs",
        "swarm_stall_nudge",
        "root_profile",
        "profiles",
        "model_prompts",
        "advisor",
    ]);
    assert_eq!(keys(&AgentsConfig::default()), expected);
}

#[test]
fn sections_survive_a_toml_round_trip() {
    let display = DisplayConfig::default();
    let again: DisplayConfig = toml::from_str(&toml::to_string(&display).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&display).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    let agents = AgentsConfig::default();
    let again: AgentsConfig = toml::from_str(&toml::to_string(&agents).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&agents).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
}

#[test]
fn display_setters_keep_the_legacy_todo_pin_in_sync() {
    let mut display = DisplayConfig::default();
    display.set_todo_header_mode(crate::TodoHeaderMode::Hidden);
    assert!(!display.pin_todos);
    display.set_todo_header_mode(crate::TodoHeaderMode::Minimal);
    assert!(display.pin_todos);
    assert_eq!(display.todo_header_mode(), crate::TodoHeaderMode::Minimal);
}

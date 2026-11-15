//! The model picker's second step: reasoning effort.
//!
//! `/model` chooses a model once. When that route has reasoning levels, Enter
//! opens a second list limited to the levels the route accepts, and the choice
//! applies the model and the level together. Esc steps back to the model list
//! before it closes anything.
//!
//! The level list is built from the same provider inference the effort pill
//! and the Desktop picker use, so the picker never offers a level the provider
//! rejects. Rows are ordinary model entries with `effort` set, so the existing
//! switch path (route selection, remote dispatch, telemetry) applies them.

use jcode_provider_core::inferred_reasoning_efforts;

use super::inline_interactive::placeholder_routes::route_supports_reasoning_effort;
use crate::tui::{PickerAction, PickerEntry, PickerOption};

/// Levels this route accepts, weakest first, without the swarm rungs.
///
/// Swarm is an orchestration mode, not a provider reasoning level, so it is not
/// a per-model choice.
pub(super) fn route_efforts(model: &str, api_method: &str) -> Vec<&'static str> {
    if !route_supports_reasoning_effort(api_method) {
        return Vec::new();
    }
    inferred_reasoning_efforts(Some(api_method), Some(model))
        .into_iter()
        .filter(|effort| !crate::prompt::is_swarm_mode_effort(effort))
        .collect()
}

/// Whether Enter on this row opens the effort step instead of switching.
///
/// Only a plain model row steps; an effort row already carries its level, and
/// the `/agents` role pickers store a model spec that has no effort field.
pub(super) fn opens_effort_step(entry: &PickerEntry) -> bool {
    if !matches!(entry.action, PickerAction::Model) || entry.effort.is_some() {
        return false;
    }
    entry
        .active_option()
        .is_some_and(|route| !route_efforts(&entry.name, &route.api_method).is_empty())
}

/// The row title for a level, e.g. `Claude Opus 5.5 (◒ high)`.
///
/// The parent name is reused verbatim so the filtered text of step two still
/// matches the model the user just chose; the ramp glyph leads the level so
/// the step reads as the same ladder everywhere else in the UI.
pub(super) fn effort_entry_name(model: &str, effort: &str) -> String {
    let label = match effort {
        "xhigh" => "xhigh",
        "max" => "max",
        "high" => "high",
        "medium" => "med",
        "low" => "low",
        "none" => "none",
        other => other,
    };
    match jcode_provider_core::reasoning_effort_glyph(effort) {
        Some(glyph) => format!("{model} ({glyph} {label})"),
        None => format!("{model} ({label})"),
    }
}

/// Step-two rows for one model row: the same route, one row per level.
///
/// The row keeps `is_current` only when the session already runs this model at
/// this level, so the highlight tells the truth about what is in use.
pub(super) fn effort_entries(
    parent: &PickerEntry,
    current_model: &str,
    current_effort: Option<&str>,
) -> Vec<PickerEntry> {
    let Some(route) = parent.active_option() else {
        return Vec::new();
    };
    let on_current_model = parent.name == current_model;
    route_efforts(&parent.name, &route.api_method)
        .into_iter()
        .map(|effort| PickerEntry {
            name: effort_entry_name(&parent.name, effort),
            options: vec![PickerOption {
                provider: route.provider.clone(),
                api_method: route.api_method.clone(),
                available: route.available,
                detail: route.detail.clone(),
                estimated_reference_cost_micros: route.estimated_reference_cost_micros,
            }],
            action: PickerAction::Model,
            selected_option: 0,
            is_current: on_current_model && current_effort == Some(effort),
            is_default: false,
            is_favorite: false,
            // A level is not a recommendation: "high" of one model is not more
            // recommended than "high" of another.
            recommended: false,
            recommendation_rank: usize::MAX,
            usage_score: 0,
            old: false,
            created_date: None,
            effort: Some(effort.to_string()),
        })
        .collect()
}

/// The model name a row stands for, without its ` (level)` suffix.
pub(super) fn model_name_without_effort(name: &str) -> &str {
    match name.rsplit_once(" (") {
        Some((model, suffix)) if suffix.ends_with(')') => model,
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_entry(name: &str, api_method: &str) -> PickerEntry {
        PickerEntry {
            name: name.to_string(),
            options: vec![PickerOption {
                provider: "OpenAI".to_string(),
                api_method: api_method.to_string(),
                available: true,
                detail: String::new(),
                estimated_reference_cost_micros: None,
            }],
            action: PickerAction::Model,
            selected_option: 0,
            is_current: false,
            is_default: false,
            is_favorite: false,
            recommended: false,
            recommendation_rank: usize::MAX,
            usage_score: 0,
            old: false,
            created_date: None,
            effort: None,
        }
    }

    #[test]
    fn a_model_row_with_levels_opens_the_effort_step() {
        let openai = model_entry("gpt-5.3-codex", "openai-oauth");
        assert!(opens_effort_step(&openai));
        // A provider without effort control switches in one step.
        assert!(!opens_effort_step(&model_entry(
            "llama3",
            "openai-compatible"
        )));
        // An effort row is already a choice.
        let mut chosen = openai.clone();
        chosen.effort = Some("high".into());
        assert!(!opens_effort_step(&chosen));
        // A model with no route details cannot be checked.
        let mut empty = openai.clone();
        empty.options.clear();
        assert!(!opens_effort_step(&empty));
    }

    #[test]
    fn the_effort_step_lists_only_what_the_route_accepts() {
        let levels = route_efforts("gpt-5.3-codex", "openai-oauth");
        assert!(
            levels.contains(&"high") && levels.contains(&"minimal"),
            "{levels:?}"
        );
        assert!(
            !levels.iter().any(|level| level.starts_with("swarm")),
            "swarm is an orchestration mode, not a model level: {levels:?}"
        );
        let claude = route_efforts("claude-opus-5-5", "claude-oauth");
        assert!(!claude.contains(&"minimal"), "{claude:?}");
        assert!(route_efforts("llama3", "openai-compatible").is_empty());
    }

    #[test]
    fn step_two_rows_keep_the_chosen_route_and_mark_the_running_level() {
        let parent = model_entry("gpt-5.3-codex", "openai-oauth");
        let rows = effort_entries(&parent, "gpt-5.3-codex", Some("high"));
        assert_eq!(
            rows.len(),
            route_efforts("gpt-5.3-codex", "openai-oauth").len()
        );
        let current: Vec<_> = rows.iter().filter(|row| row.is_current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].effort.as_deref(), Some("high"));
        for row in &rows {
            let route = row.active_option().expect("route");
            assert_eq!(route.provider, "OpenAI");
            assert_eq!(route.api_method, "openai-oauth");
            assert!(row.effort.is_some());
            assert_eq!(model_name_without_effort(&row.name), "gpt-5.3-codex");
        }
        // The same model on a different session level marks nothing as current.
        assert!(
            effort_entries(&parent, "gpt-5.3-codex", Some("nonesuch"))
                .iter()
                .all(|row| !row.is_current)
        );
        assert!(
            effort_entries(&parent, "claude-opus-5-5", Some("high"))
                .iter()
                .all(|row| !row.is_current)
        );
    }

    #[test]
    fn level_rows_read_as_glyph_and_level() {
        assert_eq!(
            effort_entry_name("gpt-5.6-sol", "medium"),
            "gpt-5.6-sol (◑ med)"
        );
        assert_eq!(
            effort_entry_name("gpt-5.6-sol", "xhigh"),
            "gpt-5.6-sol (◕ xhigh)"
        );
        assert_eq!(
            model_name_without_effort("gpt-5.6-sol (◑ med)"),
            "gpt-5.6-sol"
        );
        assert_eq!(model_name_without_effort("gpt-5.6-sol"), "gpt-5.6-sol");
    }
}

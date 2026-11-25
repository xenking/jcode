#[path = "inline_interactive/cache_signature.rs"]
mod cache_signature;
use super::*;
#[path = "inline_interactive_model_picker.rs"]
mod model_picker;

use crate::tui::session_picker::{self, OverlayAction, PickerResult, ResumeTarget, SessionPicker};
use crate::tui::{
    AccountPickerAction, InlineInteractiveState, PickerAction, PickerEntry, PickerKind,
    PickerOption,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "inline_interactive/helpers.rs"]
mod helpers;
#[path = "inline_interactive/openers.rs"]
mod openers;
#[path = "inline_interactive/preview.rs"]
mod preview;
#[path = "inline_interactive/preview_request.rs"]
mod preview_request;
use helpers::{
    agent_model_default_summary, agent_model_target_label, agent_model_target_slug,
    catchup_candidates, catchup_queue_position, model_entry_base_name, model_entry_saved_spec,
    openrouter_route_model_id, picker_route_model_spec, picker_route_selection,
    save_agent_model_override,
};

pub(super) fn subagent_picker_model_spec(entry: &PickerEntry) -> String {
    model_entry_saved_spec(entry)
}

const REMOTE_MODEL_CATALOG_CACHE_FILE: &str = "remote_model_catalog_cache.json";
const REMOTE_MODEL_CATALOG_CACHE_VERSION: u8 = 3;
const REMOTE_MODEL_CATALOG_CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;
const REMOTE_MODEL_CATALOG_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const REMOTE_MODEL_CATALOG_MAX_ROUTES: usize = 10_000;
const REMOTE_MODEL_CATALOG_MAX_MODEL_BYTES: usize = 4 * 1024;
const REMOTE_MODEL_CATALOG_MAX_PROVIDER_BYTES: usize = 512;
const REMOTE_MODEL_CATALOG_MAX_DETAIL_BYTES: usize = 16 * 1024;
const MODEL_PICKER_USAGE_FILE: &str = "model_picker_usage.json";
const MODEL_PICKER_USAGE_VERSION: u8 = 1;
const MODEL_PICKER_FAVORITES_FILE: &str = "model_picker_favorites.json";
const MODEL_PICKER_FAVORITES_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteModelCatalogCache {
    version: u8,
    #[serde(default)]
    origin: String,
    #[serde(flatten)]
    snapshot: jcode_provider_core::ModelCatalogSnapshot,
    observed_at_unix_secs: u64,
}

/// Result of applying an incoming remote model-catalog snapshot.
#[derive(Debug, Clone, Copy)]
pub(super) struct CatalogReplaceOutcome {
    /// Provider name/model identity changed (terminal title needs refresh).
    pub(super) provider_meta_changed: bool,
    /// Any catalog content changed. When false the snapshot was an exact
    /// duplicate and no cache invalidation, persistence, or redraw is needed.
    pub(super) catalog_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelPickerUsageEntry {
    count: u32,
    last_selected_unix_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelPickerUsageStore {
    version: u8,
    selections: HashMap<String, ModelPickerUsageEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelPickerFavoritesStore {
    version: u8,
    favorites: HashSet<String>,
}

fn model_picker_usage_path() -> Option<std::path::PathBuf> {
    if crate::tui::is_ssh_remote() {
        return None;
    }
    crate::storage::app_config_dir()
        .ok()
        .map(|dir| dir.join(MODEL_PICKER_USAGE_FILE))
}

fn model_picker_favorites_path() -> Option<std::path::PathBuf> {
    if crate::tui::is_ssh_remote() {
        return None;
    }
    crate::storage::app_config_dir()
        .ok()
        .map(|dir| dir.join(MODEL_PICKER_FAVORITES_FILE))
}

#[path = "inline_interactive_placeholder_routes.rs"]
pub(super) mod placeholder_routes;
#[cfg(test)]
use placeholder_routes::route_supports_reasoning_effort;

/// Apply the `provider.model_picker_providers` allowlist (issue #460).
///
/// Each allowlist entry can name a provider label ("openai", "llama.cpp",
/// "anthropic"), a route api method ("claude-oauth", "openrouter",
/// "openai-compatible:myprofile"), or a bare openai-compatible profile id
/// ("myprofile"). Matching is case/format-insensitive via the shared provider
/// label normalizer. Routes for the active model are always kept so the
/// current selection never disappears from the picker, but only when their
/// provider and API method match the active route. A filter that
/// matches nothing falls back to the unfiltered list instead of an empty
/// picker.
fn filter_routes_by_provider_allowlist(
    routes: Vec<crate::provider::ModelRoute>,
    allowlist: Option<&[String]>,
    current_model: &str,
    current_provider: &str,
    current_api_method: Option<&str>,
) -> Vec<crate::provider::ModelRoute> {
    use crate::provider::normalize_model_route_provider_label as normalize;

    let Some(allowlist) = allowlist else {
        return routes;
    };
    let allowed: Vec<String> = allowlist
        .iter()
        .map(|entry| normalize(entry))
        .filter(|entry| !entry.is_empty())
        .collect();
    if allowed.is_empty() {
        return routes;
    }

    let route_matches = |route: &crate::provider::ModelRoute| -> bool {
        let provider = normalize(&route.provider);
        let api_method = normalize(&route.api_method);
        // "openai-compatible:myprofile" normalizes to "openaicompatible:myprofile";
        // also expose the bare profile id for convenience.
        let profile_id = route
            .api_method
            .split_once(':')
            .map(|(_, profile)| normalize(profile))
            .unwrap_or_default();
        let account_base_method = route
            .api_method
            .split_once('@')
            .filter(|_| jcode_provider_core::oauth_account_label(&route.api_method).is_some())
            .map(|(method, _)| normalize(method));
        allowed.iter().any(|entry| {
            *entry == provider
                || *entry == api_method
                || account_base_method
                    .as_ref()
                    .is_some_and(|method| method == entry)
                || (!profile_id.is_empty() && *entry == profile_id)
                || crate::provider::model_route_provider_labels_match(&route.provider, entry)
        })
    };

    let route_is_current = |route: &crate::provider::ModelRoute| {
        route.model == current_model
            && crate::provider::model_route_provider_labels_match(&route.provider, current_provider)
            && current_api_method.is_none_or(|api_method| {
                crate::provider::ModelRouteApiMethod::parse(&route.api_method)
                    == crate::provider::ModelRouteApiMethod::parse(api_method)
                    && jcode_provider_core::oauth_account_label(&route.api_method)
                        == jcode_provider_core::oauth_account_label(api_method)
            })
    };
    let filtered: Vec<crate::provider::ModelRoute> = routes
        .iter()
        .filter(|route| route_is_current(route) || route_matches(route))
        .cloned()
        .collect();
    if filtered.is_empty() {
        routes
    } else {
        filtered
    }
}

/// Leave out routes the user hid in `[desktop.models] hidden`. The Desktop
/// picker's "Manage models" view writes that list into the shared config, so
/// one setting hides a model in every client.
///
/// The running model always stays visible: hiding what the session already uses
/// must not take away the row that says so. An empty result keeps the catalog,
/// so hiding everything cannot leave the user without a picker.
fn filter_routes_by_hidden_models(
    routes: Vec<crate::provider::ModelRoute>,
    hidden: &[String],
    current_model: &str,
) -> Vec<crate::provider::ModelRoute> {
    if hidden.is_empty() {
        return routes;
    }
    // Matched on the model id alone: the picker lists one row per model, so a
    // provider/route comparison would let a hidden entry drop the row anyway.
    let route_is_current = |route: &crate::provider::ModelRoute| route.model == current_model;
    let filtered: Vec<crate::provider::ModelRoute> = routes
        .iter()
        .filter(|route| {
            route_is_current(route)
                || !jcode_provider_core::picker_hidden::route_is_hidden(hidden, route)
        })
        .cloned()
        .collect();
    if filtered.is_empty() {
        routes
    } else {
        filtered
    }
}

/// Provider group order, shared with the Desktop picker: provider name, then a
/// signed-in account route before an API-key one, then the route id. Entries
/// without route metadata trail everything.
fn model_picker_group_sort_key(entry: &PickerEntry) -> (bool, String, u8, String) {
    use crate::provider::ModelRouteApiMethod as Method;
    let Some(route) = entry.active_option() else {
        return (true, String::new(), 0, String::new());
    };
    let auth = match Method::parse(&route.api_method) {
        Method::ClaudeOAuth
        | Method::OpenAIOAuth
        | Method::CodeAssistOAuth
        | Method::Copilot
        | Method::Cursor
        | Method::AntigravityHttps
        | Method::GrokBuild => 0,
        Method::JcodeSubscription => 1,
        Method::AnthropicApiKey
        | Method::OpenAIApiKey
        | Method::OpenRouter
        | Method::OpenAiCompatible { .. } => 2,
        Method::Bedrock => 3,
        Method::RemoteCatalog | Method::Current | Method::Other(_) => 4,
    };
    let provider = route.provider.to_ascii_lowercase();
    let id = format!("{provider} · {}", route.api_method.to_ascii_lowercase());
    (false, provider, auth, id)
}

fn model_picker_usage_key(model_name: &str, route: &PickerOption, effort: Option<&str>) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        model_name,
        route.provider,
        route.api_method,
        effort.unwrap_or("")
    )
}

fn load_model_picker_usage_store() -> ModelPickerUsageStore {
    let Some(path) = model_picker_usage_path() else {
        return ModelPickerUsageStore::default();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return ModelPickerUsageStore::default();
    };
    let Ok(mut store) = serde_json::from_slice::<ModelPickerUsageStore>(&bytes) else {
        return ModelPickerUsageStore::default();
    };
    if store.version != MODEL_PICKER_USAGE_VERSION {
        return ModelPickerUsageStore::default();
    }
    store.selections.retain(|_, entry| entry.count > 0);
    store
}

fn save_model_picker_usage_store(store: &ModelPickerUsageStore) {
    let Some(path) = model_picker_usage_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(store) {
        let _ = std::fs::write(path, json);
    }
}

fn model_picker_usage_score(
    store: &ModelPickerUsageStore,
    model_name: &str,
    route: &PickerOption,
    effort: Option<&str>,
) -> u32 {
    store
        .selections
        .get(&model_picker_usage_key(model_name, route, effort))
        .map(|entry| entry.count.saturating_mul(100).saturating_add(50))
        .unwrap_or(0)
}

fn record_model_picker_selection(model_name: &str, route: &PickerOption, effort: Option<&str>) {
    let mut store = load_model_picker_usage_store();
    store.version = MODEL_PICKER_USAGE_VERSION;
    let key = model_picker_usage_key(model_name, route, effort);
    let entry = store.selections.entry(key).or_default();
    entry.count = entry.count.saturating_add(1);
    entry.last_selected_unix_secs = remote_model_catalog_observed_at_unix_secs();
    save_model_picker_usage_store(&store);
}

fn load_model_picker_favorites_store() -> ModelPickerFavoritesStore {
    let Some(path) = model_picker_favorites_path() else {
        return ModelPickerFavoritesStore::default();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return ModelPickerFavoritesStore::default();
    };
    let Ok(mut store) = serde_json::from_slice::<ModelPickerFavoritesStore>(&bytes) else {
        return ModelPickerFavoritesStore::default();
    };
    if store.version != MODEL_PICKER_FAVORITES_VERSION {
        return ModelPickerFavoritesStore::default();
    }
    store.favorites.retain(|key| !key.trim().is_empty());
    store
}

fn save_model_picker_favorites_store(store: &ModelPickerFavoritesStore) {
    let Some(path) = model_picker_favorites_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(store) {
        let _ = std::fs::write(path, json);
    }
}

fn model_picker_is_favorite(
    store: &ModelPickerFavoritesStore,
    model_name: &str,
    route: &PickerOption,
    effort: Option<&str>,
) -> bool {
    store
        .favorites
        .contains(&model_picker_usage_key(model_name, route, effort))
}

/// Return the filtered position of the favorite after the current model in a
/// stable identity order. The runtime picker itself is sorted with the current
/// model first, so using its row order after reopening the picker can only
/// alternate between the current model and the highest-ranked other favorite.
fn next_model_favorite_after_current(picker: &InlineInteractiveState) -> Option<usize> {
    let mut favorites: Vec<(String, usize, bool)> = picker
        .filtered
        .iter()
        .enumerate()
        .filter_map(|(filtered_pos, &entry_idx)| {
            let entry = picker.entries.get(entry_idx)?;
            if !entry.is_favorite || !matches!(entry.action, PickerAction::Model) {
                return None;
            }
            let route = entry.active_option()?;
            Some((
                model_picker_usage_key(
                    &model_entry_base_name(entry),
                    route,
                    entry.effort.as_deref(),
                ),
                filtered_pos,
                entry.is_current,
            ))
        })
        .collect();
    favorites.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let next = favorites
        .iter()
        .position(|(_, _, is_current)| *is_current)
        .map(|current| (current + 1) % favorites.len())
        .unwrap_or(0);
    favorites
        .get(next)
        .map(|(_, filtered_pos, _)| *filtered_pos)
}

fn picker_is_runtime_model_picker(picker: &InlineInteractiveState) -> bool {
    picker.kind == PickerKind::Model
        && picker
            .entries
            .iter()
            .any(|entry| matches!(entry.action, PickerAction::Model))
}

/// Which `/agents` view is open, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentModelsPickerView {
    /// The role list (swarm, review, judge, memory, ambient).
    Targets,
    /// The model list for one role.
    Models(crate::tui::AgentModelTarget),
}

pub(crate) fn agent_models_picker_view(
    picker: &InlineInteractiveState,
) -> Option<AgentModelsPickerView> {
    if picker.kind != PickerKind::Model {
        return None;
    }
    if picker.is_agent_target_picker() {
        return Some(AgentModelsPickerView::Targets);
    }
    picker.entries.iter().find_map(|entry| match entry.action {
        PickerAction::AgentModelChoice { target, .. } => {
            Some(AgentModelsPickerView::Models(target))
        }
        _ => None,
    })
}

fn key_char_eq_ignore_ascii_case(code: KeyCode, expected: char) -> bool {
    matches!(code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected))
}

fn remote_model_catalog_cache_path() -> Option<std::path::PathBuf> {
    if crate::tui::is_ssh_remote() {
        return None;
    }
    crate::storage::app_config_dir()
        .ok()
        .map(|dir| dir.join(REMOTE_MODEL_CATALOG_CACHE_FILE))
}

fn remote_model_catalog_cache_origin() -> String {
    crate::server::socket_path().to_string_lossy().into_owned()
}

fn remote_catalog_text_is_safe(value: &str, max_bytes: usize, allow_empty: bool) -> bool {
    value.len() <= max_bytes
        && (allow_empty || !value.trim().is_empty())
        && !value.chars().any(char::is_control)
}

fn remote_catalog_api_method_is_safe(api_method: &str) -> bool {
    use crate::provider::ModelRouteApiMethod as Method;
    match Method::parse(api_method) {
        Method::Other(_) | Method::Current => false,
        Method::OpenAiCompatible {
            profile_id: Some(profile_id),
        } => {
            profile_id.len() <= 128
                && profile_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        }
        _ => true,
    }
}

fn remote_model_catalog_snapshot_is_safe(
    snapshot: &jcode_provider_core::ModelCatalogSnapshot,
) -> bool {
    if snapshot.available_models.len() > REMOTE_MODEL_CATALOG_MAX_ROUTES
        || snapshot.model_routes.len() > REMOTE_MODEL_CATALOG_MAX_ROUTES
    {
        return false;
    }
    if snapshot.provider_name.as_deref().is_some_and(|value| {
        !remote_catalog_text_is_safe(value, REMOTE_MODEL_CATALOG_MAX_PROVIDER_BYTES, false)
    }) || snapshot.provider_model.as_deref().is_some_and(|value| {
        !remote_catalog_text_is_safe(value, REMOTE_MODEL_CATALOG_MAX_MODEL_BYTES, false)
    }) {
        return false;
    }
    if snapshot.available_models.iter().any(|model| {
        !remote_catalog_text_is_safe(model, REMOTE_MODEL_CATALOG_MAX_MODEL_BYTES, false)
    }) {
        return false;
    }
    snapshot.model_routes.iter().all(|route| {
        remote_catalog_text_is_safe(&route.model, REMOTE_MODEL_CATALOG_MAX_MODEL_BYTES, false)
            && remote_catalog_text_is_safe(
                &route.provider,
                REMOTE_MODEL_CATALOG_MAX_PROVIDER_BYTES,
                false,
            )
            && remote_catalog_text_is_safe(
                &route.api_method,
                REMOTE_MODEL_CATALOG_MAX_PROVIDER_BYTES,
                false,
            )
            && remote_catalog_text_is_safe(
                &route.detail,
                REMOTE_MODEL_CATALOG_MAX_DETAIL_BYTES,
                true,
            )
            && remote_catalog_api_method_is_safe(&route.api_method)
    })
}

fn remote_model_catalog_cache_is_fresh(cache: &RemoteModelCatalogCache, now: u64) -> bool {
    cache.observed_at_unix_secs <= now.saturating_add(5 * 60)
        && now.saturating_sub(cache.observed_at_unix_secs)
            <= REMOTE_MODEL_CATALOG_CACHE_MAX_AGE_SECS
}

fn remote_model_catalog_observed_at_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn model_picker_route_is_current(
    model_name: &str,
    route: &PickerOption,
    current_model: &str,
    current_provider: &str,
    current_api_method: Option<&str>,
) -> bool {
    if model_name != current_model {
        return false;
    }
    // Remote sessions whose catalog arrives as names-only do not carry a
    // provider name, so `current_provider` is the generic "remote"
    // placeholder. The model name was synthesized into a real provider route
    // (Copilot/OpenAI/...), so a provider-label comparison would never match
    // and the current model would not preselect. Fall back to name-only
    // matching in that case.
    if current_provider.trim().eq_ignore_ascii_case("remote") {
        return current_api_method
            .map(|method| route.api_method.eq_ignore_ascii_case(method))
            .unwrap_or(true);
    }
    jcode_provider_core::model_route_provider_labels_match(&route.provider, current_provider)
        && current_api_method
            .map(|method| route.api_method.eq_ignore_ascii_case(method))
            .unwrap_or(true)
}

const RECOMMENDED_MODELS: &[&str] = &[
    jcode_provider_core::DEFAULT_OPENAI_MODEL,
    jcode_provider_core::DEFAULT_CLAUDE_MODEL,
];

fn model_picker_recommendation_rank(name: &str) -> usize {
    RECOMMENDED_MODELS
        .iter()
        .position(|model| *model == name)
        .unwrap_or(usize::MAX)
}

fn model_picker_route_is_recommended(model_name: &str, route: &PickerOption) -> bool {
    RECOMMENDED_MODELS.contains(&model_name)
        && jcode_provider_core::model_route_metadata_is_recommended(
            model_name,
            &route.provider,
            &route.api_method,
            route.available,
        )
}

fn model_picker_provider_hint_from_model_spec(model_spec: &str) -> Option<(&str, &str)> {
    let (provider_hint, bare_model) = model_spec.split_once(':')?;
    let provider_hint = provider_hint.trim();
    let bare_model = bare_model.trim();
    if provider_hint.is_empty() || bare_model.is_empty() {
        return None;
    }

    let normalized = provider_hint.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "claude"
            | "anthropic"
            | "openai"
            | "copilot"
            | "cursor"
            | "antigravity"
            | "bedrock"
            | "openrouter"
            | "gemini"
    ) || crate::provider_catalog::openai_compatible_profile_by_id(provider_hint).is_some()
    {
        Some((provider_hint, bare_model))
    } else {
        None
    }
}

fn model_picker_route_provider_matches_key(
    route_provider_key: Option<&str>,
    route_provider_label: &str,
    desired_provider: &str,
) -> bool {
    jcode_provider_core::model_route_provider_matches_key(
        route_provider_key,
        route_provider_label,
        desired_provider,
    )
}

/// Whether an effort-qualified picker entry matches the persisted reasoning
/// effort for its route's provider family. Entries without an effort always
/// match. When no effort is persisted for the family, every variant matches
/// (legacy model-level behavior) so we never hide the `default` marker
/// entirely (issue #675).
fn model_picker_effort_matches_default(
    provider_key: Option<&str>,
    entry_effort: Option<&str>,
    anthropic_effort: Option<&str>,
    openai_effort: Option<&str>,
) -> bool {
    let Some(effort) = entry_effort else {
        return true;
    };
    let stored = match provider_key {
        Some("claude-oauth") | Some("claude-api") => anthropic_effort,
        Some("openai-oauth") | Some("openai-api") => openai_effort,
        _ => None,
    };
    match stored {
        Some(stored) => stored.eq_ignore_ascii_case(effort),
        None => true,
    }
}

fn model_picker_route_is_default(
    model_name: &str,
    route: &PickerOption,
    config_default_model: Option<&str>,
    config_default_provider: Option<&str>,
) -> bool {
    let Some(default_model) = config_default_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    let selection = crate::provider::MultiProvider::default_model_selection_from_route(
        model_name,
        &route.api_method,
        &route.provider,
    );
    if default_model
        .split_once(':')
        .is_some_and(|(method, _)| jcode_provider_core::oauth_account_label(method).is_some())
    {
        return default_model == selection.model_spec;
    }
    let provider_matches = |provider: &str| {
        model_picker_route_provider_matches_key(
            selection.provider_key.as_deref(),
            &route.provider,
            provider,
        )
    };

    let model_matches_bare_or_exact = default_model == selection.model_spec
        || default_model == model_name
        || model_picker_provider_hint_from_model_spec(default_model)
            .map(|(_, bare_model)| bare_model == model_name)
            .unwrap_or(false);

    if let Some(default_provider) = config_default_provider
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return model_matches_bare_or_exact && provider_matches(default_provider);
    }

    if default_model == selection.model_spec {
        return true;
    }

    if let Some((provider_hint, bare_model)) =
        model_picker_provider_hint_from_model_spec(default_model)
    {
        return bare_model == model_name && provider_matches(provider_hint);
    }

    if let Some((bare_model, provider_label)) = default_model.rsplit_once('@') {
        return bare_model == model_name
            && jcode_provider_core::model_route_provider_labels_match(
                &route.provider,
                provider_label,
            );
    }

    // Legacy configs may only contain a bare model. In that case the persisted
    // data cannot identify the route, so keep the previous model-only marker.
    default_model == model_name
}

#[path = "inline_interactive/interactive_key.rs"]
mod interactive_key;

#[path = "inline_interactive/session_picker_flow.rs"]
mod session_picker_flow;

#[path = "inline_interactive/model_picker_entries.rs"]
mod model_picker_entries;

#[path = "inline_interactive/model_catalog.rs"]
mod model_catalog;

impl App {
    pub(super) fn open_model_picker(&mut self) {
        self.open_model_picker_inner(false);
    }

    fn open_model_picker_preserving_input(&mut self) {
        self.open_model_picker_inner(true);
    }

    /// Rebuild an already-open `/model` picker after a fresh catalog arrives.
    ///
    /// Catalog updates only invalidate the picker cache, so a picker opened on
    /// placeholder ("remote-catalog") rows would keep showing them until the
    /// user closed and reopened it. Reopen in place instead, preserving the
    /// typed filter.
    pub(super) fn refresh_open_model_picker_after_catalog_update(&mut self) {
        // The effort step lists levels for an already-chosen route, so a new
        // catalog does not invalidate it.
        if self.model_picker_effort_parent.is_some() {
            return;
        }
        let picker_open = self
            .inline_interactive_state
            .as_ref()
            .is_some_and(picker_is_runtime_model_picker);
        if picker_open && !self.auth_catalog_refresh_pending {
            self.open_model_picker_preserving_input();
        }
    }

    /// Apply a completed auth-driven catalog refresh to an already-open picker.
    /// Login/import can finish while `/model` is visible, so merely invalidating
    /// its cache would leave the stale pre-login entries on screen until the user
    /// closed and reopened it.
    pub(super) fn finish_auth_catalog_refresh(&mut self) {
        let was_pending = self.auth_catalog_refresh_pending;
        self.auth_catalog_refresh_pending = false;
        self.invalidate_model_picker_cache();
        if self.model_picker_effort_parent.is_some() {
            return;
        }
        let picker_open = self
            .inline_interactive_state
            .as_ref()
            .is_some_and(picker_is_runtime_model_picker);
        if was_pending && picker_open {
            self.open_model_picker_preserving_input();
        }
    }

    fn open_loading_model_picker(&mut self, current_model: &str) {
        let model_label = if current_model.trim().is_empty() || current_model == "unknown" {
            "Loading models…".to_string()
        } else {
            current_model.to_string()
        };
        self.clear_inline_view();
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Model,
            filtered: vec![0],
            entries: vec![PickerEntry {
                name: model_label,
                options: vec![PickerOption {
                    provider: self.provider.name().to_string(),
                    api_method: "current".to_string(),
                    available: true,
                    detail: "updating model list…".to_string(),
                    estimated_reference_cost_micros: None,
                }],
                action: PickerAction::Model,
                selected_option: 0,
                is_current: true,
                is_default: false,
                is_favorite: false,
                recommended: false,
                recommendation_rank: usize::MAX,
                usage_score: 0,
                old: false,
                created_date: None,
                effort: None,
            }],
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        self.set_status_notice("Updating model list…");
    }

    fn start_model_picker_route_load(
        &mut self,
        signature: ModelPickerCacheSignature,
        picker_started: std::time::Instant,
    ) {
        let provider = self.provider.clone();
        self.start_model_picker_route_load_with(signature, picker_started, move || {
            provider.model_routes()
        });
    }

    /// Run an arbitrary route builder off the UI thread and deliver the result
    /// through the pending-load channel polled by `poll_model_picker_load`.
    fn start_model_picker_route_load_with(
        &mut self,
        signature: ModelPickerCacheSignature,
        picker_started: std::time::Instant,
        build_routes: impl FnOnce() -> Vec<crate::provider::ModelRoute> + Send + 'static,
    ) {
        self.start_model_picker_route_load_inner(signature, picker_started, false, build_routes);
    }

    /// Build routes AND the full picker entry list off the UI thread. Only
    /// cheap state application happens on the UI thread when it lands.
    fn start_model_picker_route_load_inner(
        &mut self,
        signature: ModelPickerCacheSignature,
        picker_started: std::time::Instant,
        store_remote_routes: bool,
        build_routes: impl FnOnce() -> Vec<crate::provider::ModelRoute> + Send + 'static,
    ) {
        self.model_picker_load_request_id = self.model_picker_load_request_id.wrapping_add(1);
        let request_id = self.model_picker_load_request_id;
        let ctx = self.model_picker_build_context();
        let (tx, rx) = std::sync::mpsc::channel();
        let build = move || {
            let routes_started = std::time::Instant::now();
            let routes = build_routes();
            let routes_ms = routes_started.elapsed().as_millis();
            let outcome = Self::build_model_picker_entries(ctx, routes, routes_ms, picker_started);
            let _ = tx.send(Ok(ModelPickerRoutesResult { outcome }));
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(build);
        } else {
            std::thread::spawn(build);
        }

        self.pending_model_picker_load = Some(PendingModelPickerLoad {
            request_id,
            signature,
            picker_started,
            receiver: rx,
            store_remote_routes,
        });
    }

    pub(super) fn poll_model_picker_load(&mut self) -> bool {
        let Some(pending) = self.pending_model_picker_load.as_ref() else {
            return false;
        };

        let received = match pending.receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_model_picker_load = None;
                self.set_status_notice("Model list update failed");
                return true;
            }
        };

        let Some(pending) = self.pending_model_picker_load.take() else {
            return false;
        };
        if pending.request_id != self.model_picker_load_request_id {
            return false;
        }

        let current_model = if self.is_remote {
            self.remote_provider_model
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.provider.model().to_string()
        };
        // UI preferences may be local, but provider defaults must not be.
        let remote_defaults = crate::config::Config::default();
        let config = if crate::tui::is_ssh_remote() {
            &remote_defaults
        } else {
            crate::config::config()
        };
        let config_default_model = config.provider.default_model.clone();
        let config_default_provider = config.provider.default_provider.clone();
        let current_effort = if self.is_remote {
            self.remote_reasoning_effort.clone()
        } else {
            self.provider.reasoning_effort()
        };
        let available_efforts = if self.is_remote {
            inferred_reasoning_efforts(
                self.remote_provider_name.as_deref(),
                self.remote_provider_model.as_deref(),
            )
        } else {
            self.provider.available_efforts()
        };
        let config_default_model_for_resign = config_default_model.clone();
        let config_default_provider_for_resign = config_default_provider.clone();
        let current_effort_for_resign = current_effort.clone();
        let current_signature = self.model_picker_cache_signature(
            &current_model,
            config_default_model,
            config_default_provider,
            current_effort,
            &available_efforts,
        );
        if current_signature != pending.signature {
            return false;
        }

        match received {
            Ok(result) => {
                let picker_open = self
                    .inline_interactive_state
                    .as_ref()
                    .is_some_and(|picker| picker.kind == PickerKind::Model);
                let routes = if picker_open {
                    self.apply_built_model_picker(pending.signature, result.outcome, true, true)
                } else {
                    // User dismissed the picker while it loaded. Keep the work
                    // as a warm cache, but do not pop the picker back open.
                    match result.outcome {
                        ModelPickerBuildOutcome::Built {
                            entries,
                            model_count,
                            routes,
                        } => {
                            if Self::should_cache_model_picker_entries(model_count, routes.len()) {
                                self.model_picker_cache = Some(ModelPickerCache {
                                    signature: pending.signature,
                                    entries,
                                    route_count: routes.len(),
                                    model_count,
                                });
                            }
                            routes
                        }
                        ModelPickerBuildOutcome::Empty => Vec::new(),
                    }
                };
                if pending.store_remote_routes && !routes.is_empty() {
                    // Keep the normalized routes so the signature (which
                    // samples remote_model_options) stays stable, then
                    // re-sign the cache so the next open is a cache hit.
                    self.remote_model_options = routes;
                    let fresh_signature = self.model_picker_cache_signature(
                        &current_model,
                        config_default_model_for_resign,
                        config_default_provider_for_resign,
                        current_effort_for_resign,
                        &available_efforts,
                    );
                    if let Some(cache) = self.model_picker_cache.as_mut() {
                        cache.signature = fresh_signature;
                    }
                }
                if picker_open && self.inline_interactive_state.is_some() {
                    self.set_status_notice("Model list updated");
                }
                true
            }
            Err(error) => {
                self.set_status_notice(format!("Model list update failed: {}", error));
                true
            }
        }
    }

    fn open_model_picker_with_routes(
        &mut self,
        cache_signature: ModelPickerCacheSignature,
        picker_started: std::time::Instant,
        routes: Vec<crate::provider::ModelRoute>,
        routes_ms: u128,
        preserve_input: bool,
        cache_entries: bool,
    ) -> Vec<crate::provider::ModelRoute> {
        let ctx = self.model_picker_build_context();
        let outcome = Self::build_model_picker_entries(ctx, routes, routes_ms, picker_started);
        self.apply_built_model_picker(cache_signature, outcome, preserve_input, cache_entries)
    }

    /// Snapshot of the App state the picker entry builder needs, so the
    /// expensive build can run off the UI thread.
    fn model_picker_build_context(&self) -> ModelPickerBuildContext {
        ModelPickerBuildContext {
            is_remote: self.is_remote,
            current_model: if self.is_remote {
                self.remote_provider_model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                self.provider.model().to_string()
            },
            current_provider: if self.is_remote {
                self.remote_provider_name
                    .clone()
                    .unwrap_or_else(|| "remote".to_string())
            } else {
                self.provider.display_name()
            },
            remote_provider_name: self.remote_provider_name.clone(),
            current_api_method: self.current_route_api_method(),
            current_effort: if self.is_remote {
                self.remote_reasoning_effort.clone()
            } else {
                self.provider.reasoning_effort()
            },
            recent_authenticated_provider: self
                .recent_authenticated_provider
                .as_ref()
                .map(|(provider, _)| provider.clone()),
        }
    }

    pub(in crate::tui::app) fn debug_model_picker_live_json(
        &mut self,
        visible_limit: Option<usize>,
    ) -> String {
        let previous_inline_view = self.inline_view_state.clone();
        let previous_inline_view_is_btw = self.inline_view_is_btw;
        let previous_inline_interactive = self.inline_interactive_state.clone();
        let previous_model_picker_cache = self.model_picker_cache.clone();
        let previous_pending_model_picker_load = self.pending_model_picker_load.take();
        let previous_model_picker_load_request_id = self.model_picker_load_request_id;
        let previous_input = self.input.clone();
        let previous_cursor_pos = self.cursor_pos;
        let previous_status_notice = self.status_notice.clone();

        if self.is_remote && self.remote_model_options.is_empty() {
            self.hydrate_remote_model_catalog_cache();
        }

        let started = std::time::Instant::now();
        let current_model = if self.is_remote {
            self.remote_provider_model
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.provider.model().to_string()
        };
        // UI preferences may be local, but provider defaults must not be.
        let remote_defaults = crate::config::Config::default();
        let config = if crate::tui::is_ssh_remote() {
            &remote_defaults
        } else {
            crate::config::config()
        };
        let config_default_model = config.provider.default_model.clone();
        let config_default_provider = config.provider.default_provider.clone();
        let current_effort = if self.is_remote {
            self.remote_reasoning_effort.clone()
        } else {
            self.provider.reasoning_effort()
        };
        let available_efforts = if self.is_remote {
            inferred_reasoning_efforts(
                self.remote_provider_name.as_deref(),
                self.remote_provider_model.as_deref(),
            )
        } else {
            self.provider.available_efforts()
        };
        let signature = self.model_picker_cache_signature(
            &current_model,
            config_default_model,
            config_default_provider,
            current_effort,
            &available_efforts,
        );

        let routes_started = std::time::Instant::now();
        let took_remote_options = self.is_remote && !self.remote_model_options.is_empty();
        let routes: Vec<crate::provider::ModelRoute> = if self.is_remote {
            if took_remote_options {
                let mut routes = std::mem::take(&mut self.remote_model_options);
                self.extend_remote_routes_for_uncovered_models(&mut routes);
                routes
            } else {
                self.build_remote_model_routes_lightweight_fallback(&current_model)
            }
        } else if crate::perf::tui_policy().simplified_model_picker {
            self.simplified_model_routes_for_picker(&current_model)
        } else {
            self.provider.model_routes()
        };
        let routes_ms = routes_started.elapsed().as_millis();
        let raw_route_count = routes.len();
        let mut raw_static_fallback_by_provider =
            std::collections::BTreeMap::<String, usize>::new();
        for route in &routes {
            if route
                .detail
                .contains("fallback: static provider model list")
            {
                *raw_static_fallback_by_provider
                    .entry(route.provider.clone())
                    .or_default() += 1;
            }
        }
        let raw_static_fallback_count: usize = raw_static_fallback_by_provider.values().sum();
        let raw_routes = routes
            .iter()
            .take(visible_limit.unwrap_or(200))
            .map(|route| {
                serde_json::json!({
                    "model": route.model,
                    "provider": route.provider,
                    "api_method": route.api_method,
                    "available": route.available,
                    "detail": route.detail,
                    "estimated_reference_cost_micros": route.estimated_reference_cost_micros(),
                })
            })
            .collect::<Vec<_>>();

        let routes =
            self.open_model_picker_with_routes(signature, started, routes, routes_ms, false, false);
        let picker_json = self.debug_picker_state_json(visible_limit);
        let picker_value: serde_json::Value = serde_json::from_str(&picker_json)
            .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize picker" }));
        let mut picker_static_fallback_by_provider =
            std::collections::BTreeMap::<String, usize>::new();
        if let Some(picker) = self.inline_interactive_state.as_ref() {
            for entry in &picker.entries {
                if let Some(route) = entry.active_option()
                    && route
                        .detail
                        .contains("fallback: static provider model list")
                {
                    *picker_static_fallback_by_provider
                        .entry(route.provider.clone())
                        .or_default() += 1;
                }
            }
        }
        let picker_static_fallback_count: usize = picker_static_fallback_by_provider.values().sum();

        self.inline_view_state = previous_inline_view;
        self.inline_view_is_btw = previous_inline_view_is_btw;
        self.inline_interactive_state = previous_inline_interactive;
        self.model_picker_cache = previous_model_picker_cache;
        self.pending_model_picker_load = previous_pending_model_picker_load;
        self.model_picker_load_request_id = previous_model_picker_load_request_id;
        if took_remote_options && self.remote_model_options.is_empty() {
            // Restore only routes that originated from remote_model_options.
            // Lightweight fallback routes carry placeholder "remote-catalog"
            // methods and must never be written back as the detailed catalog.
            self.remote_model_options = routes;
        }
        self.input = previous_input;
        self.cursor_pos = previous_cursor_pos;
        self.status_notice = previous_status_notice;

        serde_json::to_string_pretty(&serde_json::json!({
            "source_of_truth": "materialized_tui_model_picker",
            "remote": self.is_remote,
            "provider_name": if self.is_remote {
                self.remote_provider_name.clone().unwrap_or_else(|| "remote".to_string())
            } else {
                self.provider.name().to_string()
            },
            "current_model": current_model,
            "raw_route_count": raw_route_count,
            "raw_route_sample_count": raw_routes.len(),
            "raw_static_fallback_count": raw_static_fallback_count,
            "raw_static_fallback_by_provider": raw_static_fallback_by_provider,
            "picker_static_fallback_count": picker_static_fallback_count,
            "picker_static_fallback_by_provider": picker_static_fallback_by_provider,
            "routes_ms": routes_ms,
            "total_ms": started.elapsed().as_millis(),
            "raw_routes": raw_routes,
            "picker": picker_value,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    pub(super) fn build_remote_model_routes_fallback(&self) -> Vec<crate::provider::ModelRoute> {
        if crate::tui::is_ssh_remote() {
            return self.build_remote_model_routes_lightweight_fallback(
                self.remote_provider_model.as_deref().unwrap_or("unknown"),
            );
        }
        crate::provider::remote_model_routes_fallback(
            self.remote_provider_name.as_deref(),
            &self.remote_available_entries,
        )
    }

    fn build_remote_model_routes_lightweight_fallback(
        &self,
        current_model: &str,
    ) -> Vec<crate::provider::ModelRoute> {
        crate::provider::remote_model_routes_lightweight_fallback(
            self.remote_provider_name.as_deref(),
            &self.remote_available_entries,
            current_model,
        )
    }

    /// When a runtime model-picker preview is visible, route its favorite/default
    /// hotkeys to the focused picker handler. Returns true if the key was consumed.
    pub(super) fn model_picker_preview_hotkey(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        let active = self
            .inline_interactive_state
            .as_ref()
            .map(|picker| picker.preview && picker_is_runtime_model_picker(picker))
            .unwrap_or(false);
        if !active {
            return Ok(false);
        }
        // Use Ctrl+O (set default) and Ctrl+N (toggle favorite) so the picker
        // preview no longer steals Ctrl+B / Ctrl+F / Alt+F, which are the tmux
        // prefix and readline word-navigation keys users rely on while editing
        // the `/model` command line. Cycling favorites stays on Shift+Tab.
        let is_default =
            modifiers.contains(KeyModifiers::CONTROL) && key_char_eq_ignore_ascii_case(code, 'o');
        let is_favorite =
            modifiers.contains(KeyModifiers::CONTROL) && key_char_eq_ignore_ascii_case(code, 'n');
        if is_default || is_favorite {
            self.handle_inline_interactive_key(code, modifiers)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn handle_inline_interactive_preview_key(
        &mut self,
        code: &KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        let is_preview = self
            .inline_interactive_state
            .as_ref()
            .is_some_and(|p| p.preview);
        if !is_preview {
            return Ok(false);
        }
        match code {
            KeyCode::Down => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    let max = picker.filtered.len().saturating_sub(1);
                    picker.selected = (picker.selected + 1).min(max);
                }
                Ok(true)
            }
            KeyCode::Up => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                Ok(true)
            }
            KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    let max = picker.filtered.len().saturating_sub(1);
                    picker.selected = (picker.selected + 1).min(max);
                }
                Ok(true)
            }
            KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                Ok(true)
            }
            KeyCode::PageDown => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    let max = picker.filtered.len().saturating_sub(1);
                    picker.selected = (picker.selected + 5).min(max);
                }
                Ok(true)
            }
            KeyCode::PageUp => {
                if let Some(picker) = self.inline_interactive_state.as_mut() {
                    picker.selected = picker.selected.saturating_sub(5);
                }
                Ok(true)
            }
            KeyCode::Enter => {
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.filtered.is_empty() {
                        self.inline_interactive_state = None;
                        self.input.clear();
                        self.cursor_pos = 0;
                        return Ok(true);
                    }
                    // `/login` + immediate Enter should not silently launch the
                    // first provider's login flow. Without a filter or an
                    // explicit selection there is no clear user choice yet, so
                    // activate the picker and let them pick deliberately.
                    if picker.kind == PickerKind::Login
                        && picker.filter.is_empty()
                        && picker.selected == 0
                    {
                        picker.preview = false;
                        picker.column = 0;
                        self.input.clear();
                        self.cursor_pos = 0;
                        return Ok(true);
                    }
                    picker.preview = false;
                    if picker.kind == PickerKind::Usage {
                        picker.column = 0;
                        self.input.clear();
                        self.cursor_pos = 0;
                        self.request_usage_report();
                        return Ok(true);
                    }
                    picker.column = picker.preview_activation_column();
                }
                self.input.clear();
                self.cursor_pos = 0;
                self.handle_inline_interactive_key(KeyCode::Enter, modifiers)?;
                Ok(true)
            }
            KeyCode::Esc => {
                self.inline_interactive_state = None;
                self.input.clear();
                self.cursor_pos = 0;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn handle_account_picker_selection(&mut self, action: AccountPickerAction) {
        if (self.is_remote || crate::tui::is_ssh_remote())
            && matches!(&action, AccountPickerAction::Switch { .. })
        {
            if let AccountPickerAction::Switch { provider_id, label } = action {
                self.set_status_notice(format!(
                    "Account switch queued: {} ({})",
                    label, provider_id
                ));
                self.pending_account_picker_action =
                    Some(AccountPickerAction::Switch { provider_id, label });
            }
            return;
        }
        if super::commands_dispatch::ssh_local_action_blocked(self, "Local account selection") {
            return;
        }
        match action {
            AccountPickerAction::Switch { provider_id, label } => match provider_id.as_str() {
                "claude" => self.switch_account(&label),
                "openai" => self.switch_openai_account(&label),
                _ => self.push_display_message(DisplayMessage::error(format!(
                    "Provider `{}` does not support account switching.",
                    provider_id
                ))),
            },
            AccountPickerAction::Add { provider_id } => match provider_id.as_str() {
                "claude" => match crate::auth::claude::next_account_label() {
                    Ok(label) => self.start_claude_login_for_account(&label),
                    Err(e) => self.push_display_message(DisplayMessage::error(format!(
                        "Failed to prepare Claude account: {}",
                        e
                    ))),
                },
                "openai" => match crate::auth::codex::next_account_label() {
                    Ok(label) => self.start_openai_login_for_account(&label),
                    Err(e) => self.push_display_message(DisplayMessage::error(format!(
                        "Failed to prepare OpenAI account: {}",
                        e
                    ))),
                },
                _ => self.push_display_message(DisplayMessage::error(format!(
                    "Provider `{}` does not support multiple accounts.",
                    provider_id
                ))),
            },
            AccountPickerAction::Replace { provider_id, label } => match provider_id.as_str() {
                "claude" => self.start_claude_login_for_account(&label),
                "openai" => self.start_openai_login_for_account(&label),
                _ => self.push_display_message(DisplayMessage::error(format!(
                    "Provider `{}` does not support account replacement.",
                    provider_id
                ))),
            },
            AccountPickerAction::OpenCenter { provider_filter } => {
                self.open_account_center(provider_filter.as_deref())
            }
        }
    }

    fn toggle_selected_model_favorite(&mut self) {
        if super::commands_dispatch::ssh_local_action_blocked(self, "Saving model favorites") {
            return;
        }
        let Some((entry_name, is_favorite, store)) = (|| {
            let picker = self.inline_interactive_state.as_mut()?;
            if !picker_is_runtime_model_picker(picker) || picker.filtered.is_empty() {
                return None;
            }
            let idx = picker.filtered[picker.selected];
            let entry = picker.entries.get_mut(idx)?;
            if !matches!(entry.action, PickerAction::Model) {
                return None;
            }
            let base_name = model_entry_base_name(entry);
            let effort = entry.effort.clone();
            let route = entry.options.get(entry.selected_option).cloned()?;
            let key = model_picker_usage_key(&base_name, &route, effort.as_deref());
            let mut store = load_model_picker_favorites_store();
            store.version = MODEL_PICKER_FAVORITES_VERSION;
            let is_favorite = if store.favorites.remove(&key) {
                false
            } else {
                store.favorites.insert(key);
                true
            };
            entry.is_favorite = is_favorite;
            Some((entry.name.clone(), is_favorite, store))
        })() else {
            return;
        };
        save_model_picker_favorites_store(&store);
        self.invalidate_model_picker_cache();
        let action = if is_favorite {
            "Favorited"
        } else {
            "Unfavorited"
        };
        self.set_status_notice(format!("{} {}", action, entry_name));
    }

    fn cycle_selected_model_favorite(&mut self) {
        let selected_name = (|| {
            let picker = self.inline_interactive_state.as_mut()?;
            if !picker_is_runtime_model_picker(picker) || picker.filtered.is_empty() {
                return None;
            }
            let total = picker.filtered.len();
            for offset in 1..=total {
                let next = (picker.selected + offset) % total;
                let entry_idx = picker.filtered[next];
                if picker
                    .entries
                    .get(entry_idx)
                    .map(|entry| entry.is_favorite)
                    .unwrap_or(false)
                {
                    picker.selected = next;
                    picker.column = 0;
                    return picker
                        .entries
                        .get(entry_idx)
                        .map(|entry| entry.name.clone());
                }
            }
            None
        })();
        if let Some(entry_name) = selected_name {
            self.set_status_notice(format!("Favorite → {}", entry_name));
        } else {
            self.set_status_notice("No favorited models yet. Use Ctrl+N to favorite one.");
        }
    }

    fn cycle_model_favorite_after_current(&mut self) -> bool {
        let selected_name = (|| {
            let picker = self.inline_interactive_state.as_mut()?;
            if !picker_is_runtime_model_picker(picker) || picker.filtered.is_empty() {
                return None;
            }
            let next = next_model_favorite_after_current(picker)?;
            let entry_idx = picker.filtered[next];
            picker.selected = next;
            picker.column = 0;
            picker
                .entries
                .get(entry_idx)
                .map(|entry| entry.name.clone())
        })();
        if let Some(entry_name) = selected_name {
            self.set_status_notice(format!("Favorite → {}", entry_name));
            true
        } else {
            self.set_status_notice("No favorited models yet. Use Ctrl+N to favorite one.");
            false
        }
    }

    pub(super) fn cycle_model_favorite_hotkey(&mut self) {
        if self
            .inline_interactive_state
            .as_ref()
            .map(picker_is_runtime_model_picker)
            .unwrap_or(false)
        {
            self.cycle_selected_model_favorite();
            return;
        }

        self.open_model_picker_preserving_input();
        if !self
            .inline_interactive_state
            .as_ref()
            .map(picker_is_runtime_model_picker)
            .unwrap_or(false)
        {
            self.set_status_notice("Model favorites unavailable until model routes finish loading");
            return;
        }
        if self.cycle_model_favorite_after_current() {
            let _ = self.handle_inline_interactive_key(KeyCode::Enter, KeyModifiers::NONE);
        }
    }

    /// Step two of the model picker: the levels the chosen route accepts.
    ///
    /// The model list is stashed so Esc can return to the same place, and the
    /// level rows are ordinary model entries carrying `effort`, so the existing
    /// switch path applies model and level together.
    pub(super) fn open_model_picker_effort_step(&mut self, entry: &PickerEntry) {
        let ctx = self.model_picker_build_context();
        let rows = model_picker_stage::effort_entries(
            entry,
            &ctx.current_model,
            ctx.current_effort.as_deref(),
        );
        let Some(kind) = self
            .inline_interactive_state
            .as_ref()
            .map(|picker| picker.kind)
        else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        // Open on the level in use, else the provider default, else the weakest.
        let selected = rows
            .iter()
            .position(|row| row.is_current)
            .or_else(|| {
                let current = ctx.current_effort.as_deref();
                rows.iter().position(|row| row.effort.as_deref() == current)
            })
            .or_else(|| {
                rows.iter()
                    .position(|row| row.effort.as_deref() == Some("medium"))
            })
            .unwrap_or(0);
        let parent = self.inline_interactive_state.take();
        self.model_picker_effort_parent = parent.map(Box::new);
        let mut state = InlineInteractiveState {
            kind,
            filtered: (0..rows.len()).collect(),
            entries: rows,
            selected,
            column: 0,
            filter: String::new(),
            preview: false,
        };
        Self::apply_inline_interactive_filter(&mut state);
        self.inline_interactive_state = Some(state);
    }

    /// Esc from the effort step: back to the model list it came from.
    pub(super) fn close_model_picker_effort_step(&mut self) {
        if let Some(parent) = self.model_picker_effort_parent.take() {
            self.inline_interactive_state = Some(*parent);
        }
    }
}

#[cfg(test)]
#[path = "queued_picker_submission_tests.rs"]
mod queued_picker_submission_tests;

#[cfg(test)]
#[path = "inline_interactive_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "inline_interactive/cloud_resume_tests.rs"]
mod cloud_resume_tests;

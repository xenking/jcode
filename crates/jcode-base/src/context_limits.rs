//! Process-wide cache of model context window sizes.
//!
//! Filled from provider catalogs and from `[providers.*].models[].context_window`
//! in the user's config. It sits below `config` and `provider` so config loading
//! can seed it directly, in the same call and order as before, without
//! depending on the provider module.

use jcode_config_types::NamedProviderConfig;
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, RwLock};

static CACHE: LazyLock<RwLock<HashMap<String, usize>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Look up a cached context limit for a model.
pub fn cached_context_limit_for_model(model: &str) -> Option<usize> {
    let cache = CACHE.read().ok()?;
    cache.get(model).copied()
}

/// Populate the context limit cache from API-provided model data.
/// Called once at startup when OpenAI OAuth credentials are available.
pub fn populate_context_limits(models: HashMap<String, usize>) {
    if let Ok(mut cache) = CACHE.write() {
        for (model, limit) in models {
            crate::logging::info(&format!(
                "Context limit cache: {} = {}k",
                model,
                limit / 1000
            ));
            cache.insert(model, limit);
        }
    }
}

/// Seed the cache from named provider model configs.
///
/// Custom OpenAI-compatible providers that lack a usable `/v1/models` endpoint
/// rely on per-model `context_window` config. Every resolution path (TUI info
/// widget, compaction budget, model switching) goes through this cache, so a
/// configured limit is seeded here instead of falling back to the default.
///
/// Runtime model specs reach the lookup in several shapes, so each configured
/// model is seeded under every key the lookup can normalize to (issue #421):
/// - the bare lowercased id (`qwen3.6-35b-a2000-128k`);
/// - the slash base (`x.gguf` for `/opt/models/x.gguf`), because
///   `model_id_for_capability_lookup` reduces slash-containing ids to their
///   final segment;
/// - the profile-qualified spec (`cachyai-a2000:qwen3.6-35b-a2000-128k`),
///   because session-restored models keep the `<profile>:` routing prefix and
///   non-slash qualified specs are looked up verbatim.
pub fn populate_from_providers(providers: &BTreeMap<String, NamedProviderConfig>) {
    let mut limits = HashMap::new();
    for (profile_id, provider_cfg) in providers {
        for model in &provider_cfg.models {
            let Some(limit) = model.context_window else {
                continue;
            };
            for key in config_cache_keys(profile_id, &model.id) {
                limits.insert(key, limit);
            }
        }
    }
    if !limits.is_empty() {
        populate_context_limits(limits);
    }
}

/// Cache keys under which a configured per-model `context_window` must be
/// discoverable so every runtime lookup shape resolves to it.
fn config_cache_keys(profile_id: &str, model_id: &str) -> Vec<String> {
    let id = model_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![id.clone()];
    let slash_base = jcode_provider_core::model_id::slash_base(&id).to_string();
    if slash_base != id && !slash_base.is_empty() {
        keys.push(slash_base);
    }
    let profile = profile_id.trim().to_ascii_lowercase();
    if !profile.is_empty() {
        keys.push(format!("{profile}:{id}"));
    }
    keys
}

/// Run `f` on the cache, for tests that must inspect or clear entries.
#[cfg(test)]
pub(crate) fn with_cache<R>(f: impl FnOnce(&mut HashMap<String, usize>) -> R) -> R {
    f(&mut CACHE.write().unwrap())
}

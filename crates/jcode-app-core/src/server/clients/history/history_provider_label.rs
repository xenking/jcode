//! Profile-aware provider labels for history payloads.

use super::*;

pub(super) fn history_provider_name_from_session(
    session: &crate::session::Session,
) -> Option<String> {
    let key = session.provider_key()?.trim();
    if key.is_empty() {
        return None;
    }
    Some(provider_label_for_key(key))
}

fn provider_label_for_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "openai" => "OpenAI".to_string(),
        "claude" | "anthropic" => "Anthropic".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        "copilot" => "GitHub Copilot".to_string(),
        "cursor" => "Cursor".to_string(),
        "gemini" => "Gemini".to_string(),
        "bedrock" => "Bedrock".to_string(),
        "antigravity" => "Antigravity".to_string(),
        "jcode" => "Jcode".to_string(),
        // A direct OpenAI-compatible profile is persisted either bare
        // (`deepseek`, the session vocabulary) or as its source key
        // (`openai-compatible:deepseek`). Both name the same profile, so report
        // its display name like every other arm here; a raw key would otherwise
        // reach the header, and `source_key_for_provider_label` maps the
        // prefixed shape away from `openai:api-key` only by accident (#1286).
        other => {
            // Import-source codes are stored in the same field but are not
            // profile routes; `opencode` in particular collides with the
            // `OpenCode Zen` profile id, so it must stay verbatim.
            const IMPORT_SOURCE_CODES: &[&str] = &["opencode", "claude-code", "openai-codex"];
            if IMPORT_SOURCE_CODES.contains(&other) {
                other.to_string()
            } else {
                let profile_id = other.strip_prefix("openai-compatible:").unwrap_or(other);
                crate::provider_catalog::openai_compatible_profile_by_id(profile_id)
                    .map(|profile| profile.display_name.to_string())
                    .unwrap_or_else(|| other.to_string())
            }
        }
    }
}

/// Who a session is, as its own persisted record says. A snapshot built while
/// the agent is busy has only this record; reading the daemon's shared provider
/// instead named one session's model with another session's provider and
/// account (a `gpt-` model under `Claude` / `claude-fox`).
pub(super) struct RecordIdentity {
    pub provider_name: Option<String>,
    pub credential: Option<jcode_provider_core::ResolvedCredential>,
    pub account_label: Option<String>,
}

/// `None` for a record that names no model yet (a fresh session): nothing in
/// it can contradict the provider the daemon would give it.
pub(super) fn record_identity(session: &crate::session::Session) -> Option<RecordIdentity> {
    let model = session.model()?;
    let method = session.route_api_method();
    Some(RecordIdentity {
        provider_name: history_provider_name_from_session(session)
            .or_else(|| crate::provider::provider_for_model(model).map(provider_label_for_key)),
        credential: method
            .is_some_and(|method| method.contains("oauth"))
            .then_some(jcode_provider_core::ResolvedCredential::Oauth),
        account_label: method
            .and_then(jcode_provider_core::oauth_account_label)
            .map(str::to_owned),
    })
}

/// The provider name a `History` payload reports.
///
/// The persisted session key wins when there is one. Otherwise ask for the
/// profile-aware label: `Provider::name()` is the stable machine id for the
/// provider *class* (`OpenRouter` for the multiplexing slot, `openrouter` for
/// a concrete runtime instance), and that slot also serves every direct
/// OpenAI-compatible profile, so it must not be shown as this session's
/// provider (issue #1286).
pub(super) fn history_provider_name(
    session: &crate::session::Session,
    provider: &dyn Provider,
) -> Option<String> {
    history_provider_name_from_session(session).or_else(|| Some(provider.display_name()))
}

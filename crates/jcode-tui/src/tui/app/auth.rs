#[path = "auth_subscription_status.rs"]
mod subscription_status;

#[path = "auth_account_commands.rs"]
mod auth_account_commands;
#[path = "auth_account_picker.rs"]
mod auth_account_picker;
#[path = "auth/auth_muse.rs"]
pub(super) mod auth_muse;
#[path = "auth_progress.rs"]
pub(crate) mod auth_progress;
/// Attempt-scoped Muse event helpers shared with the remote event forwarder.
pub(super) use auth_muse as auth_muse_helpers;
#[path = "auth_types.rs"]
mod auth_types;
pub(crate) use self::auth_account_commands::{
    account_command_from_picker, execute_account_command_remote, handle_account_command_remote,
    handle_auth_command, handle_ssh_usage_command_remote, save_openai_fast_setting_local,
};

pub(crate) fn resolve_account_provider_descriptor(
    input: &str,
) -> Option<crate::provider_catalog::LoginProviderDescriptor> {
    crate::provider_catalog::resolve_login_provider(input)
}
pub(super) use self::auth_types::{AccountCommand, PendingAccountInput, PendingLogin};

use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

fn saved_account_route_selection(
    provider_id: &str,
    label: &str,
    current_provider: &str,
    current_model: &str,
    configured_default: Option<&str>,
) -> anyhow::Result<crate::provider::RouteSelection> {
    if !matches!(provider_id, "claude" | "openai") {
        anyhow::bail!("Saved OAuth accounts are not supported for provider {provider_id}");
    }
    let (runtime_key, provider_label, method) = if provider_id == "claude" {
        (
            crate::provider::RuntimeKey::ClaudeOAuth,
            "Claude",
            "claude-oauth",
        )
    } else {
        (
            crate::provider::RuntimeKey::OpenAIOAuth,
            "OpenAI",
            "openai-oauth",
        )
    };
    let same_provider = current_provider.eq_ignore_ascii_case(provider_label)
        || (provider_id == "claude" && current_provider.eq_ignore_ascii_case("anthropic"));
    let configured_model = configured_default
        .and_then(|spec| {
            jcode_provider_core::selection::explicit_model_provider_prefix(spec.trim())
        })
        .filter(|(provider, _, _)| {
            matches!(
                (provider_id, provider),
                ("claude", jcode_provider_core::ActiveProvider::Claude)
                    | ("openai", jcode_provider_core::ActiveProvider::OpenAI)
            )
        })
        .map(|(_, _, model)| model.trim())
        .filter(|model| !model.is_empty());
    let model = if same_provider && !current_model.trim().is_empty() {
        current_model
    } else {
        configured_model
            .or_else(|| crate::auth::lifecycle::default_model_for_provider(provider_id))
            .ok_or_else(|| anyhow::anyhow!("No default model is configured for {provider_label}"))?
    };
    Ok(crate::provider::RouteSelection {
        model: model.to_string(),
        runtime_key,
        api_method: format!("{method}@{label}"),
        provider_label: provider_label.to_string(),
        detail: format!("Saved account {label}"),
    })
}

#[cfg(test)]
#[path = "saved_account_route_tests.rs"]
mod saved_account_route_tests;

impl App {
    pub(in self::super) fn handle_login_completed(&mut self, login: LoginCompleted) {
        if login.provider == "copilot_code" {
            if jcode_tui_messages::parse_collapsible_swarm_content(&login.message).is_some() {
                self.upsert_login_progress_card(
                    auth_progress::COPILOT_LOGIN_TITLE,
                    login.message.clone(),
                );
            } else {
                self.push_display_message(DisplayMessage::system(login.message.clone()));
            }
            if let Some(code) = Self::extract_device_code_from_message(&login.message) {
                self.set_status_notice(format!("Login: enter {} at GitHub", code));
            }
            return;
        }
        if login.provider == "openai_device_code" {
            if !matches!(self.pending_login, Some(PendingLogin::OpenAiAccount { .. })) {
                return;
            }
            self.upsert_login_progress_card(
                auth_progress::OPENAI_LOGIN_TITLE,
                login.message.clone(),
            );
            if let Some(code) = Self::extract_device_code_from_message(&login.message) {
                self.set_status_notice(format!("Login: enter {} at OpenAI", code));
            }
            return;
        }
        if login.provider == "grok_build_code" {
            if !matches!(self.pending_login, Some(PendingLogin::GrokBuild)) {
                return;
            }
            self.upsert_login_progress_card(
                auth_progress::GROK_BUILD_LOGIN_TITLE,
                login.message.clone(),
            );
            if let Some(code) = Self::extract_device_code_from_message(&login.message) {
                self.set_status_notice(format!("Login: enter {} at xAI", code));
            }
            return;
        }
        if login.provider == "jcode_code" {
            self.upsert_login_progress_card(
                auth_progress::JCODE_LOGIN_TITLE,
                login.message.clone(),
            );
            self.set_status_notice("Jcode account: waiting for browser approval");
            return;
        }
        // A callback may complete while the user is still typing a URL/code.
        // Never leave that authentication input armed as the next AI prompt.
        if !matches!(
            self.pending_login,
            Some(PendingLogin::MuseSubscription { .. })
        ) && self
            .pending_login
            .as_ref()
            .and_then(PendingLogin::telemetry_context)
            .is_some_and(|(provider, _)| provider == login.provider)
        {
            self.input.clear();
            self.cursor_pos = 0;
            self.pasted_contents.clear();
            self.clear_input_undo_history();
        }
        // Native Muse subscription progress (device code display): render only,
        // like copilot_code. This is pending approval, never a completed login.
        // Progress events carry the attempt id (`muse_subscription_code#<id>`)
        // and are accepted only while the same attempt is still pending and active.
        // (copilot_code keeps its dedicated handler above.)
        if let Some(attempt) = login.provider.strip_prefix("muse_subscription_code#") {
            let (is_match, is_cancelled) = match &self.pending_login {
                Some(PendingLogin::MuseSubscription {
                    attempt: pending_attempt,
                    cancelled,
                    ..
                }) if auth_muse_helpers::pending_attempt_matches(pending_attempt, attempt) => {
                    (true, cancelled.load(std::sync::atomic::Ordering::Acquire))
                }
                _ => (false, false),
            };
            if !is_match || is_cancelled {
                return;
            }
            if jcode_tui_messages::parse_collapsible_swarm_content(&login.message).is_some() {
                self.upsert_login_progress_card(
                    auth_progress::MUSE_LOGIN_TITLE,
                    login.message.clone(),
                );
            } else {
                self.push_display_message(DisplayMessage::system(login.message.clone()));
            }
            if let Some(code) = Self::extract_device_code_from_message(&login.message) {
                self.set_status_notice(format!("Login: enter {} at Meta", code));
            }
            return;
        }
        // Native Muse subscription approval: the background task placed the
        // credentials in the pending slot and never persisted. Take them here
        // on the main thread and save synchronously, but only when the event's
        // attempt id matches the currently pending attempt. Anything else
        // (cancelled, superseded, or cross-session) is ignored, and a missing
        // slot can never switch the provider.
        if self.take_muse_subscription_approval(&login) {
            return;
        }
        crate::auth::AuthStatus::invalidate_cache();
        crate::logging::event_info(
            "login_completed",
            vec![
                ("provider", login.provider.clone()),
                ("success", login.success.to_string()),
            ],
        );
        if let Some((provider, method)) = self
            .pending_login
            .as_ref()
            .and_then(PendingLogin::telemetry_context)
        {
            if login.success {
                if !matches!(
                    self.pending_login,
                    Some(PendingLogin::MuseSubscription { .. })
                ) {
                    crate::telemetry::record_auth_success(&provider, &method);
                }
            } else {
                let reason =
                    crate::auth::login_diagnostics::classify_auth_failure_message(&login.message);
                crate::telemetry::record_auth_failed_reason(&provider, &method, reason.label());
            }
        }
        if login.success {
            let replaced_login_card = auth_progress::login_card_title_for_provider(&login.provider)
                .is_some_and(|title| {
                    let completion = if login.provider == "openai" {
                        "✓ OpenAI login complete.".to_string()
                    } else if login.message.starts_with('✓') {
                        login.message.clone()
                    } else {
                        format!("✓ {}", login.message)
                    };
                    self.finish_login_progress(title, completion)
                });
            self.recent_authenticated_provider = Some((login.provider.clone(), Instant::now()));
            // A fresh login is exactly what the credential-failure breaker is
            // waiting for: give automatic retries a fresh budget.
            self.reset_credential_failure_breaker();
            self.auth_catalog_refresh_pending = true;
            self.invalidate_model_picker_cache();
            let suppress_first_run_login_noise = self.onboarding_flow_active()
                && !auth_muse_helpers::is_device_code_progress(&login.provider);
            if !suppress_first_run_login_noise && !replaced_login_card {
                self.push_display_message(DisplayMessage::system(login.message));
            }
            self.set_status_notice(format!("Login: {} ready", login.provider));
            if Self::login_provider_is_azure(&login.provider) {
                self.activate_azure_runtime_model_after_login();
            } else {
                let prefer_strongest = self.onboarding_should_prefer_strongest_model();
                if prefer_strongest {
                    self.onboarding_auto_model_selection_active
                        .store(true, std::sync::atomic::Ordering::Release);
                    *self
                        .onboarding_auto_model_selection_baseline
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(self.provider.model());
                }
                // Direct OpenAI-compatible logins already launched the
                // profile-specific catalog refresh and model activation before
                // publishing LoginCompleted. The generic auth refresh still
                // needs to rebuild routes and release the picker loading state,
                // but must not race it with a second model selection.
                let profile_activation_owns_selection = if let Some(profile) =
                    crate::provider_catalog::resolve_openai_compatible_profile_selection(
                        &login.provider,
                    ) {
                    if matches!(self.pending_login, Some(PendingLogin::Exchanging { .. })) {
                        self.start_openai_compatible_post_login_activation(
                            profile.id.to_string(),
                            profile.display_name.to_string(),
                        );
                    }
                    true
                } else {
                    false
                };
                let require_model_confirmation = !prefer_strongest
                    && crate::auth::lifecycle::normalized_auth_provider_id(Some(&login.provider))
                        == Some("claude");
                if require_model_confirmation {
                    self.push_display_message(DisplayMessage::system(
                        "Claude access restored. Use /model to switch if desired.",
                    ));
                }
                self.trigger_provider_auth_changed(
                    Some(&login.provider),
                    prefer_strongest,
                    !profile_activation_owns_selection && !require_model_confirmation,
                );
            }
            // First-run onboarding: once the user has authenticated on a fresh
            // install, walk them through model selection -> continue/suggestions.
            self.maybe_begin_onboarding_flow_after_login();
        } else {
            let message = crate::auth::login_diagnostics::augment_auth_error_message(
                &login.provider,
                &login.message,
            );
            // During onboarding we route the failure to the recovery screen
            // (which explains next steps) instead of dumping a raw error message
            // and a status notice the user can miss.
            let replaced_login_card = auth_progress::login_card_title_for_provider(&login.provider)
                .is_some_and(|title| {
                    let retry = auth_progress::login_retry_command_for_provider(&login.provider);
                    self.finish_login_progress(
                        title,
                        format!("{title} failed: {message}\nRetry {retry}."),
                    )
                });
            if self.onboarding_flow_active() {
                self.onboarding_handle_login_failed(Some(message));
            } else {
                if !replaced_login_card {
                    self.push_display_message(DisplayMessage::error(message));
                }
                self.set_status_notice(format!("Login: {} failed", login.provider));
                self.onboarding_handle_login_failed(None);
            }
        }
        if !matches!(
            self.pending_login,
            Some(PendingLogin::MuseSubscription { .. })
        ) && self
            .pending_login
            .as_ref()
            .and_then(PendingLogin::telemetry_context)
            .is_some_and(|(provider, _)| provider == login.provider)
        {
            if let Some(PendingLogin::OpenAiAccount {
                device_fallback_cancelled: Some(cancelled),
                ..
            }) = &self.pending_login
            {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            }
            self.pending_login = None;
        }
    }

    async fn claude_token_exchange(
        verifier: String,
        input: String,
        label: &str,
        redirect_uri: Option<String>,
    ) -> Result<String, String> {
        let fallback_redirect_uri =
            redirect_uri.unwrap_or_else(|| crate::auth::oauth::claude::REDIRECT_URI.to_string());
        let redirect_uri =
            crate::auth::oauth::claude_redirect_uri_for_input(input.trim(), &fallback_redirect_uri);
        let oauth_tokens =
            crate::auth::oauth::exchange_claude_code(&verifier, input.trim(), &redirect_uri)
                .await
                .map_err(|e| e.to_string())?;

        crate::auth::oauth::save_claude_tokens_for_account(&oauth_tokens, label)
            .map_err(|e| format!("Failed to save tokens: {}", e))?;

        let profile_suffix = match crate::auth::oauth::update_claude_account_profile(
            label,
            &oauth_tokens.access_token,
        )
        .await
        {
            Ok(Some(email)) => format!(" (email: {})", mask_email(&email)),
            Ok(None) => String::new(),
            Err(e) => {
                crate::logging::warn(&format!(
                    "Claude login [{}] profile fetch failed: {}",
                    label, e
                ));
                String::new()
            }
        };

        Ok(format!(
            "Successfully logged in to Claude! (account: {}){}",
            label, profile_suffix
        ))
    }
}

#[path = "auth/login_completion.rs"]
mod login_completion;

impl App {
    fn begin_pending_login(&mut self, pending: PendingLogin) {
        if let Some((provider, method)) = pending.telemetry_context() {
            crate::telemetry::record_auth_started(&provider, &method);
        }
        self.pending_login = Some(pending);
    }
}

#[path = "auth/login_start.rs"]
mod login_start;

impl App {
    pub(super) fn open_auth_browser(url: &str) -> bool {
        // Honors --no-browser/NO_BROWSER/JCODE_NO_BROWSER and never opens real
        // browser windows from test binaries (login flows are exercised by TUI
        // tests; without this guard a test run pops OAuth pages on the
        // developer's desktop).
        super::helpers::open_path_or_url_detached(url).is_ok()
    }

    fn record_oauth_preflight(
        provider_id: &str,
        browser_opened: bool,
        callback_target: Option<&str>,
        callback_available: Option<bool>,
    ) -> String {
        let mut notices = Vec::new();
        if !browser_opened {
            crate::telemetry::record_auth_surface_blocked_reason(
                provider_id,
                "oauth",
                crate::auth::login_diagnostics::AuthFailureReason::BrowserOpenFailed.label(),
            );
            notices.push("This machine could not open a browser automatically.".to_string());
        }
        if matches!(callback_available, Some(false)) {
            crate::telemetry::record_auth_surface_blocked_reason(
                provider_id,
                "oauth",
                crate::auth::login_diagnostics::AuthFailureReason::CallbackPortUnavailable.label(),
            );
            if let Some(target) = callback_target {
                notices.push(format!(
                    "Local callback target {} is unavailable, so jcode is using manual-safe paste completion instead.",
                    target
                ));
            } else {
                notices.push(
                    "The local callback listener is unavailable, so jcode is using manual-safe paste completion instead."
                        .to_string(),
                );
            }
        }
        if !notices.is_empty() {
            notices.push(format!(
                "If login still fails, run jcode auth doctor {} for a guided diagnosis.",
                provider_id
            ));
        }
        notices.join("\n")
    }

    pub(super) fn show_auth_status(&mut self) {
        let status = crate::auth::AuthStatus::check();
        let validation = crate::auth::validation::load_all();
        let icon = |state: crate::auth::AuthState| match state {
            crate::auth::AuthState::Available => "ok",
            crate::auth::AuthState::Expired => "needs attention",
            crate::auth::AuthState::NotConfigured => "not configured",
        };
        let providers = crate::provider_catalog::auth_status_login_providers();
        let mut rows: Vec<[String; 5]> = vec![[
            "Provider".to_string(),
            "Status".to_string(),
            "Method".to_string(),
            "Health".to_string(),
            "Validation".to_string(),
        ]];
        for provider in providers {
            let assessment = status.assessment_for_provider(provider);
            rows.push([
                provider.display_name.to_string(),
                icon(assessment.state).to_string(),
                assessment.method_detail.to_string(),
                assessment.health_summary(),
                validation
                    .get(provider.id)
                    .map(crate::auth::validation::format_record_label)
                    .unwrap_or_else(|| "not validated".to_string()),
            ]);
        }
        let mut widths = [0usize; 5];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut message = String::from("Authentication Status:\n\n");
        for row in &rows {
            let line = row
                .iter()
                .enumerate()
                .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
                .collect::<Vec<_>>()
                .join("  ");
            message.push_str(line.trim_end());
            message.push('\n');
        }
        message.push_str(
            "\nUse /login <provider> to authenticate. /login jcode is for curated jcode subscription access; /account opens the provider/account management center, /account <provider> settings shows provider-specific controls, and /auth doctor or /account <provider> doctor shows recovery steps.",
        );
        self.push_display_message(DisplayMessage::system(message));
    }

    pub(super) fn show_interactive_login(&mut self) {
        crate::telemetry::record_setup_step_once("login_picker_opened");
        self.open_login_picker_inline();
        self.set_status_notice("Login: choose a provider");
    }

    pub(super) fn show_interactive_logout(&mut self) {
        self.open_logout_picker_inline();
        self.set_status_notice("Logout: choose a provider");
    }

    pub(super) fn start_logout_provider(
        &mut self,
        provider: crate::provider_catalog::LoginProviderDescriptor,
    ) {
        use crate::provider_catalog::LoginProviderTarget;

        if matches!(provider.target, LoginProviderTarget::Jcode) {
            self.start_jcode_account_logout();
            return;
        }

        let result: anyhow::Result<String> = (|| match provider.target {
            LoginProviderTarget::Jcode => unreachable!("handled above"),
            LoginProviderTarget::Claude => {
                let removed = crate::auth::claude::clear_accounts()?;
                Ok(format!("Logged out of {} Anthropic account(s).", removed))
            }
            LoginProviderTarget::ClaudeApiKey => {
                Self::clear_api_key_login("ANTHROPIC_API_KEY", "anthropic.env")?;
                Ok("Logged out of Anthropic API key.".to_string())
            }
            LoginProviderTarget::OpenAi => {
                let removed = crate::auth::codex::clear_accounts()?;
                Ok(format!("Logged out of {} OpenAI account(s).", removed))
            }
            LoginProviderTarget::OpenAiApiKey => {
                Self::clear_api_key_login("OPENAI_API_KEY", "openai.env")?;
                Ok("Logged out of OpenAI API key.".to_string())
            }
            LoginProviderTarget::OpenRouter => {
                Self::clear_api_key_login("OPENROUTER_API_KEY", "openrouter.env")?;
                Ok("Logged out of OpenRouter API key.".to_string())
            }
            LoginProviderTarget::Bedrock => {
                Self::clear_api_key_login(
                    crate::provider::bedrock::API_KEY_ENV,
                    crate::provider::bedrock::ENV_FILE,
                )?;
                Ok("Logged out of Bedrock API key.".to_string())
            }
            LoginProviderTarget::Azure => {
                Self::clear_api_key_login(
                    crate::auth::azure::API_KEY_ENV,
                    crate::auth::azure::ENV_FILE,
                )?;
                crate::provider_catalog::save_env_value_to_env_file(
                    crate::auth::azure::USE_ENTRA_ENV,
                    crate::auth::azure::ENV_FILE,
                    None,
                )?;
                Ok("Logged out of Azure OpenAI API key / Entra configuration.".to_string())
            }
            LoginProviderTarget::OpenAiCompatible(profile) => {
                // Native Muse subscription credentials live in auth::muse's own
                // JSON store, not in profile env files; clear them there.
                if auth_muse::is_muse_subscription_profile(profile) {
                    auth_muse::logout_provider()
                } else {
                    let resolved =
                        crate::provider_catalog::resolve_openai_compatible_profile(profile);
                    Self::clear_api_key_login(&resolved.api_key_env, &resolved.env_file)?;
                    crate::provider_catalog::save_env_value_to_env_file(
                        crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
                        &resolved.env_file,
                        None,
                    )?;
                    Ok(format!("Logged out of {} API key.", resolved.display_name))
                }
            }
            LoginProviderTarget::Cursor => {
                crate::auth::cursor::clear_api_key()?;
                Ok("Logged out of Cursor API key.".to_string())
            }
            LoginProviderTarget::Gemini => {
                crate::auth::gemini::clear_tokens()?;
                Ok("Logged out of Gemini.".to_string())
            }
            _ => Ok(format!(
                "Logout for {} is not automated yet. Remove its saved API key or external CLI session from /account {} settings.",
                provider.display_name, provider.id
            )),
        })();

        match result {
            Ok(message) => {
                crate::auth::AuthStatus::invalidate_cache();
                self.push_display_message(DisplayMessage::system(message));
                self.set_status_notice(format!("Logout: {}", provider.display_name));
            }
            Err(err) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to log out of {}: {}",
                    provider.display_name, err
                )));
                self.set_status_notice("Logout failed");
            }
        }
    }

    pub(super) fn start_logout_all(&mut self) {
        let mut summary: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        match crate::auth::claude::clear_accounts() {
            Ok(removed) if removed > 0 => summary.push(format!("{} Anthropic account(s)", removed)),
            Ok(_) => {}
            Err(err) => errors.push(format!("Anthropic: {}", err)),
        }
        match crate::auth::codex::clear_accounts() {
            Ok(removed) if removed > 0 => summary.push(format!("{} OpenAI account(s)", removed)),
            Ok(_) => {}
            Err(err) => errors.push(format!("OpenAI: {}", err)),
        }

        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "jcode subscription API key",
            crate::subscription_catalog::JCODE_API_KEY_ENV,
            crate::subscription_catalog::JCODE_ENV_FILE,
        );
        for env_key in [
            crate::subscription_catalog::JCODE_API_BASE_ENV,
            crate::subscription_catalog::JCODE_ACCOUNT_ID_ENV,
            crate::subscription_catalog::JCODE_ACCOUNT_EMAIL_ENV,
            crate::subscription_catalog::JCODE_TIER_ENV,
        ] {
            if let Err(err) = crate::provider_catalog::save_env_value_to_env_file(
                env_key,
                crate::subscription_catalog::JCODE_ENV_FILE,
                None,
            ) {
                errors.push(format!("jcode subscription {}: {}", env_key, err));
            }
        }

        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "Anthropic API key",
            "ANTHROPIC_API_KEY",
            "anthropic.env",
        );
        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "OpenAI API key",
            "OPENAI_API_KEY",
            "openai.env",
        );
        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            "openrouter.env",
        );
        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "Bedrock API key",
            crate::provider::bedrock::API_KEY_ENV,
            crate::provider::bedrock::ENV_FILE,
        );
        Self::clear_api_key_logout_summary(
            &mut summary,
            &mut errors,
            "Azure OpenAI API key",
            crate::auth::azure::API_KEY_ENV,
            crate::auth::azure::ENV_FILE,
        );
        if let Err(err) = crate::provider_catalog::save_env_value_to_env_file(
            crate::auth::azure::USE_ENTRA_ENV,
            crate::auth::azure::ENV_FILE,
            None,
        ) {
            errors.push(format!("Azure OpenAI Entra config: {}", err));
        }
        for profile in crate::provider_catalog::openai_compatible_profiles() {
            // Native Muse subscription credentials live in auth::muse's own JSON
            // store, not in a profile env file; clear them there instead of the
            // generic API-key path.
            if auth_muse::is_muse_subscription_profile(*profile) {
                auth_muse::logout_all(&mut summary, &mut errors);
                continue;
            }
            let resolved = crate::provider_catalog::resolve_openai_compatible_profile(*profile);
            Self::clear_api_key_logout_summary(
                &mut summary,
                &mut errors,
                &format!("{} API key", resolved.display_name),
                &resolved.api_key_env,
                &resolved.env_file,
            );
            if let Err(err) = crate::provider_catalog::save_env_value_to_env_file(
                crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
                &resolved.env_file,
                None,
            ) {
                errors.push(format!(
                    "{} local endpoint config: {}",
                    resolved.display_name, err
                ));
            }
        }
        let cursor_configured = crate::auth::cursor::load_api_key().is_ok();
        match crate::auth::cursor::clear_api_key() {
            Ok(()) if cursor_configured => summary.push("Cursor API key".to_string()),
            Ok(()) => {}
            Err(err) => errors.push(format!("Cursor API key: {}", err)),
        }
        match crate::auth::gemini::clear_tokens() {
            Ok(()) => summary.push("Gemini".to_string()),
            Err(err) => errors.push(format!("Gemini: {}", err)),
        }

        crate::auth::AuthStatus::invalidate_cache();

        let message = if summary.is_empty() {
            "No automated logins to clear.".to_string()
        } else {
            format!("Logged out of: {}.", summary.join(", "))
        };
        self.push_display_message(DisplayMessage::system(message));

        if errors.is_empty() {
            self.set_status_notice("Logout: all providers");
        } else {
            self.push_display_message(DisplayMessage::error(format!(
                "Some logouts failed: {}",
                errors.join("; ")
            )));
            self.set_status_notice("Logout: completed with errors");
        }
    }

    fn clear_api_key_login(env_key: &str, env_file: &str) -> anyhow::Result<()> {
        crate::provider_catalog::save_env_value_to_env_file(env_key, env_file, None)
    }

    fn clear_api_key_logout_summary(
        summary: &mut Vec<String>,
        errors: &mut Vec<String>,
        label: &str,
        env_key: &str,
        env_file: &str,
    ) {
        let configured =
            crate::provider_catalog::load_env_value_from_env_or_config(env_key, env_file).is_some();
        match Self::clear_api_key_login(env_key, env_file) {
            Ok(()) if configured => summary.push(label.to_string()),
            Ok(()) => {}
            Err(err) => errors.push(format!("{}: {}", label, err)),
        }
    }

    pub(super) fn handle_login_input(&mut self, pending: PendingLogin, input: String) {
        let trimmed = input.trim();
        if trimmed == "/cancel" {
            if matches!(&pending, PendingLogin::Exchanging { .. }) {
                self.set_status_notice("Login exchange is running. Wait for completion.");
                self.pending_login = Some(pending);
                return;
            }
            // Signal the Muse subscription background task (if any) and drop
            // its credential slot: no late progress, error, or approval event
            // is accepted afterwards, and nothing is ever persisted.
            if let PendingLogin::MuseSubscription {
                cancelled,
                approved,
                ..
            } = &pending
            {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                if let Ok(mut slot) = approved.lock() {
                    *slot = None;
                }
            }
            if let PendingLogin::OpenAiAccount {
                device_fallback_cancelled: Some(cancelled),
                ..
            } = &pending
            {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            }
            if let Some((provider, method)) = pending.telemetry_context() {
                crate::telemetry::record_auth_cancelled(&provider, &method);
            }
            let replaced =
                auth_progress::login_card_title_for_pending(&pending).is_some_and(|title| {
                    self.finish_login_progress(title, format!("{title} cancelled."))
                });
            if !replaced {
                self.push_display_message(DisplayMessage::system("Login cancelled.".to_string()));
            }
            self.set_status_notice("Login cancelled");
            return;
        }

        if trimmed.is_empty()
            && !matches!(
                &pending,
                PendingLogin::ApiKeyProfile {
                    api_key_optional: true,
                    ..
                }
            )
        {
            let help = match &pending {
                PendingLogin::AutoImportSelection { .. } => {
                    "Auto import is waiting for your selection. Reply with a to approve all, 1,3 to approve specific sources, or /cancel to abort.".to_string()
                }
                PendingLogin::AutoApiKey => {
                    "Paste an API key below, or type /cancel to abort.".to_string()
                }
                PendingLogin::ApiKeyChooseProvider { .. } => {
                    "Reply with a provider number or name (for example: openai-api, deepseek, mistral), or /cancel.".to_string()
                }
                PendingLogin::ApiKeyProfile { .. } | PendingLogin::CursorApiKey => {
                    "Paste your API key below, or type /cancel to abort.".to_string()
                }
                _ => "Login still in progress. Complete it in your browser, or paste the callback URL / authorization code here. Type /cancel to abort.".to_string(),
            };
            self.push_display_message(DisplayMessage::system(help));
            self.pending_login = Some(pending);
            return;
        }

        match &pending {
            PendingLogin::OpenAiAccount { .. } if !looks_like_oauth_callback_input(trimmed) => {
                self.push_display_message(DisplayMessage::system(
                    "Still waiting for the browser callback. Paste the full callback URL or query string if you want to finish manually, or keep waiting for the automatic redirect.".to_string(),
                ));
                self.pending_login = Some(pending);
                return;
            }
            PendingLogin::Antigravity { .. } if !looks_like_oauth_callback_input(trimmed) => {
                self.push_display_message(DisplayMessage::system(
                    "Still waiting for the browser callback. Paste the full callback URL or query string if you want to finish manually, or keep waiting for the automatic redirect.".to_string(),
                ));
                self.pending_login = Some(pending);
                return;
            }
            _ => {}
        }

        match pending {
            PendingLogin::Exchanging { provider } => {
                self.set_status_notice("Login only: exchanging credentials, input not sent to AI");
                self.pending_login = Some(PendingLogin::Exchanging { provider });
            }
            PendingLogin::Remote => {
                // SSH input must never fall through to laptop credential handlers.
                self.append_ssh_login_input(&input);
            }
            PendingLogin::ClaudeAccount {
                verifier,
                label,
                redirect_uri,
            } => {
                self.pending_login = Some(PendingLogin::Exchanging {
                    provider: "claude".to_string(),
                });
                self.set_status_notice(format!("Login [{}]: exchanging...", label));
                let input_owned = input.clone();
                let label_clone = label.clone();
                tokio::spawn(async move {
                    match Self::claude_token_exchange(
                        verifier,
                        input_owned,
                        &label_clone,
                        redirect_uri,
                    )
                    .await
                    {
                        Ok(msg) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "claude".to_string(),
                                success: true,
                                message: msg,
                            }));
                        }
                        Err(e) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "claude".to_string(),
                                success: false,
                                message: format!("Claude login [{}] failed: {}", label_clone, e),
                            }));
                        }
                    }
                });
                self.update_login_progress_exchange(
                    auth_progress::CLAUDE_LOGIN_TITLE,
                    Some(&label),
                );
            }
            PendingLogin::OpenAiAccount {
                verifier,
                label,
                expected_state,
                redirect_uri,
                device_fallback_cancelled,
            } => {
                if let Some(cancelled) = device_fallback_cancelled {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                }
                self.pending_login = Some(PendingLogin::Exchanging {
                    provider: "openai".to_string(),
                });
                self.set_status_notice(format!("Login [{}]: exchanging...", label));
                let input_owned = input.clone();
                let label_clone = label.clone();
                tokio::spawn(async move {
                    match Self::openai_token_exchange(
                        verifier,
                        input_owned,
                        Some(label_clone.clone()),
                        Some(expected_state),
                        &redirect_uri,
                    )
                    .await
                    {
                        Ok(msg) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "openai".to_string(),
                                success: true,
                                message: msg,
                            }));
                        }
                        Err(e) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "openai".to_string(),
                                success: false,
                                message: format!("OpenAI login [{}] failed: {}", label_clone, e),
                            }));
                        }
                    }
                });
                self.update_openai_login_exchange(&label);
            }
            PendingLogin::Gemini {
                verifier,
                expected_state,
                redirect_uri,
            } => {
                self.pending_login = Some(PendingLogin::Exchanging {
                    provider: "gemini".to_string(),
                });
                self.set_status_notice("Login: exchanging...");
                let input_owned = input.clone();
                tokio::spawn(async move {
                    match crate::auth::gemini::exchange_callback_input(
                        &verifier,
                        input_owned.trim(),
                        expected_state.as_deref(),
                        &redirect_uri,
                    )
                    .await
                    {
                        Ok(tokens) => {
                            let msg = if let Some(email) = tokens.email {
                                format!("Successfully logged in to Gemini! (account: {})", email)
                            } else {
                                "Successfully logged in to Gemini!".to_string()
                            };
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "gemini".to_string(),
                                success: true,
                                message: msg,
                            }));
                        }
                        Err(e) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "gemini".to_string(),
                                success: false,
                                message: format!("Gemini login failed: {}", e),
                            }));
                        }
                    }
                });
                self.update_login_progress_exchange(auth_progress::GEMINI_LOGIN_TITLE, None);
            }
            PendingLogin::Antigravity {
                verifier,
                expected_state,
                redirect_uri,
            } => {
                self.pending_login = Some(PendingLogin::Exchanging {
                    provider: "antigravity".to_string(),
                });
                self.set_status_notice("Login: exchanging...");
                let input_owned = input.clone();
                tokio::spawn(async move {
                    match Self::antigravity_token_exchange(
                        verifier,
                        input_owned,
                        Some(expected_state),
                        redirect_uri,
                    )
                    .await
                    {
                        Ok(msg) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "antigravity".to_string(),
                                success: true,
                                message: msg,
                            }));
                        }
                        Err(e) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "antigravity".to_string(),
                                success: false,
                                message: format!("Antigravity login failed: {}", e),
                            }));
                        }
                    }
                });
                self.update_login_progress_exchange(auth_progress::ANTIGRAVITY_LOGIN_TITLE, None);
            }
            PendingLogin::AutoApiKey => {
                let key = crate::provider_catalog::normalize_pasted_api_key(&input);
                if key.is_empty() {
                    self.push_display_message(DisplayMessage::error(
                        "API key cannot be empty.".to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::AutoApiKey);
                    return;
                }
                if key.len() < 8 && key.chars().all(|c| c.is_ascii_digit()) {
                    self.push_display_message(DisplayMessage::error(format!(
                        "'{}' looks like a menu selection, not an API key. Paste the API key itself, or /cancel.",
                        key
                    )));
                    self.pending_login = Some(PendingLogin::AutoApiKey);
                    return;
                }
                match crate::provider_catalog::detect_api_key_provider(&key) {
                    crate::provider_catalog::ApiKeyDetection::Matched(provider) => {
                        self.validate_and_save_detected_api_key(provider, key, true);
                    }
                    crate::provider_catalog::ApiKeyDetection::Ambiguous { prefix, candidates } => {
                        let choices =
                            crate::provider_catalog::format_api_key_candidates(candidates);
                        self.push_display_message(DisplayMessage::system(format!(
                            "Keys starting with `{prefix}` are used by multiple providers:\n{choices}\n\nReply with a number or any provider name, or /cancel."
                        )));
                        self.set_status_notice("Login: pick provider for key...");
                        self.pending_login =
                            Some(PendingLogin::ApiKeyChooseProvider { key, candidates });
                    }
                    crate::provider_catalog::ApiKeyDetection::Unknown => {
                        self.push_display_message(DisplayMessage::system(
                            "Couldn't detect the provider from that key automatically.\nReply with the provider name (e.g. mistral, deepseek, openrouter, cerebras, bedrock), or /cancel."
                                .to_string(),
                        ));
                        self.set_status_notice("Login: enter provider name...");
                        self.pending_login = Some(PendingLogin::ApiKeyChooseProvider {
                            key,
                            candidates: &[],
                        });
                    }
                }
            }
            PendingLogin::ApiKeyChooseProvider { key, candidates } => {
                match crate::provider_catalog::resolve_api_key_provider_input(trimmed, candidates) {
                    Some(provider) => {
                        self.validate_and_save_detected_api_key(provider, key, false);
                    }
                    None => {
                        let hint = if candidates.is_empty() {
                            "Enter an API-key provider name (e.g. openai-api, anthropic-api, deepseek, mistral, openrouter), or /cancel.".to_string()
                        } else {
                            format!(
                                "Enter 1-{} or a provider name ({}), or /cancel.",
                                candidates.len(),
                                crate::provider_catalog::format_api_key_candidates(candidates)
                            )
                        };
                        self.push_display_message(DisplayMessage::error(format!(
                            "Unknown API-key provider '{trimmed}'. {hint}"
                        )));
                        self.pending_login =
                            Some(PendingLogin::ApiKeyChooseProvider { key, candidates });
                    }
                }
            }
            PendingLogin::ApiKeyProfile {
                provider_id,
                provider,
                auth_method,
                docs_url,
                env_file,
                key_name,
                default_model,
                endpoint,
                api_key_optional,
                openai_compatible_profile,
            } => {
                let key = crate::provider_catalog::normalize_pasted_api_key(&input);
                if key.is_empty() && !api_key_optional {
                    self.push_display_message(DisplayMessage::error(
                        "API key cannot be empty.".to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::ApiKeyProfile {
                        provider_id,
                        provider,
                        auth_method,
                        docs_url,
                        env_file,
                        key_name,
                        default_model,
                        endpoint,
                        api_key_optional,
                        openai_compatible_profile,
                    });
                    return;
                }
                // Real API keys are never short digit strings. Users sometimes
                // type a menu number like `1` here, trying to select from a
                // numbered list shown earlier; silently saving that as the key
                // bricks the provider until they log in again (issue #496).
                if !key.is_empty() && key.len() < 8 && key.chars().all(|c| c.is_ascii_digit()) {
                    self.push_display_message(DisplayMessage::error(format!(
                        "'{}' looks like a menu selection, not an API key. This prompt is waiting for the {} API key itself. Paste the key (see {}), or type /cancel to abort.",
                        key, provider, docs_url
                    )));
                    self.pending_login = Some(PendingLogin::ApiKeyProfile {
                        provider_id,
                        provider,
                        auth_method,
                        docs_url,
                        env_file,
                        key_name,
                        default_model,
                        endpoint,
                        api_key_optional,
                        openai_compatible_profile,
                    });
                    return;
                }
                if key_name == "OPENROUTER_API_KEY" && !key.starts_with("sk-or-") {
                    self.push_display_message(DisplayMessage::system(
                        "OpenRouter keys typically start with sk-or-. Saving anyway...".to_string(),
                    ));
                }

                let resolved_openai_compatible = openai_compatible_profile
                    .map(crate::provider_catalog::resolve_openai_compatible_profile);

                // Record the key-save attempt before touching disk. This is the
                // single most important breadcrumb for issue #312 ("paste API
                // key for OpenAI-compatible/opencode silently returns to menu"):
                // it proves the input was received and which env var/file jcode
                // tried to write, without logging the key itself.
                crate::logging::event_info(
                    "login_api_key_save_attempt",
                    vec![
                        ("provider_id", provider_id.clone()),
                        ("provider", provider.clone()),
                        ("auth_method", auth_method.clone()),
                        ("env_var", key_name.clone()),
                        ("env_file", env_file.clone()),
                        ("input_len", key.len().to_string()),
                        ("optional", api_key_optional.to_string()),
                        (
                            "openai_compatible",
                            openai_compatible_profile.is_some().to_string(),
                        ),
                    ],
                );

                let save_result: anyhow::Result<()> = if let Some(resolved) =
                    resolved_openai_compatible.as_ref()
                {
                    (|| {
                        if resolved.requires_api_key {
                            crate::provider_catalog::save_env_value_to_env_file(
                                crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
                                &resolved.env_file,
                                None,
                            )?;
                            crate::provider_catalog::save_named_api_key(
                                &resolved.env_file,
                                &resolved.api_key_env,
                                key.trim(),
                            )
                        } else {
                            crate::provider_catalog::save_env_value_to_env_file(
                                crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
                                &resolved.env_file,
                                Some("1"),
                            )?;
                            if key.trim().is_empty() {
                                crate::provider_catalog::save_env_value_to_env_file(
                                    &resolved.api_key_env,
                                    &resolved.env_file,
                                    None,
                                )
                            } else {
                                crate::provider_catalog::save_named_api_key(
                                    &resolved.env_file,
                                    &resolved.api_key_env,
                                    key.trim(),
                                )
                            }
                        }
                    })()
                } else if key_name == crate::subscription_catalog::JCODE_API_KEY_ENV {
                    (|| {
                        let mut content = format!("{}={}\n", key_name, key);
                        if let Some(base) = crate::subscription_catalog::configured_api_base() {
                            content.push_str(&format!(
                                "{}={}\n",
                                crate::subscription_catalog::JCODE_API_BASE_ENV,
                                base
                            ));
                        }

                        let config_dir = crate::storage::app_config_dir()?;
                        std::fs::create_dir_all(&config_dir)?;
                        crate::platform::set_directory_permissions_owner_only(&config_dir)?;

                        let file_path = config_dir.join(&env_file);
                        std::fs::write(&file_path, content)?;
                        crate::platform::set_permissions_owner_only(&file_path)?;
                        Ok(())
                    })()
                } else if key_name == crate::provider::bedrock::API_KEY_ENV {
                    (|| {
                        crate::provider_catalog::save_named_api_key(&env_file, &key_name, &key)?;
                        crate::provider_catalog::save_env_value_to_env_file(
                            crate::provider::bedrock::REGION_ENV,
                            &env_file,
                            Some("us-east-2"),
                        )
                    })()
                } else {
                    crate::provider_catalog::save_named_api_key(&env_file, &key_name, &key)
                };

                match save_result {
                    Ok(()) => {
                        crate::auth::AuthStatus::invalidate_cache();
                        crate::logging::event_info(
                            "login_api_key_saved",
                            vec![
                                ("provider_id", provider_id.clone()),
                                ("provider", provider.clone()),
                                ("env_var", key_name.clone()),
                                ("env_file", env_file.clone()),
                                (
                                    "openai_compatible",
                                    openai_compatible_profile.is_some().to_string(),
                                ),
                            ],
                        );
                        if key_name == crate::provider::bedrock::API_KEY_ENV {
                            crate::provider::activation::select_initial_runtime_provider_key(
                                "bedrock",
                            );
                            if let Some(default_model) = default_model.as_deref() {
                                crate::env::set_var("JCODE_BEDROCK_MODEL", default_model);
                            }
                        }

                        if let Some(profile) = openai_compatible_profile {
                            crate::provider_catalog::apply_openai_compatible_profile_env(Some(
                                profile,
                            ));
                            self.start_openai_compatible_post_login_activation(
                                profile.id.to_string(),
                                provider.clone(),
                            );
                        }

                        let effective_default_model = resolved_openai_compatible
                            .as_ref()
                            .and_then(|resolved| resolved.default_model.as_deref())
                            .or(default_model.as_deref());
                        let model_hint = effective_default_model
                            .map(|m| format!("\nSuggested default model: {}", m))
                            .unwrap_or_default();
                        let guidance = if key_name == crate::subscription_catalog::JCODE_API_KEY_ENV
                        {
                            format!(
                                "Use /login jcode to access curated models via your router. If the model list looks stale, run /refresh-model-list.\nDocs: {}",
                                docs_url
                            )
                        } else if let Some(resolved) = resolved_openai_compatible.as_ref() {
                            if resolved.requires_api_key {
                                "Fetching models now. Jcode will switch to an accessible model returned by the live catalog and show the catalog diff when discovery finishes. If the model list looks stale, run /refresh-model-list.".to_string()
                            } else {
                                format!(
                                    "Local endpoint configured at {}. Fetching models now; Jcode will switch to an accessible model returned by the live catalog and show the catalog diff when discovery finishes. If the model list looks stale, run /refresh-model-list.",
                                    endpoint.as_deref().unwrap_or(resolved.api_base.as_str()),
                                )
                            }
                        } else if key_name == crate::provider::bedrock::API_KEY_ENV {
                            "You can now use /model to switch to Bedrock models. TUI onboarding saved region us-east-2; for a different region, run jcode login --provider bedrock from a terminal.".to_string()
                        } else if key_name == "OPENROUTER_API_KEY" {
                            "Refreshing models automatically. Use /model to switch to OpenRouter models.".to_string()
                        } else {
                            "Refreshing models automatically. Use /model to pick a model."
                                .to_string()
                        };
                        let saved_label =
                            if let Some(resolved) = resolved_openai_compatible.as_ref() {
                                if resolved.requires_api_key {
                                    format!("✓ {} API key saved", provider)
                                } else if key.trim().is_empty() {
                                    format!("✓ {} local endpoint saved", provider)
                                } else {
                                    format!("✓ {} local endpoint and API key saved", provider)
                                }
                            } else {
                                format!("✓ {} API key saved", provider)
                            };
                        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                            provider: provider.clone(),
                            success: true,
                            message: format!("{}.\n{}{}", saved_label, guidance, model_hint),
                        }));
                    }
                    Err(e) => {
                        let reason = crate::auth::login_diagnostics::classify_auth_failure_message(
                            &e.to_string(),
                        );
                        crate::logging::event_error(
                            "login_api_key_save_failed",
                            vec![
                                ("provider_id", provider_id.clone()),
                                ("provider", provider.clone()),
                                ("env_var", key_name.clone()),
                                ("env_file", env_file.clone()),
                                ("reason", reason.label().to_string()),
                                ("error", e.to_string()),
                            ],
                        );
                        crate::telemetry::record_auth_failed_reason(
                            &provider_id,
                            &auth_method,
                            reason.label(),
                        );
                        self.push_display_message(DisplayMessage::error(format!(
                            "Failed to save {} key: {}",
                            provider, e
                        )));
                        self.pending_login = Some(PendingLogin::ApiKeyProfile {
                            provider_id,
                            provider,
                            auth_method,
                            docs_url,
                            env_file,
                            key_name,
                            default_model,
                            endpoint,
                            api_key_optional,
                            openai_compatible_profile,
                        });
                    }
                }
            }
            PendingLogin::OpenAiCompatibleApiBase { profile } => {
                let api_base = input.trim();
                if !api_base.is_empty() {
                    let normalized = match crate::provider_catalog::normalize_api_base(api_base) {
                        Some(value) => value,
                        None => {
                            self.push_display_message(DisplayMessage::error(
                                "OpenAI-compatible API base must be https://..., or http:// with a localhost/.local name or a bare IP address."
                                    .to_string(),
                            ));
                            self.pending_login =
                                Some(PendingLogin::OpenAiCompatibleApiBase { profile });
                            return;
                        }
                    };
                    if let Err(err) = crate::provider_catalog::save_env_value_to_env_file(
                        "JCODE_OPENAI_COMPAT_API_BASE",
                        crate::provider_catalog::OPENAI_COMPAT_PROFILE.env_file,
                        Some(&normalized),
                    ) {
                        self.push_display_message(DisplayMessage::error(format!(
                            "Failed to save OpenAI-compatible API base: {}",
                            err
                        )));
                        self.pending_login =
                            Some(PendingLogin::OpenAiCompatibleApiBase { profile });
                        return;
                    }
                }
                self.start_openai_compatible_key_login(profile);
            }
            PendingLogin::AzureEndpoint => {
                let endpoint_raw = input.trim();
                let Some(endpoint) = crate::auth::azure::normalize_endpoint(endpoint_raw) else {
                    self.push_display_message(DisplayMessage::error(
                        "Invalid Azure OpenAI endpoint. Use https://<resource>.openai.azure.com or the full /openai/v1 URL."
                            .to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::AzureEndpoint);
                    return;
                };
                self.push_display_message(DisplayMessage::system(
                    "Azure endpoint accepted. Now enter the Azure deployment/model name, for example gpt-4.1-nano."
                        .to_string(),
                ));
                self.set_status_notice("Login: Azure model...");
                self.pending_login = Some(PendingLogin::AzureModel { endpoint });
            }
            PendingLogin::AzureModel { endpoint } => {
                let model = input.trim().to_string();
                if model.is_empty() {
                    self.push_display_message(DisplayMessage::error(
                        "Azure deployment/model name cannot be empty.".to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::AzureModel { endpoint });
                    return;
                }
                self.push_display_message(DisplayMessage::system(
                    "Authentication method:\n\n\
                     1 Microsoft Entra ID via DefaultAzureCredential, for example az login\n\
                     2 Azure OpenAI API key\n\n\
                     Enter 1 or 2 [1]."
                        .to_string(),
                ));
                self.set_status_notice("Login: Azure auth method...");
                self.pending_login = Some(PendingLogin::AzureAuthChoice { endpoint, model });
            }
            PendingLogin::AzureAuthChoice { endpoint, model } => {
                let choice = input.trim();
                let use_entra = match choice {
                    "" | "1" => true,
                    "2" => false,
                    other
                        if other.eq_ignore_ascii_case("entra")
                            || other.eq_ignore_ascii_case("oauth") =>
                    {
                        true
                    }
                    other
                        if other.eq_ignore_ascii_case("key")
                            || other.eq_ignore_ascii_case("api-key") =>
                    {
                        false
                    }
                    _ => {
                        self.push_display_message(DisplayMessage::error(
                            "Invalid auth choice. Enter 1 for Entra ID or 2 for API key."
                                .to_string(),
                        ));
                        self.pending_login =
                            Some(PendingLogin::AzureAuthChoice { endpoint, model });
                        return;
                    }
                };
                if use_entra {
                    match Self::save_azure_config(&endpoint, &model, true, None) {
                        Ok(()) => self.finish_azure_login(true),
                        Err(err) => {
                            self.push_display_message(DisplayMessage::error(format!(
                                "Failed to save Azure OpenAI configuration: {}",
                                err
                            )));
                            self.pending_login =
                                Some(PendingLogin::AzureAuthChoice { endpoint, model });
                        }
                    }
                } else {
                    self.push_display_message(DisplayMessage::system(
                        "Paste your Azure OpenAI API key, or type /cancel to abort.".to_string(),
                    ));
                    self.set_status_notice("Login: Azure API key...");
                    self.pending_login = Some(PendingLogin::AzureApiKey { endpoint, model });
                }
            }
            PendingLogin::AzureApiKey { endpoint, model } => {
                let key = input.trim().to_string();
                if key.is_empty() {
                    self.push_display_message(DisplayMessage::error(
                        "Azure OpenAI API key cannot be empty.".to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::AzureApiKey { endpoint, model });
                    return;
                }
                match Self::save_azure_config(&endpoint, &model, false, Some(&key)) {
                    Ok(()) => self.finish_azure_login(false),
                    Err(err) => {
                        self.push_display_message(DisplayMessage::error(format!(
                            "Failed to save Azure OpenAI configuration: {}",
                            err
                        )));
                        self.pending_login = Some(PendingLogin::AzureApiKey { endpoint, model });
                    }
                }
            }
            PendingLogin::CursorApiKey => {
                let key = input.trim().to_string();
                if key.is_empty() {
                    self.push_display_message(DisplayMessage::error(
                        "API key cannot be empty.".to_string(),
                    ));
                    self.pending_login = Some(PendingLogin::CursorApiKey);
                    return;
                }

                match crate::auth::cursor::save_api_key(&key) {
                    Ok(()) => {
                        crate::auth::AuthStatus::invalidate_cache();
                        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                            provider: "cursor".to_string(),
                            success: true,
                            message: format!(
                                "Cursor API key saved.\n\n\
                                 Stored at {}.\n\
                                 jcode will use it with the native Cursor HTTPS transport.",
                                crate::storage::app_config_dir()
                                    .expect("config directory resolved while saving Cursor API key")
                                    .join("cursor.env")
                                    .display()
                            ),
                        }));
                    }
                    Err(e) => {
                        let reason = crate::auth::login_diagnostics::classify_auth_failure_message(
                            &e.to_string(),
                        );
                        crate::telemetry::record_auth_failed_reason(
                            "cursor",
                            "api_key",
                            reason.label(),
                        );
                        self.push_display_message(DisplayMessage::error(format!(
                            "Failed to save Cursor API key: {}",
                            e
                        )));
                        self.pending_login = Some(PendingLogin::CursorApiKey);
                    }
                }
            }
            PendingLogin::Copilot => {
                self.push_display_message(DisplayMessage::system(
                    "Copilot login is waiting for browser authorization.\n\
                     Complete the login in your browser, or type /cancel to abort."
                        .to_string(),
                ));
                self.pending_login = Some(PendingLogin::Copilot);
            }
            PendingLogin::MuseSubscription { .. } => {
                self.push_display_message(DisplayMessage::system(
                    "Muse subscription login is waiting for browser authorization.\n\
                     Complete the login in your browser, or type /cancel to abort."
                        .to_string(),
                ));
                self.pending_login = Some(pending);
            }
            PendingLogin::GrokBuild => {
                self.push_display_message(DisplayMessage::system(
                    "Grok Build login is waiting for browser authorization. Complete the xAI login in your browser, or type /cancel to dismiss."
                        .to_string(),
                ));
                self.pending_login = Some(PendingLogin::GrokBuild);
            }
            PendingLogin::AutoImportSelection { candidates } => {
                let selected = match crate::external_auth::parse_external_auth_review_selection(
                    &input,
                    candidates.len(),
                ) {
                    Ok(selected) => selected,
                    Err(err) => {
                        self.push_display_message(DisplayMessage::error(err.to_string()));
                        self.pending_login = Some(PendingLogin::AutoImportSelection { candidates });
                        return;
                    }
                };

                if selected.is_empty() {
                    self.push_display_message(DisplayMessage::system(
                        "Skipped external login import. Run `/login` anytime to choose a provider."
                            .to_string(),
                    ));
                    self.set_status_notice("Login: auto import skipped");
                    return;
                }

                self.set_status_notice("Login: importing approved sources...");
                tokio::spawn(async move {
                    match crate::external_auth::run_external_auth_auto_import_candidates(
                        &candidates,
                        &selected,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            // Auto-import bypasses the manual `pending_login`
                            // telemetry path, so record `auth_success` for each
                            // imported provider to keep the activation funnel
                            // accurate.
                            for (provider, method) in &outcome.imported_auth_labels {
                                crate::telemetry::record_auth_success(provider, method);
                            }
                            let imported_provider = if outcome.imported > 0 {
                                crate::auth::lifecycle::preferred_frontier_auth_provider(
                                    &crate::auth::AuthStatus::check_fast(),
                                )
                                .or_else(|| outcome.preferred_activation_provider())
                                .unwrap_or("auto-import")
                                .to_string()
                            } else {
                                "auto-import".to_string()
                            };
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: imported_provider,
                                success: outcome.imported > 0,
                                message: outcome.render_markdown(),
                            }));
                        }
                        Err(err) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: "auto-import".to_string(),
                                success: false,
                                message: format!("Auto import failed: {}", err),
                            }));
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
fn save_tui_openai_compatible_api_base(
    api_base: &str,
) -> anyhow::Result<crate::provider_catalog::ResolvedOpenAiCompatibleProfile> {
    let trimmed = api_base.trim();
    if !trimmed.is_empty() {
        let normalized = crate::provider_catalog::normalize_api_base(trimmed).ok_or_else(|| {
            anyhow::anyhow!("OpenAI-compatible API base must be https://..., or http:// with a localhost/.local name or a bare IP address.")
        })?;
        crate::provider_catalog::save_env_value_to_env_file(
            "JCODE_OPENAI_COMPAT_API_BASE",
            crate::provider_catalog::OPENAI_COMPAT_PROFILE.env_file,
            Some(&normalized),
        )?;
    }
    Ok(crate::provider_catalog::resolve_openai_compatible_profile(
        crate::provider_catalog::OPENAI_COMPAT_PROFILE,
    ))
}

#[cfg(test)]
fn save_tui_openai_compatible_key(
    profile: crate::provider_catalog::OpenAiCompatibleProfile,
    key: &str,
) -> anyhow::Result<crate::provider_catalog::ResolvedOpenAiCompatibleProfile> {
    let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
    if resolved.requires_api_key {
        crate::provider_catalog::save_env_value_to_env_file(
            crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
            &resolved.env_file,
            None,
        )?;
        crate::provider_catalog::save_named_api_key(
            &resolved.env_file,
            &resolved.api_key_env,
            key.trim(),
        )?;
    } else {
        crate::provider_catalog::save_env_value_to_env_file(
            crate::provider_catalog::OPENAI_COMPAT_LOCAL_ENABLED_ENV,
            &resolved.env_file,
            Some("1"),
        )?;
        if key.trim().is_empty() {
            crate::provider_catalog::save_env_value_to_env_file(
                &resolved.api_key_env,
                &resolved.env_file,
                None,
            )?;
        } else {
            crate::provider_catalog::save_named_api_key(
                &resolved.env_file,
                &resolved.api_key_env,
                key.trim(),
            )?;
        }
    }
    Ok(resolved)
}

fn looks_like_oauth_callback_input(input: &str) -> bool {
    let input = input.trim();
    input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with('?')
        || input.contains("code=")
        || input.contains("state=")
}

fn antigravity_input_requires_state_validation(input: &str, expected_state: Option<&str>) -> bool {
    expected_state.is_some() && looks_like_oauth_callback_input(input)
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

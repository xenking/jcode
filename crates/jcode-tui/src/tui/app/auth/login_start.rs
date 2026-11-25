use super::*;

impl App {
    pub(in super::super) fn start_login_provider(
        &mut self,
        provider: crate::provider_catalog::LoginProviderDescriptor,
    ) {
        crate::telemetry::record_provider_selected(provider.id);
        crate::logging::event_info(
            "login_started",
            vec![
                ("provider_id", provider.id.to_string()),
                ("auth_kind", provider.auth_kind.label().to_string()),
            ],
        );
        match provider.target {
            crate::provider_catalog::LoginProviderTarget::AutoImport => {
                match crate::external_auth::pending_external_auth_review_candidates() {
                    Ok(candidates) if candidates.is_empty() => {
                        self.push_display_message(DisplayMessage::system(
                            "No importable external logins were found.".to_string(),
                        ));
                        self.set_status_notice("Login: no external imports found");
                    }
                    Ok(candidates) => {
                        self.push_display_message(DisplayMessage::system(
                            crate::external_auth::format_external_auth_review_candidates_markdown(
                                &candidates,
                            ),
                        ));
                        self.set_status_notice("Login: choose sources to import");
                        self.pending_login = Some(PendingLogin::AutoImportSelection { candidates });
                    }
                    Err(err) => {
                        self.push_display_message(DisplayMessage::error(format!(
                            "Failed to inspect external login sources: {}",
                            err
                        )));
                        self.set_status_notice("Login: auto import failed");
                    }
                }
            }
            crate::provider_catalog::LoginProviderTarget::Jcode => self.start_jcode_login(),
            crate::provider_catalog::LoginProviderTarget::Claude => self.start_claude_login(),
            crate::provider_catalog::LoginProviderTarget::ClaudeApiKey => {
                self.start_anthropic_api_key_login()
            }
            crate::provider_catalog::LoginProviderTarget::OpenAi => self.start_openai_login(),
            crate::provider_catalog::LoginProviderTarget::OpenAiApiKey => {
                self.start_openai_api_key_login()
            }
            crate::provider_catalog::LoginProviderTarget::OpenRouter => {
                self.start_openrouter_login()
            }
            crate::provider_catalog::LoginProviderTarget::Bedrock => self.start_bedrock_login(),
            crate::provider_catalog::LoginProviderTarget::Azure => self.start_azure_login(),
            crate::provider_catalog::LoginProviderTarget::OpenAiCompatible(profile) => {
                if auth_muse::is_muse_subscription_profile(profile) {
                    self.start_muse_subscription_login();
                } else {
                    self.start_openai_compatible_profile_login(profile)
                }
            }
            crate::provider_catalog::LoginProviderTarget::Cursor => self.start_cursor_login(),
            crate::provider_catalog::LoginProviderTarget::GrokBuild => {
                self.start_grok_build_login()
            }
            crate::provider_catalog::LoginProviderTarget::Copilot => self.start_copilot_login(),
            crate::provider_catalog::LoginProviderTarget::Gemini => self.start_gemini_login(),
            crate::provider_catalog::LoginProviderTarget::Antigravity => {
                self.start_antigravity_login()
            }
            crate::provider_catalog::LoginProviderTarget::Google => {
                crate::telemetry::record_auth_surface_blocked(
                    provider.id,
                    provider.auth_kind.label(),
                );
                self.push_display_message(DisplayMessage::error(
                    "Google/Gmail login is only available from the CLI right now. Run jcode login --provider google."
                        .to_string(),
                ));
            }
        }
    }

    pub(in super::super) fn start_claude_login(&mut self) {
        let label = crate::auth::claude::login_target_label(None)
            .unwrap_or_else(|_| crate::auth::claude::primary_account_label());
        self.start_claude_login_for_account(&label);
    }

    pub(in super::super) fn start_jcode_login(&mut self) {
        self.upsert_login_progress_card(
            auth_progress::JCODE_LOGIN_TITLE,
            "Jcode login: requesting browser approval...".to_string(),
        );
        self.set_status_notice("Jcode account: requesting browser approval");
        let session_id = self.session.id.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.push_display_message(DisplayMessage::error(
                "Jcode account login requires the async runtime.".to_string(),
            ));
            return;
        };
        handle.spawn(async move {
            use crate::subscription_api::{
                ActivationOutcome, PollingBackoff, TokenPollOutcome,
            };
            use std::time::Duration;

            let publish = |message: String, status: &'static str| {
                crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
                    crate::bus::UiActivity::background(
                        Some(session_id.clone()),
                        message,
                        Some(status),
                    ),
                ));
            };
            let client = crate::provider::shared_http_client();
            let api_base = crate::subscription_api::configured_api_base();
            let device = match crate::subscription_api::request_device_authorization(
                &client,
                &api_base,
                None,
            )
            .await
            {
                Ok(device) => device,
                Err(error) => {
                    publish(
                        format!(
                            "Jcode Account Login\n\nCould not start browser approval: {}\n\nRetry /account jcode login. No credential was saved.",
                            error
                        ),
                        "Jcode account login failed",
                    );
                    return;
                }
            };

            let opened = App::open_auth_browser(&device.verification_uri_complete);
            let card = auth_progress::encode_device_login_card(
                "Jcode",
                &device.verification_uri_complete,
                None,
                false,
                opened,
                "Waiting for browser approval",
                "Approve the request in your browser",
                "No email or API key is requested in the terminal.",
            );
            crate::bus::Bus::global().publish(crate::bus::BusEvent::LoginCompleted(
                crate::bus::LoginCompleted {
                    provider: "jcode_code".to_string(),
                    success: true,
                    message: card,
                },
            ));

            let approved = {
                let deadline = tokio::time::Instant::now()
                    + Duration::from_secs(device.expires_in.max(device.interval));
                let mut backoff = PollingBackoff::new(Duration::from_secs(device.interval));
                loop {
                    let delay = backoff.delay();
                    if tokio::time::Instant::now() + delay >= deadline {
                        break Err("Browser approval timed out. No credential was saved.".to_string());
                    }
                    tokio::time::sleep(delay).await;
                    match crate::subscription_api::poll_device_token_once(
                        &client,
                        &api_base,
                        &device.device_code,
                    )
                    .await
                    {
                        Ok(TokenPollOutcome::Pending) => backoff.on_pending(),
                        Ok(TokenPollOutcome::SlowDown { retry_after }) => {
                            backoff.on_slow_down(retry_after)
                        }
                        Ok(TokenPollOutcome::Approved(key)) => break Ok(key),
                        Ok(TokenPollOutcome::Expired) => break Err(
                            "The browser approval expired or was already exchanged. Start a new login."
                                .to_string(),
                        ),
                        Ok(TokenPollOutcome::Denied) => break Err(
                            "Jcode account login was canceled or denied in the browser."
                                .to_string(),
                        ),
                        Err(error) if error.is_temporary() => backoff.on_offline_error(),
                        Err(error) => break Err(error.to_string()),
                    }
                }
            };
            let approved = match approved {
                Ok(approved) => approved,
                Err(error) => {
                    publish(
                        format!("Jcode Account Login\n\n{error}\n\nRetry /account jcode login."),
                        "Jcode account login stopped",
                    );
                    return;
                }
            };

            if let Err(error) = crate::subscription_catalog::persist_account_credentials(
                &approved.api_key,
                Some(&approved.account_id),
                Some(&approved.email),
                Some(&approved.tier),
            ) {
                publish(
                    format!("Jcode Account Login\n\nBrowser approval succeeded, but secure credential persistence failed: {error}"),
                    "Jcode account credential save failed",
                );
                return;
            }
            crate::auth::AuthStatus::invalidate_cache();
            publish(
                format!(
                    "Jcode Account Approved\n\nSigned in as {}. The API key is stored with owner-only permissions. Finish setting your monthly spending limit in the browser; Jcode is checking /v1/me...",
                    approved.email
                ),
                "Jcode account: waiting for spending limit",
            );

            match crate::subscription_api::poll_for_paid_activation(
                &client,
                &api_base,
                &approved.api_key,
                crate::subscription_api::ACTIVATION_TIMEOUT,
                Duration::from_secs(device.interval.max(2)),
            )
            .await
            {
                ActivationOutcome::Active(me) => {
                    let message = format!(
                        "Jcode Account Ready\n\nHosted models are enabled for {} with a ${:.2} monthly spending limit. Models are being refreshed automatically.\n\nUsage: /usage\nManage limit: /account jcode manage\nLogout: /account jcode logout",
                        me.email,
                        me.usage.budget_usd
                    );
                    publish(message.clone(), "Jcode hosted models ready");

                    // The device flow used to stop after saving the credential and
                    // publishing a status message. Unlike every other login flow it
                    // never told the App that authentication had completed, so the
                    // running provider retained its pre-login routes until the user
                    // manually ran /refresh-model-list. Route activation also powers
                    // model-switch availability checks, which made every newly shown
                    // hosted model appear unavailable in that stale runtime.
                    crate::bus::Bus::global().publish(
                        crate::bus::BusEvent::LoginCompleted(crate::bus::LoginCompleted {
                            provider: "jcode".to_string(),
                            success: true,
                            message,
                        }),
                    );
                }
                ActivationOutcome::Canceled(_) => publish(
                    "Jcode Account Login\n\nBilling setup was canceled. The valid account key remains saved, but hosted usage is not enabled.\n\nStatus: /usage\nManage limit: /account jcode manage\nLogout: /account jcode logout".to_string(),
                    "Jcode hosted billing not active",
                ),
                ActivationOutcome::TimedOut { last_error_was_offline } => publish(
                    format!(
                        "Jcode Account Login\n\nA spending limit was not confirmed before timeout{}. The valid account key remains saved.\n\nStatus: /usage\nManage limit: /account jcode manage\nLogout: /account jcode logout",
                        if last_error_was_offline { " because the API remained unreachable" } else { "" }
                    ),
                    "Jcode hosted billing setup pending",
                ),
                ActivationOutcome::Revoked | ActivationOutcome::Denied => {
                    let _ = crate::subscription_catalog::clear_account_credentials();
                    publish(
                        "Jcode Account Login\n\nThe issued key was revoked or denied during activation checks. Local credentials were cleared. Retry /account jcode login.".to_string(),
                        "Jcode account key rejected",
                    );
                }
            }
        });
    }

    pub(in super::super) fn open_jcode_account_management(&mut self) {
        let url = crate::subscription_catalog::JCODE_ACCOUNT_URL;
        let opened = Self::open_auth_browser(url);
        self.push_display_message(DisplayMessage::system(format!(
            "Jcode Account Management\n\n{}{}",
            url,
            if opened {
                "\n\nOpened in your browser."
            } else {
                "\n\nThe browser could not be opened automatically. Open the public URL above manually."
            }
        )));
        self.set_status_notice("Jcode account management");
    }

    pub(in super::super) fn start_jcode_account_logout(&mut self) {
        self.set_status_notice("Jcode account: logging out");
        let session_id = self.session.id.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            let result = crate::subscription_catalog::clear_account_credentials();
            match result {
                Ok(()) => self.push_display_message(DisplayMessage::system(
                    "Jcode account credentials and cache were cleared locally. Remote revocation could not be attempted without the async runtime."
                        .to_string(),
                )),
                Err(error) => self.push_display_message(DisplayMessage::error(format!(
                    "Failed to clear local Jcode account credentials: {error}"
                ))),
            }
            return;
        };
        handle.spawn(async move {
            let api_key = crate::subscription_catalog::configured_api_key();
            let remote = if let Some(api_key) = api_key.as_deref() {
                crate::subscription_api::revoke_current_key(
                    &crate::provider::shared_http_client(),
                    &crate::subscription_api::configured_api_base(),
                    api_key,
                )
                .await
            } else {
                Ok(())
            };
            let local = crate::subscription_catalog::clear_account_credentials();
            crate::auth::AuthStatus::invalidate_cache();
            let message = match local {
                Err(error) => format!(
                    "Jcode Account Logout\n\nFailed to securely clear local credentials: {error}"
                ),
                Ok(()) => match (api_key.is_some(), remote) {
                    (false, _) => "Jcode Account Logout\n\nNo local credential was present. Local account cache is clear.".to_string(),
                    (true, Ok(())) => "Jcode Account Logout\n\nThe current key was revoked. Local credentials and account cache were securely cleared.".to_string(),
                    (true, Err(crate::subscription_api::AccountApiError::Unauthorized)) => "Jcode Account Logout\n\nThe key was already revoked. Local credentials and account cache were securely cleared.".to_string(),
                    (true, Err(crate::subscription_api::AccountApiError::Offline(_))) => "Jcode Account Logout\n\nLocal credentials and account cache were securely cleared. The API was offline, so remote revocation could not be confirmed.".to_string(),
                    (true, Err(error)) => format!("Jcode Account Logout\n\nLocal credentials and account cache were securely cleared. Remote revocation could not be confirmed: {error}"),
                },
            };
            crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
                crate::bus::UiActivity::background(
                    Some(session_id),
                    message,
                    Some("Jcode account logout complete"),
                ),
            ));
        });
    }

    pub(in super::super) fn start_claude_login_for_account(&mut self, label: &str) {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};

        let verifier: String = {
            use rand::Rng;
            const CHARSET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let mut rng = rand::rng();
            (0..64)
                .map(|_| {
                    let idx = rng.random_range(0..CHARSET.len());
                    CHARSET[idx] as char
                })
                .collect()
        };

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(hash);

        // Try a loopback callback first so the user never has to copy/paste the
        // authorization code (mirrors the OpenAI/Gemini flows). Claude uses the
        // PKCE verifier as the OAuth `state`, so we wait for that on the
        // listener. If binding fails we fall back to manual paste with the
        // hosted redirect page.
        let callback_listener = crate::auth::oauth::bind_callback_listener(0).ok();
        let callback_port = callback_listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|addr| addr.port());
        let callback_available = callback_listener.is_some() && callback_port.is_some();

        let (auth_url, redirect_uri) = match callback_port {
            Some(port) if callback_available => {
                let redirect_uri = format!("http://localhost:{}/callback", port);
                let auth_url =
                    crate::auth::oauth::claude_auth_url(&redirect_uri, &challenge, &verifier);
                (auth_url, redirect_uri)
            }
            _ => {
                let redirect_uri = crate::auth::oauth::claude::REDIRECT_URI.to_string();
                let auth_url =
                    crate::auth::oauth::claude_auth_url(&redirect_uri, &challenge, &verifier);
                (auth_url, redirect_uri)
            }
        };
        let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
            &auth_url,
            "Scan this on another device if this machine has no browser:",
        )
        .map(|section| format!("\n\n{section}"))
        .unwrap_or_default();

        let browser_opened = Self::open_auth_browser(&auth_url);
        let preflight = Self::record_oauth_preflight(
            "claude",
            browser_opened,
            callback_port.map(|p| format!("localhost:{}", p)).as_deref(),
            Some(callback_available),
        );

        // Spawn the loopback waiter. On success it publishes LoginCompleted just
        // like the manual paste path, so onboarding and account UI react
        // identically.
        if let (Some(listener), true) = (callback_listener, callback_available) {
            let verifier_clone = verifier.clone();
            let label_clone = label.to_string();
            let redirect_clone = redirect_uri.clone();
            tokio::spawn(async move {
                match Self::claude_login_callback(
                    verifier_clone,
                    label_clone,
                    redirect_clone,
                    listener,
                )
                .await
                {
                    Ok(msg) => {
                        crate::logging::info(&format!("Claude login: {}", msg));
                        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                            provider: "claude".to_string(),
                            success: true,
                            message: msg,
                        }));
                    }
                    Err(e) => {
                        crate::logging::info(&format!(
                            "Claude automatic callback did not complete: {}",
                            e
                        ));
                    }
                }
            });
        }

        let callback_line = if callback_available {
            "Waiting for the browser callback... (this completes automatically)\n".to_string()
        } else {
            "After logging in, copy the callback URL or authorization code and paste it here.\n"
                .to_string()
        };
        let details = format!(
            "{}{}{}{}",
            if preflight.is_empty() {
                String::new()
            } else {
                format!("{preflight}\n")
            },
            callback_line,
            if preflight.is_empty() {
                String::new()
            } else {
                "Manual-safe fallback is already active here.\n".to_string()
            },
            qr_section.trim_start_matches('\n')
        );

        self.show_oauth_login_progress(
            auth_progress::CLAUDE_LOGIN_TITLE,
            "Claude",
            Some(label),
            &auth_url,
            browser_opened,
            callback_available,
            "Paste callback URL or code here if needed",
            &details,
        );
        if callback_available {
            self.set_status_notice(format!("Login [{}]: waiting...", label));
        } else {
            self.set_status_notice(format!("Login [{}]: paste code...", label));
        }
        self.begin_pending_login(PendingLogin::ClaudeAccount {
            verifier,
            label: label.to_string(),
            redirect_uri: if callback_available {
                Some(redirect_uri)
            } else {
                None
            },
        });
    }

    pub(super) async fn claude_login_callback(
        verifier: String,
        label: String,
        redirect_uri: String,
        listener: tokio::net::TcpListener,
    ) -> Result<String, String> {
        // Claude uses the PKCE verifier as the OAuth `state` value.
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            crate::auth::oauth::wait_for_callback_async_on_listener(listener, &verifier),
        )
        .await
        .map_err(|_| "Login timed out after 5 minutes. Please try again.".to_string())?
        .map_err(|e| format!("Callback failed: {}", e))?;

        Self::claude_token_exchange(verifier, code, &label, Some(redirect_uri)).await
    }

    pub(in super::super) fn switch_account(&mut self, label: &str) {
        self.switch_saved_oauth_account("claude", label);
    }

    pub(super) fn switch_saved_oauth_account(&mut self, provider_id: &str, label: &str) {
        let selection = match saved_account_route_selection(
            provider_id,
            label,
            self.provider.name(),
            &self.provider.model(),
            crate::config::config().provider.default_model.as_deref(),
        ) {
            Ok(selection) => selection,
            Err(error) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to select saved account {label}: {error}"
                )));
                return;
            }
        };
        let model_request = selection.routed_model_spec();
        if let Err(error) = self.provider.set_route_selection(&selection) {
            self.push_display_message(DisplayMessage::error(format!(
                "Failed to switch {} account: {}",
                selection.provider_label, error
            )));
            return;
        }
        // Reset upstream conversation state and persist the qualified account
        // route, not just the process-wide saved-login preference.
        self.finalize_model_switch(&model_request);
        let saved = if provider_id == "claude" {
            crate::auth::claude::set_active_account(label)
        } else {
            crate::auth::codex::set_active_account(label)
        };
        if let Err(error) = saved {
            self.push_display_message(DisplayMessage::error(format!(
                "Selected {} account {} for this session, but could not save the active login: {}",
                selection.provider_label, label, error
            )));
        } else {
            self.push_display_message(DisplayMessage::system(format!(
                "Switched to {} account {}.",
                selection.provider_label, label
            )));
        }
        crate::auth::AuthStatus::invalidate_cache();
        let provider = self.provider.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                provider.invalidate_credentials().await;
            });
        }
        self.set_context_limit_and_sync_budget(self.provider.context_window());
    }

    pub(in super::super) fn switch_account_by_label(&mut self, label: &str) {
        let has_anthropic = crate::auth::claude::list_accounts()
            .unwrap_or_default()
            .iter()
            .any(|account| account.label == label);
        let has_openai = crate::auth::codex::list_accounts()
            .unwrap_or_default()
            .iter()
            .any(|account| account.label == label);

        match (has_anthropic, has_openai) {
            (true, false) => self.switch_account(label),
            (false, true) => self.switch_openai_account(label),
            (true, true) => self.push_display_message(DisplayMessage::error(format!(
                "Account label {} exists for both Anthropic and OpenAI. Use /account switch {} or /account openai switch {} explicitly.",
                label, label, label
            ))),
            (false, false) => self.push_display_message(DisplayMessage::error(format!(
                "No Anthropic or OpenAI account with label {} found.",
                label
            ))),
        }
    }

    pub(in super::super) fn remove_account(&mut self, label: &str) {
        match crate::auth::claude::remove_account(label) {
            Ok(()) => {
                self.push_display_message(DisplayMessage::system(format!(
                    "Removed Anthropic account {}.",
                    label
                )));
            }
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to remove account: {}",
                    e
                )));
            }
        }
    }

    pub(in super::super) fn switch_openai_account(&mut self, label: &str) {
        self.switch_saved_oauth_account("openai", label);
    }

    pub(in super::super) fn remove_openai_account(&mut self, label: &str) {
        match crate::auth::codex::remove_account(label) {
            Ok(()) => {
                self.push_display_message(DisplayMessage::system(format!(
                    "Removed OpenAI account {}.",
                    label
                )));
            }
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to remove OpenAI account: {}",
                    e
                )));
            }
        }
    }

    pub(super) fn start_openai_login(&mut self) {
        let label = crate::auth::codex::login_target_label(None)
            .unwrap_or_else(|_| crate::auth::codex::primary_account_label());
        self.start_openai_login_for_account(&label);
    }

    pub(in super::super) fn start_openai_login_for_account(&mut self, label: &str) {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};

        let verifier: String = {
            use rand::Rng;
            const CHARSET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let mut rng = rand::rng();
            (0..64)
                .map(|_| {
                    let idx = rng.random_range(0..CHARSET.len());
                    CHARSET[idx] as char
                })
                .collect()
        };

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(hash);

        let state: String = {
            let bytes: [u8; 16] = rand::random();
            hex::encode(bytes)
        };

        let port = crate::auth::oauth::openai::DEFAULT_PORT;
        let redirect_uri = crate::auth::oauth::openai::redirect_uri(port);
        let auth_url = crate::auth::oauth::openai_auth_url_with_prompt(
            &redirect_uri,
            &challenge,
            &state,
            Some("login"),
        );
        let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
            &auth_url,
            "Scan this on another device if this machine has no browser, then paste the full callback URL here:",
        )
        .map(|section| format!("\n\n{section}"))
        .unwrap_or_default();

        let callback_listener = crate::auth::oauth::bind_callback_listener(port).ok();
        let callback_available = callback_listener.is_some();
        let browser_opened = Self::open_auth_browser(&auth_url);
        let label_owned = label.to_string();

        if let Some(listener) = callback_listener {
            let verifier_clone = verifier.clone();
            let state_clone = state.clone();
            let label_clone = label_owned.clone();
            tokio::spawn(async move {
                match Self::openai_login_callback(
                    verifier_clone,
                    state_clone,
                    Some(label_clone),
                    listener,
                )
                .await
                {
                    Ok(msg) => {
                        crate::logging::info(&format!("OpenAI login: {}", msg));
                        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                            provider: "openai".to_string(),
                            success: true,
                            message: msg,
                        }));
                    }
                    Err(e) => {
                        crate::logging::info(&format!(
                            "OpenAI automatic callback did not complete: {}",
                            e
                        ));
                    }
                }
            });
        }

        let callback_line = if callback_available {
            format!(
                "Waiting for callback on localhost:{}... (this will complete automatically)\n",
                port
            )
        } else {
            format!(
                "Local callback port localhost:{} is unavailable, so finish in any browser and paste the full callback URL here.\n",
                port
            )
        };
        let preflight = Self::record_oauth_preflight(
            "openai",
            browser_opened,
            Some(&format!("localhost:{}", port)),
            Some(callback_available),
        );

        let device_fallback_cancelled = if !browser_opened || !callback_available {
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            Self::spawn_openai_device_fallback(
                label_owned.clone(),
                std::sync::Arc::clone(&cancelled),
            );
            Some(cancelled)
        } else {
            None
        };

        self.show_openai_login_progress(
            label,
            &auth_url,
            browser_opened,
            callback_available,
            &format!("{preflight}\n{callback_line}\nIf Continue is disabled, wait for the page to finish loading.\nPaste the full callback URL or query string to finish from another device.{qr_section}"),
        );
        self.set_status_notice(format!("Login [{}]: waiting...", label));
        self.begin_pending_login(PendingLogin::OpenAiAccount {
            verifier,
            label: label.to_string(),
            expected_state: state,
            redirect_uri,
            device_fallback_cancelled,
        });
    }

    /// Automatically try OpenAI's device-code flow when browser launch or the
    /// localhost:1455 callback listener is unavailable (e.g. headless/SSH/busy port).
    /// If device-code auth is unavailable or disabled on the account, stays silent
    /// so the already-displayed manual callback URL card remains active.
    pub(super) fn spawn_openai_device_fallback(
        label: String,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            use std::sync::atomic::Ordering;

            let device = match crate::auth::oauth::start_openai_device_login().await {
                Ok(device) => device,
                Err(err) => {
                    crate::logging::info(&format!(
                        "OpenAI device-code fallback unavailable (keeping manual callback): {}",
                        err
                    ));
                    return;
                }
            };
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let clipboard_ok = super::helpers::copy_to_clipboard(&device.user_code);
            let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
                &device.verification_url,
                "Scan this on a phone or another device to enter the code:",
            )
            .map(|section| format!("\n\n{section}"))
            .unwrap_or_default();
            let details = format!(
                "Account: {label}\n\nOpen {} on any browser or phone and enter code `{}`.\nYou can still paste a manual OAuth callback URL here if preferred.{qr_section}",
                device.verification_url, device.user_code
            );
            let card = auth_progress::encode_device_login_card(
                "OpenAI",
                &device.verification_url,
                Some(&device.user_code),
                clipboard_ok,
                false,
                "Waiting for OpenAI approval (or paste callback URL)",
                "Paste callback URL here if needed · /cancel",
                &details,
            );
            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                provider: "openai_device_code".to_string(),
                success: true,
                message: card,
            }));

            let interval = std::time::Duration::from_secs(device.interval.max(1));
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                match crate::auth::oauth::poll_openai_device_login_at(
                    crate::auth::oauth::openai::AUTH_BASE,
                    &device.device_auth_id,
                    &device.user_code,
                )
                .await
                {
                    Ok(Some((code, verifier))) => {
                        if cancelled.load(Ordering::Acquire) {
                            return;
                        }
                        match crate::auth::oauth::exchange_openai_code(
                            &code,
                            &verifier,
                            crate::auth::oauth::openai::DEVICE_REDIRECT_URI,
                        )
                        .await
                        {
                            Ok(tokens) => {
                                if cancelled.load(Ordering::Acquire) {
                                    return;
                                }
                                match crate::auth::oauth::save_openai_tokens_for_account(
                                    &tokens, &label,
                                ) {
                                    Ok(()) => {
                                        Bus::global().publish(BusEvent::LoginCompleted(
                                            LoginCompleted {
                                                provider: "openai".to_string(),
                                                success: true,
                                                message: format!(
                                                    "Successfully logged in to OpenAI! (account: {})",
                                                    label
                                                ),
                                            },
                                        ));
                                    }
                                    Err(err) => {
                                        Bus::global().publish(BusEvent::LoginCompleted(
                                            LoginCompleted {
                                                provider: "openai".to_string(),
                                                success: false,
                                                message: format!("Failed to save tokens: {}", err),
                                            },
                                        ));
                                    }
                                }
                            }
                            Err(err) => {
                                crate::logging::info(&format!(
                                    "OpenAI device-code token exchange failed: {}",
                                    err
                                ));
                                Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                    provider: "openai".to_string(),
                                    success: false,
                                    message: format!("OpenAI device login failed: {}", err),
                                }));
                            }
                        }
                        return;
                    }
                    Ok(None) => {
                        if chrono::Utc::now().timestamp_millis() >= device.expires_at_ms {
                            crate::logging::info("OpenAI device-code fallback expired.");
                            return;
                        }
                        tokio::time::sleep(interval).await;
                    }
                    Err(err) => {
                        crate::logging::info(&format!(
                            "OpenAI device-code polling stopped: {}",
                            err
                        ));
                        return;
                    }
                }
            }
        });
    }

    pub(super) async fn openai_login_callback(
        verifier: String,
        expected_state: String,
        label: Option<String>,
        listener: tokio::net::TcpListener,
    ) -> Result<String, String> {
        let port = crate::auth::oauth::openai::DEFAULT_PORT;
        let redirect_uri = crate::auth::oauth::openai::redirect_uri(port);
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            crate::auth::oauth::wait_for_callback_async_on_listener(listener, &expected_state),
        )
        .await
        .map_err(|_| "Login timed out after 5 minutes. Please try again.".to_string())?
        .map_err(|e| format!("Callback failed: {}", e))?;

        Self::openai_token_exchange(verifier, code, label, None, &redirect_uri).await
    }

    pub(super) async fn openai_token_exchange(
        verifier: String,
        input: String,
        label: Option<String>,
        expected_state: Option<String>,
        redirect_uri: &str,
    ) -> Result<String, String> {
        let oauth_tokens = if let Some(expected_state) = expected_state {
            crate::auth::oauth::exchange_openai_callback_input(
                &verifier,
                input.trim(),
                &expected_state,
                redirect_uri,
            )
            .await
            .map_err(|e| e.to_string())?
        } else {
            crate::auth::oauth::exchange_openai_code(&input, &verifier, redirect_uri)
                .await
                .map_err(|e| e.to_string())?
        };

        let label = label.unwrap_or_else(crate::auth::codex::primary_account_label);
        crate::auth::oauth::save_openai_tokens_for_account(&oauth_tokens, &label)
            .map_err(|e| format!("Failed to save tokens: {}", e))?;

        Ok(format!(
            "Successfully logged in to OpenAI! (account: {})",
            label
        ))
    }

    pub(super) fn start_gemini_login(&mut self) {
        let (verifier, challenge) = crate::auth::oauth::generate_pkce_public();
        let state = crate::auth::oauth::generate_state_public();

        let callback_listener = crate::auth::oauth::bind_callback_listener(0).ok();
        let maybe_redirect_uri = callback_listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| format!("http://127.0.0.1:{}/oauth2callback", addr.port()));

        let auth_setup: anyhow::Result<(String, Option<String>, String)> =
            if let Some(redirect_uri) = maybe_redirect_uri {
                crate::auth::gemini::build_web_auth_url(&redirect_uri, &challenge, &state)
                    .map(|auth_url| (auth_url, Some(state.clone()), redirect_uri))
            } else {
                crate::auth::gemini::build_manual_auth_url(
                    "https://codeassist.google.com/authcode",
                    &challenge,
                    &state,
                )
                .map(|auth_url| {
                    (
                        auth_url,
                        None,
                        "https://codeassist.google.com/authcode".to_string(),
                    )
                })
            };

        let (auth_url, pending_state, redirect_uri) = match auth_setup {
            Ok(values) => values,
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Gemini login is unavailable: {}",
                    e
                )));
                self.set_status_notice("Login: failed");
                return;
            }
        };

        let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
            &auth_url,
            "Scan this on another device if this machine has no browser, then paste the callback URL or authorization code here:",
        )
        .map(|section| format!("\n\n{section}"))
        .unwrap_or_default();

        let browser_opened = Self::open_auth_browser(&auth_url);
        let callback_available = callback_listener.is_some() && pending_state.is_some();

        if let (Some(listener), Some(expected_state)) = (callback_listener, pending_state.clone()) {
            let redirect_clone = redirect_uri.clone();
            let verifier_clone = verifier.clone();
            tokio::spawn(async move {
                let code = tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    crate::auth::oauth::wait_for_callback_async_on_listener(
                        listener,
                        &expected_state,
                    ),
                )
                .await
                .map_err(|_| "Login timed out after 5 minutes. Please try again.".to_string())
                .and_then(|result| result.map_err(|e| format!("Callback failed: {}", e)));

                match code {
                    Ok(code) => {
                        match crate::auth::gemini::exchange_callback_code(
                            &code,
                            &verifier_clone,
                            &redirect_clone,
                        )
                        .await
                        {
                            Ok(tokens) => {
                                let msg = if let Some(email) = tokens.email {
                                    format!(
                                        "Successfully logged in to Gemini! (account: {})",
                                        email
                                    )
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
                                let message = format!("Gemini login failed: {}", e);
                                crate::logging::info(&format!(
                                    "Gemini automatic callback did not complete: {}",
                                    e
                                ));
                                Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                    provider: "gemini".to_string(),
                                    success: false,
                                    message,
                                }));
                            }
                        }
                    }
                    Err(e) => {
                        crate::logging::info(&format!(
                            "Gemini automatic callback did not complete: {}",
                            e
                        ));
                        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                            provider: "gemini".to_string(),
                            success: false,
                            message: format!("Gemini login failed: {}", e),
                        }));
                    }
                }
            });
        }

        let callback_line = if callback_available {
            format!(
                "Waiting for callback on {}... (this will complete automatically)\n",
                redirect_uri
            )
        } else {
            "Finish login in any browser, then paste the callback URL or authorization code here.\n"
                .to_string()
        };
        let preflight = Self::record_oauth_preflight(
            "gemini",
            browser_opened,
            Some(&redirect_uri),
            Some(callback_available),
        );

        let details = format!(
            "{}{}{}{}",
            if preflight.is_empty() {
                String::new()
            } else {
                format!("{preflight}\n")
            },
            callback_line,
            if preflight.is_empty() {
                String::new()
            } else {
                "Manual-safe fallback is already active here.\n".to_string()
            },
            qr_section.trim_start_matches('\n')
        );

        self.show_oauth_login_progress(
            auth_progress::GEMINI_LOGIN_TITLE,
            "Gemini",
            None,
            &auth_url,
            browser_opened,
            callback_available,
            "Paste callback URL or code here if needed",
            &details,
        );
        self.set_status_notice("Login: waiting...");
        self.begin_pending_login(PendingLogin::Gemini {
            verifier,
            expected_state: pending_state,
            redirect_uri,
        });
    }

    pub(super) fn start_openrouter_login(&mut self) {
        self.start_api_key_login(
            "OpenRouter",
            "https://openrouter.ai/keys",
            "openrouter.env",
            "OPENROUTER_API_KEY",
            None,
            None,
            false,
            None,
        );
    }

    pub(super) fn start_bedrock_login(&mut self) {
        self.start_api_key_login(
            "AWS Bedrock",
            "https://console.aws.amazon.com/bedrock/home#/api-keys",
            crate::provider::bedrock::ENV_FILE,
            crate::provider::bedrock::API_KEY_ENV,
            Some("us.amazon.nova-micro-v1:0"),
            Some(
                "Region: us-east-2 (default for TUI onboarding; use CLI login for another region)",
            ),
            false,
            None,
        );
    }

    pub(super) fn start_openai_api_key_login(&mut self) {
        self.start_api_key_login(
            "OpenAI API",
            "https://platform.openai.com/api-keys",
            "openai.env",
            "OPENAI_API_KEY",
            None,
            Some("https://api.openai.com/v1"),
            false,
            None,
        );
    }

    pub(super) fn start_anthropic_api_key_login(&mut self) {
        self.start_api_key_login(
            "Anthropic API",
            "https://console.anthropic.com/settings/keys",
            "anthropic.env",
            "ANTHROPIC_API_KEY",
            None,
            Some("https://api.anthropic.com"),
            false,
            None,
        );
    }

    pub(super) fn start_openai_compatible_profile_login(
        &mut self,
        profile: crate::provider_catalog::OpenAiCompatibleProfile,
    ) {
        if profile.id == crate::provider_catalog::OPENAI_COMPAT_PROFILE.id {
            let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
            self.push_display_message(DisplayMessage::system(format!(
                "{} Endpoint\n\n\
                 Setup docs: {}\n\
                 Current API base: {}\n\n\
                 Paste the API base below. Press Enter to keep the current value, or type /cancel to abort.",
                resolved.display_name, resolved.setup_url, resolved.api_base
            )));
            self.set_status_notice("Login: API base...");
            self.pending_login = Some(PendingLogin::OpenAiCompatibleApiBase { profile });
            return;
        }

        self.start_openai_compatible_key_login(profile);
    }

    pub(super) fn start_openai_compatible_key_login(
        &mut self,
        profile: crate::provider_catalog::OpenAiCompatibleProfile,
    ) {
        let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
        self.start_api_key_login(
            &resolved.display_name,
            &resolved.setup_url,
            &resolved.env_file,
            &resolved.api_key_env,
            resolved.default_model.as_deref(),
            Some(&resolved.api_base),
            !resolved.requires_api_key,
            Some(profile),
        );
    }

    pub(in super::super) fn start_auto_api_key_login(&mut self) {
        self.push_display_message(DisplayMessage::system(
            "Paste an API key\n\n\
             Provider is detected automatically for Anthropic, OpenAI, OpenRouter, Gemini, xAI, Groq, Hugging Face, Cerebras, Fireworks, Perplexity, Together, NVIDIA NIM, Bedrock, and more.\n\n\
             Paste your key below, or /cancel."
                .to_string(),
        ));
        self.set_status_notice("Login: paste any API key...");
        self.begin_pending_login(PendingLogin::AutoApiKey);
    }

    pub(super) fn validate_and_save_detected_api_key(
        &mut self,
        provider: crate::provider_catalog::LoginProviderDescriptor,
        key: String,
        auto_detected: bool,
    ) {
        let provider_id = provider.id.to_string();
        let display_name = provider.display_name.to_string();
        crate::logging::event_info(
            "login_api_key_save_attempt",
            vec![
                ("provider_id", provider_id.clone()),
                ("provider", display_name.clone()),
                ("auth_method", "api_key".to_string()),
                ("auto_detected", auto_detected.to_string()),
                ("input_len", key.len().to_string()),
            ],
        );

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            match crate::auth::api_key_login::save_api_key_for_provider(provider, &key) {
                Ok(saved) => {
                    if let Some(profile) = saved.openai_compatible_profile {
                        crate::provider_catalog::apply_openai_compatible_profile_env(Some(profile));
                    }
                    let verb = if auto_detected {
                        "Detected and saved"
                    } else {
                        "Saved"
                    };
                    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                        provider: provider_id,
                        success: true,
                        message: format!(
                            "✓ {verb} {display_name} API key.\nUse /model to switch models."
                        ),
                    }));
                }
                Err(err) => {
                    self.push_display_message(DisplayMessage::error(format!(
                        "Failed to save {display_name} API key: {err}"
                    )));
                    self.pending_login = Some(PendingLogin::AutoApiKey);
                }
            }
            return;
        };

        self.pending_login = Some(PendingLogin::Exchanging {
            provider: provider_id.clone(),
        });
        let action_label = if auto_detected {
            format!("Detected {display_name} · verifying key...")
        } else {
            format!("Verifying {display_name} key...")
        };
        self.set_status_notice(action_label);

        handle.spawn(async move {
            let validation =
                crate::auth::api_key_login::validate_api_key_for_provider(provider, &key).await;
            match validation {
                crate::auth::api_key_login::ApiKeyValidationOutcome::Rejected { status, detail } => {
                    let detail_suffix = detail
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                        provider: provider_id,
                        success: false,
                        message: format!(
                            "{display_name} rejected that API key (HTTP {status}{detail_suffix}). Check the key and run /login key to try again."
                        ),
                    }));
                }
                crate::auth::api_key_login::ApiKeyValidationOutcome::Verified
                | crate::auth::api_key_login::ApiKeyValidationOutcome::Unverified { .. } => {
                    let verified = matches!(
                        validation,
                        crate::auth::api_key_login::ApiKeyValidationOutcome::Verified
                    );
                    match crate::auth::api_key_login::save_api_key_for_provider(provider, &key) {
                        Ok(saved) => {
                            if saved.provider_id == "bedrock" {
                                crate::provider::activation::select_initial_runtime_provider_key(
                                    "bedrock",
                                );
                            }
                            if let Some(profile) = saved.openai_compatible_profile {
                                crate::provider_catalog::apply_openai_compatible_profile_env(
                                    Some(profile),
                                );
                            }
                            let headline = match (auto_detected, verified) {
                                (true, true) => {
                                    format!("✓ Detected and verified {display_name} API key.")
                                }
                                (false, true) => {
                                    format!("✓ Verified and saved {display_name} API key.")
                                }
                                (true, false) => format!(
                                    "✓ Detected and saved {display_name} API key (could not verify online)."
                                ),
                                (false, false) => format!(
                                    "✓ Saved {display_name} API key (could not verify online)."
                                ),
                            };
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: provider_id,
                                success: true,
                                message: format!(
                                    "{headline}\nRefreshing models automatically. Use /model to switch."
                                ),
                            }));
                        }
                        Err(err) => {
                            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                                provider: provider_id,
                                success: false,
                                message: format!("Failed to save {display_name} API key: {err}"),
                            }));
                        }
                    }
                }
            }
        });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "API-key login setup passes provider-specific metadata assembled at call sites"
    )]
    pub(super) fn start_api_key_login(
        &mut self,
        provider: &str,
        docs_url: &str,
        env_file: &str,
        key_name: &str,
        default_model: Option<&str>,
        endpoint: Option<&str>,
        api_key_optional: bool,
        openai_compatible_profile: Option<crate::provider_catalog::OpenAiCompatibleProfile>,
    ) {
        let heading = if api_key_optional {
            format!("{provider} (Local Endpoint)")
        } else if provider.ends_with("API Key") {
            provider.to_string()
        } else if provider.ends_with("API") {
            format!("{provider} Key")
        } else {
            format!("{provider} API Key")
        };
        let mut lines = vec![heading, String::new()];
        if api_key_optional && let Some(endpoint) = endpoint {
            lines.push(format!("Endpoint: {endpoint}"));
        }
        if !docs_url.is_empty() && !docs_url.contains("#openai-compatible-providers") {
            lines.push(format!("Get a key: {docs_url}"));
        }
        if let Some(model) = default_model {
            lines.push(format!("Default model: {model}"));
        }
        if lines.len() > 2 {
            lines.push(String::new());
        }
        lines.push(if api_key_optional {
            "Paste a key if your endpoint requires one, press Enter to skip, or /cancel."
                .to_string()
        } else {
            "Paste your API key below, or /cancel.".to_string()
        });
        self.push_display_message(DisplayMessage::system(lines.join("\n")));
        self.set_status_notice(if api_key_optional {
            "Login: optional key..."
        } else {
            "Login: paste key..."
        });
        let provider_id = openai_compatible_profile
            .map(|profile| profile.id.to_string())
            .unwrap_or_else(|| match key_name {
                crate::subscription_catalog::JCODE_API_KEY_ENV => "jcode".to_string(),
                "OPENROUTER_API_KEY" => "openrouter".to_string(),
                _ => provider.to_ascii_lowercase().replace(' ', "-"),
            });
        let auth_method = if api_key_optional {
            "local_endpoint"
        } else {
            "api_key"
        };
        self.begin_pending_login(PendingLogin::ApiKeyProfile {
            provider_id,
            provider: provider.to_string(),
            auth_method: auth_method.to_string(),
            docs_url: docs_url.to_string(),
            env_file: env_file.to_string(),
            key_name: key_name.to_string(),
            default_model: default_model.map(|m| m.to_string()),
            endpoint: endpoint.map(|value| value.to_string()),
            api_key_optional,
            openai_compatible_profile,
        });
    }

    pub(super) fn start_azure_login(&mut self) {
        self.push_display_message(DisplayMessage::system(
            "Azure OpenAI Login\n\n\
             jcode uses Azure OpenAI's /openai/v1 API with either Microsoft Entra ID or an API key.\n\n\
             Enter your Azure OpenAI endpoint, for example https://your-resource.openai.azure.com, or type /cancel to abort."
                .to_string(),
        ));
        self.set_status_notice("Login: Azure endpoint...");
        self.begin_pending_login(PendingLogin::AzureEndpoint);
    }

    pub(super) fn start_cursor_login(&mut self) {
        crate::telemetry::record_auth_started("cursor", "api_key");

        self.push_display_message(DisplayMessage::system(
            "Cursor API Key\n\n\
             Get your API key from: https://cursor.com/settings\n\
             (Dashboard > Integrations > User API Keys)\n\n\
             jcode will save it securely and use the native Cursor HTTPS transport.\n\n\
             Paste your API key below, or type /cancel to abort."
                .to_string(),
        ));
        self.set_status_notice("Login: paste cursor key...");
        self.begin_pending_login(PendingLogin::CursorApiKey);
    }

    pub(super) fn start_grok_build_login(&mut self) {
        self.set_status_notice("Grok Build: preparing sign-in...");
        self.begin_pending_login(PendingLogin::GrokBuild);
        self.upsert_login_progress_card(
            auth_progress::GROK_BUILD_LOGIN_TITLE,
            "Grok Build login: requesting xAI device code...\nType /cancel to abort.".to_string(),
        );

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                provider: "grok-build".to_string(),
                success: false,
                message: "Grok Build login requires the async runtime.".to_string(),
            }));
            return;
        };
        handle.spawn(async move {
            let client = crate::provider::shared_http_client();
            let authorization = match crate::auth::grok_build::initiate_device_login(&client).await {
                Ok(authorization) => authorization,
                Err(error) => {
                    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                        provider: "grok-build".to_string(),
                        success: false,
                        message: format!("Failed to start Grok Build login: {error:#}"),
                    }));
                    return;
                }
            };
            let url = authorization
                .verification_uri_complete
                .as_deref()
                .unwrap_or(&authorization.verification_uri);
            let clipboard_ok = super::helpers::copy_to_clipboard(&authorization.user_code);
            let browser_opened = Self::open_auth_browser(url);
            let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
                url,
                "Scan this on another device to open the xAI verification page:",
            )
            .map(|section| format!("\n\n{section}"))
            .unwrap_or_default();
            let details = format!(
                "Confirm code `{}` in your browser. You do not need to install the Grok CLI.{qr_section}",
                authorization.user_code
            );
            let card = auth_progress::encode_device_login_card(
                "xAI",
                url,
                Some(&authorization.user_code),
                clipboard_ok,
                browser_opened,
                "Waiting for xAI browser approval",
                "Type /cancel to abort",
                &details,
            );
            Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                provider: "grok_build_code".to_string(),
                success: true,
                message: card,
            }));

            match crate::auth::grok_build::complete_device_login(&client, &authorization).await {
                Ok(()) => {
                    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                        provider: "grok-build".to_string(),
                        success: true,
                        message: "Grok Build login complete. Jcode is refreshing the provider and model list."
                            .to_string(),
                    }));
                }
                Err(error) => {
                    Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
                        provider: "grok-build".to_string(),
                        success: false,
                        message: format!("Grok Build login failed: {error}"),
                    }));
                }
            }
        });
    }

    pub(super) fn start_antigravity_login(&mut self) {
        let (verifier, challenge) = crate::auth::oauth::generate_pkce_public();
        let expected_state = crate::auth::oauth::generate_state_public();
        let port = crate::auth::antigravity::DEFAULT_PORT;
        let redirect_uri = crate::auth::antigravity::redirect_uri(port);

        let auth_url = match crate::auth::antigravity::build_auth_url(
            &redirect_uri,
            &challenge,
            &expected_state,
        ) {
            Ok(url) => url,
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Antigravity login is unavailable: {}",
                    e
                )));
                self.set_status_notice("Login: failed");
                return;
            }
        };

        let qr_section = jcode_terminal_prompt::qr::markdown_section_for_tui(
            &auth_url,
            "Scan this on another device if this machine has no browser, then paste the full callback URL or query string here:",
        )
        .map(|section| format!("\n\n{section}"))
        .unwrap_or_default();

        let callback_listener = crate::auth::oauth::bind_callback_listener(port).ok();
        let callback_available = callback_listener.is_some();
        let browser_opened = Self::open_auth_browser(&auth_url);

        if let Some(listener) = callback_listener {
            let verifier_clone = verifier.clone();
            let expected_state_clone = expected_state.clone();
            let redirect_clone = redirect_uri.clone();
            tokio::spawn(async move {
                let code = tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    crate::auth::oauth::wait_for_callback_async_on_listener(
                        listener,
                        &expected_state_clone,
                    ),
                )
                .await
                .map_err(|_| "Login timed out after 5 minutes. Please try again.".to_string())
                .and_then(|result| result.map_err(|e| format!("Callback failed: {}", e)));

                match code {
                    Ok(code) => {
                        match Self::antigravity_token_exchange(
                            verifier_clone,
                            code,
                            Some(expected_state_clone),
                            redirect_clone,
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
                    }
                    Err(e) => {
                        crate::logging::info(&format!(
                            "Antigravity automatic callback did not complete: {}",
                            e
                        ));
                    }
                }
            });
        }

        let callback_line = if callback_available {
            format!(
                "Waiting for callback on {}... (this will complete automatically)\n",
                redirect_uri
            )
        } else {
            format!(
                "Local callback port {} is unavailable, so finish in any browser and paste the full callback URL or query string here.\n",
                redirect_uri
            )
        };
        let preflight = Self::record_oauth_preflight(
            "antigravity",
            browser_opened,
            Some(&redirect_uri),
            Some(callback_available),
        );
        let manual_hint = "If the browser ends on a loopback/callback error page, copy the full URL from the address bar and paste it here immediately.\n";

        let details = format!(
            "{}{}{}{}{}",
            if preflight.is_empty() {
                String::new()
            } else {
                format!("{preflight}\n")
            },
            callback_line,
            manual_hint,
            if preflight.is_empty() {
                String::new()
            } else {
                "Manual-safe fallback is already active here.\n".to_string()
            },
            qr_section.trim_start_matches('\n')
        );

        self.show_oauth_login_progress(
            auth_progress::ANTIGRAVITY_LOGIN_TITLE,
            "Antigravity",
            None,
            &auth_url,
            browser_opened,
            callback_available,
            "Paste callback URL or query string here if needed",
            &details,
        );
        self.set_status_notice("Login: antigravity waiting...");
        self.begin_pending_login(PendingLogin::Antigravity {
            verifier,
            expected_state,
            redirect_uri,
        });
    }

    pub(super) async fn antigravity_token_exchange(
        verifier: String,
        input: String,
        expected_state: Option<String>,
        redirect_uri: String,
    ) -> Result<String, String> {
        let trimmed = input.trim();
        let tokens =
            if antigravity_input_requires_state_validation(trimmed, expected_state.as_deref()) {
                crate::auth::antigravity::exchange_callback_input(
                    &verifier,
                    trimmed,
                    expected_state.as_deref(),
                    &redirect_uri,
                )
                .await
            } else {
                crate::auth::antigravity::exchange_callback_code(trimmed, &verifier, &redirect_uri)
                    .await
            }
            .map_err(|e| e.to_string())?;

        let mut msg = if let Some(email) = tokens.email.as_deref() {
            format!(
                "Successfully logged in to Antigravity! (account: {})",
                email
            )
        } else {
            "Successfully logged in to Antigravity!".to_string()
        };
        if let Some(project_id) = tokens.project_id.as_deref() {
            msg.push_str(&format!(" (project: {})", project_id));
        }
        Ok(msg)
    }
}

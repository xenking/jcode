use super::*;

impl OpenAIProvider {
    pub(super) async fn prefetch_model_catalog(&self) -> Result<()> {
        if self.is_browser_only() {
            return Ok(());
        }
        // The loaded credential's *shape* is authoritative for which catalog
        // endpoint to hit, not the requested credential mode. In Auto mode a
        // user with only an OPENAI_API_KEY loads an API-key-shaped credential
        // while the mode stays Auto; routing by mode would send that platform
        // key to the ChatGPT/Codex endpoint and get a 401.
        let account_label = self.refresh_account_label();
        let (access_token, is_chatgpt_mode, credential_identity, scope) = {
            let creds = self.credentials.read().await;
            (
                creds.access_token.clone(),
                Self::is_chatgpt_mode(&creds),
                Self::catalog_credential_identity(&creds),
                self.selected_catalog_scope_for(&creds),
            )
        };
        let started = std::time::Instant::now();
        let catalog = if is_chatgpt_mode {
            let access_token =
                openai_access_token(account_label.clone(), &self.credentials).await?;
            match jcode_base::provider::fetch_openai_model_catalog(
                &access_token,
                account_label.as_deref(),
            )
            .await
            {
                Ok(catalog) => catalog,
                // The server can reject a token that still looks fresh by its
                // local expiry (revoked/rotated). The chat path recovers by
                // force-refreshing; without the same recovery here the model
                // catalog silently stays stale and newly released models never
                // show up until the user happens to re-login (observed as
                // days of bootstrap 401s in the logs).
                Err(err) if catalog_error_is_auth_rejection(&err) => {
                    let refresh_token = {
                        let creds = self.credentials.read().await;
                        if Self::catalog_credential_identity(&creds) != credential_identity
                            || self.refresh_account_label() != account_label
                        {
                            return Ok(());
                        }
                        creds.refresh_token.clone()
                    };
                    if refresh_token.is_empty() {
                        return Err(err);
                    }
                    jcode_base::logging::info(
                        "OpenAI model catalog fetch rejected the access token; force-refreshing and retrying",
                    );
                    let refreshed =
                        super::super::openai_stream_runtime::force_refresh_openai_token(
                            &self.credentials,
                            &refresh_token,
                            account_label.clone(),
                        )
                        .await
                        .map_err(|refresh_err| {
                            err.context(format!(
                            "token force-refresh after catalog 401/403 also failed: {refresh_err:#}"
                        ))
                        })?;
                    jcode_base::provider::fetch_openai_model_catalog(
                        &refreshed,
                        account_label.as_deref(),
                    )
                    .await?
                }
                Err(err) => return Err(err),
            }
        } else {
            jcode_base::provider::fetch_openai_api_key_model_catalog(&access_token).await?
        };
        let credentials = self.credentials.read().await;
        if Self::catalog_credential_identity(&credentials) != credential_identity
            || self.refresh_account_label() != account_label
        {
            jcode_base::logging::info(
                "Discarding OpenAI model catalog fetched for credentials that are no longer active",
            );
            return Ok(());
        }
        jcode_base::provider::publish_openai_model_catalog_refresh(
            &scope,
            started,
            catalog,
            |catalog| {
                *self
                    .model_reasoning_efforts
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = catalog.reasoning_efforts.clone();
                *self
                    .model_service_tiers
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = catalog.service_tiers.clone();
            },
        );
        drop(credentials);
        self.warn_unsupported_service_tier();
        self.revalidate_reasoning_effort();
        Ok(())
    }
}

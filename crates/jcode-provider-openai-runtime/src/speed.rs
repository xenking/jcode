use super::*;
use jcode_provider_core::{SpeedAvailability, SpeedMode, SpeedOption};

#[derive(Clone)]
pub(super) struct PremiumSpeed {
    pub(super) mode: SpeedMode,
    pub(super) model: String,
    pub(super) route: String,
}

impl OpenAIProvider {
    pub(super) fn verify_speed_route(
        credentials: &CodexCredentials,
        premium_route: Option<&str>,
    ) -> Result<()> {
        if let Some(route) = premium_route {
            anyhow::ensure!(
                Self::speed_route_for(credentials).as_deref() == Some(route),
                "The OpenAI credential route changed after speed was selected; select a speed before retrying"
            );
        }
        Ok(())
    }

    pub(super) fn speed_route_for(credentials: &CodexCredentials) -> Option<String> {
        let oauth = Self::is_chatgpt_mode(credentials);
        if !oauth {
            let api = Self::resolve_api_base();
            if !jcode_base::provider::openai::native_speed_api_endpoint(&api)
                && !(cfg!(test) && api.starts_with("http://127.0.0.1:"))
            {
                return None;
            }
        }
        let identity = if oauth {
            let workspace = credentials
                .account_id
                .as_deref()
                .filter(|id| !id.is_empty())?;
            let user = credentials
                .id_token
                .as_deref()
                .and_then(jcode_base::auth::codex::extract_user_id)
                .or_else(|| jcode_base::auth::codex::extract_user_id(&credentials.access_token))
                .unwrap_or_else(|| credentials.refresh_token.clone());
            if user.is_empty() {
                return None;
            }
            format!("{workspace}\n{user}")
        } else {
            credentials.access_token.clone()
        };
        if identity.is_empty() {
            return None;
        }
        // A persisted selection must not contain a credential or account id.
        let scoped = format!("{}\n{identity}", Self::responses_url(credentials));
        Some(jcode_base::provider::openai_catalog_scope_for_credential(
            false, &scoped,
        ))
    }

    pub(super) fn scope_speed_options(
        &self,
        model: &str,
        credentials: &CodexCredentials,
    ) -> Vec<SpeedOption> {
        if is_chatgpt_web_model(model) || Self::speed_route_for(credentials).is_none() {
            return vec![SpeedOption::standard()];
        }
        let oauth = Self::is_chatgpt_mode(credentials);
        let tiers = self
            .model_service_tiers
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let model = jcode_provider_core::model_id::canonical(model);
        let model = model.strip_suffix("[1m]").unwrap_or(&model);
        let advertised = tiers
            .get(model)
            .or_else(|| tiers.get(jcode_provider_core::model_id::slash_base(model)));
        jcode_provider_core::service_tier::openai_speed_options(
            model,
            oauth,
            advertised.map(Vec::as_slice),
        )
    }

    pub(super) fn requested_premium_speed(&self) -> Option<PremiumSpeed> {
        self.premium_speed
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(super) fn apply_speed(&self, mode: SpeedMode) -> Result<()> {
        let choice = if mode == SpeedMode::Standard {
            None
        } else {
            let credentials = self
                .credentials
                .try_read()
                .context("OpenAI credentials are changing")?;
            let model = self.model();
            self.ensure_speed_option(&model, mode, &credentials)?;
            Some(PremiumSpeed {
                mode,
                model: wire_speed_model(&model).to_owned(),
                route: Self::speed_route_for(&credentials)
                    .context("Cannot identify OpenAI credentials for this speed")?,
            })
        };
        *self
            .premium_speed
            .write()
            .unwrap_or_else(|p| p.into_inner()) = choice;
        *self.service_tier.write().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }

    fn ensure_speed_option(
        &self,
        model: &str,
        mode: SpeedMode,
        credentials: &CodexCredentials,
    ) -> Result<()> {
        let option = self
            .scope_speed_options(model, credentials)
            .into_iter()
            .find(|option| option.mode == mode);
        if !option.as_ref().is_some_and(|option| {
            selectable_speed(option.availability, Self::is_chatgpt_mode(credentials))
        }) {
            anyhow::bail!(
                "OpenAI {mode:?} speed is unavailable for {model}: {}",
                option
                    .and_then(|option| option.reason)
                    .as_deref()
                    .unwrap_or("unsupported credential route")
            );
        }
        Ok(())
    }

    pub(super) fn request_speed_tier(
        &self,
        model: &str,
        credentials: &CodexCredentials,
    ) -> Result<(Option<String>, Option<String>)> {
        let Some(choice) = self.requested_premium_speed() else {
            // Codex represents Standard by omitting the tier. The platform API
            // would instead inherit a project's potentially premium default.
            let tier = self.effective_service_tier(model).or_else(|| {
                (!Self::is_chatgpt_mode(credentials)
                    && Self::speed_route_for(credentials).is_some())
                .then(|| "default".into())
            });
            return Ok((tier, None));
        };
        anyhow::ensure!(
            wire_speed_model(model) == choice.model
                && Self::speed_route_for(credentials).as_deref() == Some(&choice.route),
            "OpenAI model or credential route changed; select a speed before retrying"
        );
        self.ensure_speed_option(model, choice.mode, credentials)?;
        Ok((
            Some(speed_wire_tier(choice.mode, Self::is_chatgpt_mode(credentials)).into()),
            Some(choice.route),
        ))
    }
}

pub(super) fn selectable_speed(availability: SpeedAvailability, oauth: bool) -> bool {
    match availability {
        SpeedAvailability::Available => true,
        SpeedAvailability::Unavailable => false,
        // API project access is checked at request time. A Codex login must
        // advertise the tier before the user can select it.
        SpeedAvailability::Unknown => !oauth,
    }
}

fn wire_speed_model(model: &str) -> &str {
    model.strip_suffix("[1m]").unwrap_or(model)
}

pub(super) fn speed_wire_tier(mode: SpeedMode, oauth: bool) -> &'static str {
    match mode {
        SpeedMode::Standard => "default",
        SpeedMode::Fast if oauth => "priority",
        SpeedMode::Fast => "fast",
        SpeedMode::Ultrafast => "ultrafast",
    }
}

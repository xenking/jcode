use super::{AnthropicProvider, Provider, Result, SpeedMode};

impl AnthropicProvider {
    fn legacy_fast_uses_speed(model: &str) -> bool {
        Self::normalized_model_key(model) == "claude-opus-5-5"
    }

    pub(super) fn selected_service_tier(&self) -> Option<String> {
        if Self::legacy_fast_uses_speed(&self.model()) {
            return (self.speed_mode() == SpeedMode::Fast).then(|| "priority".to_string());
        }
        match self
            .current_service_tier_for_model(&self.model())
            .as_deref()
        {
            Some("auto") => Some("priority".to_string()),
            _ => None,
        }
    }

    pub(super) fn select_service_tier(&self, service_tier: &str) -> Result<()> {
        // Legacy TUI aliases select speed on Opus 5.5, not API capacity.
        if Self::legacy_fast_uses_speed(&self.model()) {
            match service_tier.trim().to_ascii_lowercase().as_str() {
                "priority" => return self.set_speed_mode(SpeedMode::Fast),
                "off" => return self.set_speed_mode(SpeedMode::Standard),
                _ => {}
            }
        }
        let normalized = Self::normalize_service_tier(service_tier)?;
        if normalized.as_deref() == Some("auto")
            && !Self::model_supports_priority_service_tier(&self.model())
        {
            anyhow::bail!("Anthropic auto capacity tier is only supported for Claude Opus 4.8");
        }
        *self
            .service_tier
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = normalized;
        Ok(())
    }

    pub(super) fn selectable_service_tiers(&self) -> Vec<&'static str> {
        let model = self.model();
        let has_fast_speed =
            Self::legacy_fast_uses_speed(&model) && self.fast_target(&model).is_some();
        if has_fast_speed || Self::model_supports_priority_service_tier(&model) {
            vec!["off", "priority"]
        } else {
            vec![]
        }
    }
}

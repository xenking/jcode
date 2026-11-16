//! Model catalog refresh fanout across runtime slots and compatible profiles.
use super::*;

struct CatalogTarget {
    label: String,
    provider: Arc<dyn Provider>,
    required: bool,
}

fn catalog_slot<P: ?Sized>(slot: &RwLock<Option<Arc<P>>>) -> Option<Arc<P>> {
    slot.read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

async fn refresh_child_catalog(provider: &dyn Provider, force: bool) -> Result<()> {
    if force {
        provider.refresh_model_catalog().await.map(|_| ())
    } else {
        provider.prefetch_models().await
    }
}

impl MultiProvider {
    fn catalog_targets(&self) -> Vec<CatalogTarget> {
        let active = self.active_provider();
        let active_profile = ProviderRegistry::new(self).active_compatible_profile_id();
        let slots = [
            (
                ActiveProvider::Claude,
                "anthropic",
                catalog_slot(&self.anthropic),
            ),
            (ActiveProvider::OpenAI, "openai", catalog_slot(&self.openai)),
            (
                ActiveProvider::OpenRouter,
                "openrouter",
                catalog_slot(&self.openrouter),
            ),
            (
                ActiveProvider::Copilot,
                "copilot",
                catalog_slot(&self.copilot_api),
            ),
            (
                ActiveProvider::Antigravity,
                "antigravity",
                catalog_slot(&self.antigravity),
            ),
            (ActiveProvider::Gemini, "gemini", catalog_slot(&self.gemini)),
            (ActiveProvider::Cursor, "cursor", catalog_slot(&self.cursor)),
            (
                ActiveProvider::Bedrock,
                "bedrock",
                catalog_slot(&self.bedrock).map(|provider| provider as Arc<dyn Provider>),
            ),
        ];
        let mut targets: Vec<_> = slots
            .into_iter()
            .filter_map(|(kind, label, provider)| {
                provider.map(|provider| CatalogTarget {
                    label: label.to_owned(),
                    provider,
                    required: kind == active
                        && kind != ActiveProvider::Bedrock
                        && (kind != ActiveProvider::OpenRouter || active_profile.is_none()),
                })
            })
            .collect();
        // Every configured compatible endpoint has an independent catalog,
        // even when the real OpenRouter slot is empty or a different slot is active.
        targets.extend(
            self.openai_compatible_profiles
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|(id, provider)| CatalogTarget {
                    label: id.clone(),
                    provider: Arc::clone(provider),
                    required: active_profile.as_deref() == Some(id.as_str()),
                }),
        );
        targets
    }

    pub(super) async fn refresh_catalogs(&self, force: bool) -> Result<()> {
        // One consumption path for both single runtime slots and endpoint maps.
        // Adding a target cannot silently skip forced refresh or error handling.
        let results = futures::future::join_all(self.catalog_targets().into_iter().map(
            |target| async move {
                let result = refresh_child_catalog(target.provider.as_ref(), force).await;
                (target, result)
            },
        ))
        .await;
        let mut errors = Vec::new();
        let mut optional_errors = Vec::new();
        for (target, result) in results {
            if let Err(err) = result {
                let message = format!("{}: {err}", target.label);
                if target.required {
                    errors.push(message);
                } else {
                    optional_errors.push(message);
                }
            }
        }
        if !optional_errors.is_empty() {
            crate::logging::warn(&format!(
                "Optional model catalog refresh failed: {}",
                optional_errors.join("; ")
            ));
        }

        // Bedrock's built-in default is a 2024 model; once its catalog (live
        // or cached) is known, land on the newest usable Claude flagship.
        if let Some(bedrock) = self.bedrock_provider() {
            promote_bedrock_placeholder_default(&bedrock);
        }

        // Other providers may have refreshed successfully even if the active
        // provider failed. Never retain a stale memo after partial success.
        self.invalidate_routes_memo_globally();
        if !errors.is_empty() {
            return Err(anyhow!("{}", errors.join("; ")));
        }
        Ok(())
    }
}

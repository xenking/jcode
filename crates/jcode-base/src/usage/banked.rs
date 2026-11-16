//! Per-account banked usage resets for compact status displays.
//!
//! Both account rows and usage reports derive resets from the same cached
//! per-account provider response, not independent observations.

use std::sync::Mutex;

/// Banked resets known for one saved account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountBankedResets {
    /// Unexpired saved resets, including those not currently usable.
    pub available: u64,
    /// Number currently usable, when reported.
    pub usable: Option<u64>,
    pub cached_source: Option<String>,
    /// RFC3339 time the next reset becomes available when none are left.
    pub next_available_at: Option<String>,
    /// RFC3339 expiry of the soonest-expiring claimable reset, when reported.
    pub expires_at: Option<String>,
}

impl AccountBankedResets {
    /// Soonest known expiry among claimable resets.
    pub fn earliest_expiry(expirations: &[Option<String>]) -> Option<String> {
        expirations
            .iter()
            .flatten()
            .filter_map(|raw| match chrono::DateTime::parse_from_rfc3339(raw) {
                Ok(at) => Some((at, raw.clone())),
                Err(error) => {
                    crate::logging::info(&format!("Ignoring invalid banked reset expiry: {error}"));
                    None
                }
            })
            .min_by_key(|(at, _)| *at)
            .map(|(_, raw)| raw)
    }
}

fn anthropic_account_resets(
    offer: Option<&jcode_usage_types::AnthropicLimitResetOffer>,
    banked: Option<&jcode_usage_types::AnthropicBankedResets>,
) -> Option<AccountBankedResets> {
    if offer.is_none() && banked.is_none() {
        return None;
    }
    let session = offer.is_some_and(|offer| offer.available);
    // Saved grants and the independent weekly session offer are different
    // programs and must not be added together.
    let available = banked.map_or(u64::from(session), |b| b.available_count);
    Some(AccountBankedResets {
        available,
        usable: banked.map(|b| b.usable_count),
        cached_source: banked.and_then(|b| b.cached_source.clone()),
        next_available_at: (available == 0)
            .then(|| offer.and_then(|offer| offer.next_available_at.clone()))
            .flatten(),
        expires_at: banked.and_then(|b| b.expires_at.clone()),
    })
}

/// Cached banked resets for a saved account. Never performs a network probe.
pub fn cached_account_banked_resets(
    provider: super::MultiAccountProviderKind,
    label: &str,
) -> Option<AccountBankedResets> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    match provider {
        super::MultiAccountProviderKind::Anthropic => {
            let value = super::disk_cache::reset_status(&format!("label:{label}"));
            let offer = value
                .as_ref()
                .and_then(|v| super::anthropic_reset::offer_from_status(v, Some(label)));
            let banked = value
                .as_ref()
                .and_then(|v| {
                    super::anthropic_banked::banked_from_status(v, Some(label), chrono::Utc::now())
                })
                .or_else(|| {
                    let account = crate::auth::claude::list_accounts()
                        .ok()?
                        .into_iter()
                        .find(|a| a.label == label)?;
                    super::anthropic_banked::cached_banked_resets(&account.access, Some(label))
                });
            anthropic_account_resets(offer.as_ref(), banked.as_ref())
        }
        super::MultiAccountProviderKind::OpenAI => {
            let map = try_cache_lock(super::cache::openai_usage_cache())?;
            let data = map.get(&format!("label:{label}"))?;
            if data.last_error.is_some() {
                return None;
            }
            data.openai_reset_credits
                .as_ref()
                .map(|credits| AccountBankedResets {
                    available: credits.available_count,
                    usable: None,
                    cached_source: None,
                    next_available_at: None,
                    expires_at: AccountBankedResets::earliest_expiry(
                        &credits.available_expirations,
                    ),
                })
        }
    }
}

fn try_cache_lock<T>(mutex: &Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::WouldBlock) => None,
        Err(std::sync::TryLockError::Poisoned(_)) => {
            crate::logging::info("Banked reset cache poisoned, skipping cached offer");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_expirations_do_not_hide_valid_ones() {
        assert_eq!(
            AccountBankedResets::earliest_expiry(&[
                Some("invalid".into()),
                Some("2099-02-01T00:00:00Z".into()),
                Some("2099-01-01T00:00:00Z".into()),
            ])
            .as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
    }

    #[test]
    fn busy_and_poisoned_caches_are_unavailable() {
        let cache = Mutex::new(());
        let guard = cache.lock().unwrap();
        assert!(try_cache_lock(&cache).is_none());
        drop(guard);
        let _ = std::panic::catch_unwind(|| {
            let _guard = cache.lock().unwrap();
            panic!("poison cache");
        });
        assert!(try_cache_lock(&cache).is_none());
    }

    #[test]
    fn anthropic_account_resets_uses_saved_grants_without_adding_session_offers() {
        let label = "banked-test-account";
        let offer = jcode_usage_types::AnthropicLimitResetOffer {
            account_label: Some(label.into()),
            available: false,
            next_available_at: Some("2099-01-01T00:00:00Z".into()),
            resets_per_week: 1,
        };
        let got = anthropic_account_resets(Some(&offer), None).unwrap();
        assert_eq!(got.available, 0);
        assert_eq!(
            got.next_available_at.as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        let banked = jcode_usage_types::AnthropicBankedResets {
            account_label: Some(label.into()),
            available_count: 2,
            usable_count: 1,
            cached_source: None,
            expires_at: Some("2099-02-01T00:00:00Z".into()),
        };
        let got = anthropic_account_resets(Some(&offer), Some(&banked)).unwrap();
        assert_eq!(got.available, 2);
        assert_eq!(got.usable, Some(1));
        assert!(got.next_available_at.is_none());
        assert_eq!(got.expires_at.as_deref(), Some("2099-02-01T00:00:00Z"));
        let ready_offer = jcode_usage_types::AnthropicLimitResetOffer {
            available: true,
            ..offer
        };
        assert_eq!(
            anthropic_account_resets(Some(&ready_offer), Some(&banked))
                .unwrap()
                .available,
            2
        );
        assert!(anthropic_account_resets(None, None).is_none());
    }
}

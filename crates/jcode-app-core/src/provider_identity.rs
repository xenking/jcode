//! Which OAuth account a provider is serving, for telemetry and client display.

use crate::provider::Provider;

/// Do not probe any identity until telemetry is enabled. Only the provider
/// may identify its effective OAuth credential. An API-key route must not expose the email
/// of an unrelated saved subscription account.
pub(crate) fn provider_oauth_email(
    provider: &dyn Provider,
) -> Option<crate::protocol::ProviderOAuthEmail> {
    selected_provider_oauth_email(provider, crate::telemetry::is_enabled(), || {
        provider.active_oauth_account_email()
    })
}

pub(crate) fn selected_provider_oauth_email(
    provider: &dyn Provider,
    telemetry_enabled: bool,
    resolve: impl FnOnce() -> Option<String>,
) -> Option<crate::protocol::ProviderOAuthEmail> {
    if !telemetry_enabled
        || provider.active_resolved_credential()
            == Some(jcode_provider_core::ResolvedCredential::ApiKey)
    {
        return None;
    }
    // The provider owns the effective credential snapshot, including unbound
    // active OAuth and account failover. Never substitute a global login here.
    resolve().map(crate::protocol::ProviderOAuthEmail::new)
}

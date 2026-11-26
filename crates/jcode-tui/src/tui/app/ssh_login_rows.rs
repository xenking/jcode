use crate::provider_catalog::{self, LoginProviderDescriptor, LoginProviderTarget};
use crate::tui::{PickerAction, PickerEntry, PickerOption};

pub(super) fn remote_setup_detail(
    provider: LoginProviderDescriptor,
    host: &str,
    supported: &[&str],
) -> String {
    if supported.contains(&provider.id) {
        let prerequisite = if provider.id == "google" {
            " Google OAuth client credentials must already be configured on the SSH host."
        } else {
            ""
        };
        format!(
            "Browser approval opens on this computer, credentials stay on the SSH host.{prerequisite} /login {}",
            provider.id
        )
    } else if matches!(provider.target, LoginProviderTarget::AutoImport) {
        format!(
            "Remote-host setup required: open Jcode directly on {host} and use /login auto-import to review that host's other-tool logins. This SSH picker cannot perform Auto Import. To copy this computer's Jcode-managed login instead, choose an explicit Import local entry. No local credentials are accessed here."
        )
    } else {
        format!(
            "Remote-host setup required: open Jcode directly on {host} and use /login {} to configure this provider there. This authentication method is not supported by the SSH login flow. No local authentication is started and no local credentials are accessed here.",
            provider.id
        )
    }
}

fn provider_entry(
    provider: LoginProviderDescriptor,
    host: &str,
    supported: &[&str],
) -> PickerEntry {
    entry(
        provider.display_name.into(),
        PickerOption {
            provider: provider.auth_kind.label().into(),
            api_method: "checking remote".into(),
            available: true,
            detail: format!(
                "Destination: {host}. Checking remote login status. {} · {}",
                provider.menu_detail,
                remote_setup_detail(provider, host, supported)
            ),
            estimated_reference_cost_micros: None,
            icon: None,
        },
        PickerAction::RemoteLogin {
            provider: provider.id,
            import: false,
        },
        provider.recommended,
    )
}

fn import_entry(provider: &'static str, label: &str, host: &str) -> PickerEntry {
    entry(
        format!("Import local {label} login"),
        PickerOption {
            provider: "Local → SSH".into(),
            api_method: "confirm copy".into(),
            available: true,
            detail: format!(
                "Destination: {host}. One-time copy of your selected Jcode-managed {provider} account. Confirmation required. No local credentials read yet. Existing remote logins are never overwritten."
            ),
            estimated_reference_cost_micros: None,
            icon: None,
        },
        PickerAction::RemoteLogin {
            provider,
            import: true,
        },
        matches!(provider, "openai" | "claude"),
    )
}

fn entry(
    name: String,
    option: PickerOption,
    action: PickerAction,
    recommended: bool,
) -> PickerEntry {
    PickerEntry {
        name,
        options: vec![option],
        action,
        selected_option: 0,
        is_current: false,
        is_default: false,
        is_favorite: false,
        recommended,
        recommendation_rank: usize::MAX,
        usage_score: 0,
        old: false,
        created_date: None,
        effort: None,
    }
}

pub(super) fn login_rows(host: &str, supported: &[&str], imports_only: bool) -> Vec<PickerEntry> {
    let mut entries = if imports_only {
        Vec::new()
    } else {
        provider_catalog::tui_login_providers()
            .into_iter()
            .map(|provider| provider_entry(provider, host, supported))
            .collect()
    };
    entries.extend([
        import_entry("openai", "OpenAI", host),
        import_entry("claude", "Claude", host),
    ]);
    entries
}

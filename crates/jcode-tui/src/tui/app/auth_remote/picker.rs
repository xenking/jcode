//! Shared inline picker presentation, populated only from the SSH host's status.
use super::{App, Phase, command};
use crate::provider_catalog;
use crate::tui::{InlineInteractiveState, PickerAction, PickerKind};

impl App {
    pub(super) fn open_ssh_login_picker(&mut self, imports_only: bool) {
        let Some(login) = self.remote_login.as_ref() else {
            return;
        };
        let host = login.target.host().to_string();
        let entries =
            crate::tui::app::ssh_login_rows::login_rows(&host, &super::PROVIDERS, imports_only);
        self.clear_inline_view();
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Login,
            filtered: (0..entries.len()).collect(),
            entries,
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        self.set_status_notice(format!(
            "Login on {host}: choose a provider or Import local login. Some methods require setup directly on the SSH host. Esc cancels."
        ));
        if tokio::runtime::Handle::try_current().is_ok() {
            self.remote_login
                .as_mut()
                .unwrap()
                .run(command::Operation::Status, None);
        } else {
            self.update_ssh_login_picker_status(None);
        }
    }

    pub(super) fn update_ssh_login_picker_status(
        &mut self,
        statuses: Option<&[command::ProviderStatus]>,
    ) {
        let Some(login) = self
            .remote_login
            .as_ref()
            .filter(|login| login.phase == Phase::Choosing)
        else {
            return;
        };
        let host = login.target.host();
        let Some(picker) = self.inline_interactive_state.as_mut() else {
            return;
        };
        for entry in &mut picker.entries {
            let PickerAction::RemoteLogin { provider, import } = entry.action else {
                continue;
            };
            if import {
                continue;
            }
            let Some(descriptor) = provider_catalog::login_providers()
                .iter()
                .find(|descriptor| descriptor.id == provider)
                .copied()
            else {
                continue;
            };
            let status =
                statuses.and_then(|statuses| statuses.iter().find(|status| status.id == provider));
            let label = match status.map(|status| status.state) {
                Some(crate::auth::AuthState::Available) => "configured",
                Some(crate::auth::AuthState::Expired) => "attention",
                Some(crate::auth::AuthState::NotConfigured) => "setup",
                None => "status unknown",
            };
            entry.is_current = matches!(
                status.map(|status| status.state),
                Some(crate::auth::AuthState::Available)
            );
            entry.options[0].api_method = label.into();
            entry.options[0].detail = format!(
                "Destination: {host}. Remote status: {label} ({}). {} · {}",
                status
                    .map(|status| status.method_detail.as_str())
                    .unwrap_or("could not read remote status; reopen /login to retry"),
                descriptor.menu_detail,
                crate::tui::app::ssh_login_rows::remote_setup_detail(
                    descriptor,
                    host,
                    &super::PROVIDERS
                ),
            );
        }
        // Preserve the user's filter, cursor, and selection as async status arrives.
    }
}

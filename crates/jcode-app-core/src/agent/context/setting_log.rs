//! Every change of provider, model, reasoning effort or mode the user did not make
//! is written into the session's chat log as `old → new (cause)`. His own changes
//! are tracked but not written: the footer shows them. Mutators call
//! [`Agent::begin_setting_change`] first and [`Agent::log_setting_changes`] last;
//! the request boundary calls the latter again as a net for paths that do not.

use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Settings {
    provider: String,
    model: String,
    effort: String,
    mode: String,
}

/// The settings the chat log last reported; unset until the first observation.
#[derive(Default)]
pub(super) struct SettingLog(Option<Settings>);

const UNRECORDED_CAUSE: &str = "changed outside a recorded path, e.g. an account re-login";

/// One line, bounded: provider errors may be multi-line JSON.
fn one_line_cause(error: &str) -> String {
    let line = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    crate::util::truncate_str(line, 200).to_string()
}

impl Agent {
    fn current_settings(&self) -> Settings {
        let provider = self.provider_name();
        let provider = match self.provider.serving_oauth_account_label() {
            Some(account) => {
                let key = match jcode_provider_core::parse_provider_hint(&provider) {
                    Some(jcode_provider_core::ActiveProvider::OpenAI) => "openai",
                    _ => "claude",
                };
                let named = format!("{provider} ({account})");
                match jcode_base::routing_accounts::resolved_account_icon(&format!(
                    "{key}:{account}"
                )) {
                    Some(icon) => format!("{icon} {named}"),
                    None => named,
                }
            }
            None => provider,
        };
        let mode = self.session.pinned_skill_names().join(", ");
        Settings {
            provider,
            model: self.provider_model(),
            effort: self
                .provider
                .reasoning_effort()
                .unwrap_or_else(|| "default".into()),
            mode: if mode.is_empty() { "off".into() } else { mode },
        }
    }

    /// Remember the settings before a mutation. Idempotent.
    pub(crate) fn begin_setting_change(&mut self) {
        if self.setting_log.0.is_none() {
            self.setting_log.0 = Some(self.current_settings());
        }
    }

    /// Why the next logged change happens (failover, a queued change applied at
    /// the turn boundary). Consumed by the next [`Self::log_setting_changes`].
    pub(crate) fn set_setting_cause(&mut self, cause: impl Into<String>) {
        self.setting_cause = Some(cause.into());
    }

    /// Forgets a cause set for a change that did not happen.
    pub(crate) fn clear_setting_cause(&mut self) {
        self.setting_cause = None;
    }

    /// The next change is an automatic failover; record the provider error that
    /// triggered it.
    pub(crate) fn set_failover_cause(&mut self, error: &str) {
        self.set_setting_cause(format!(
            "automatic failover after: {}",
            one_line_cause(error)
        ));
    }

    /// Write what changed since the last report into the chat log, persist it and
    /// publish it to attached clients.
    pub(crate) fn log_setting_changes(&mut self, default_cause: &str) {
        let current = self.current_settings();
        let Some(previous) = self.setting_log.0.replace(current.clone()) else {
            return;
        };
        let cause = self
            .setting_cause
            .take()
            .unwrap_or_else(|| default_cause.to_owned());
        if crate::session::is_user_setting_cause(&cause) {
            return;
        }
        let rows = [
            ("Provider", &previous.provider, &current.provider),
            ("Model", &previous.model, &current.model),
            ("Reasoning effort", &previous.effort, &current.effort),
            ("Mode", &previous.mode, &current.mode),
        ];
        let mut changed = false;
        for (name, old, new) in rows {
            if old != new {
                changed = true;
                let message = format!("{name}: {old} → {new} ({cause})");
                self.session
                    .record_replay_display_message("setting_change", None, message.clone());
                crate::bus::Bus::global().publish(crate::bus::BusEvent::SettingChange {
                    session_id: self.session.id().to_owned(),
                    message,
                });
            }
        }
        if changed {
            self.persist_session_best_effort("setting change");
        }
    }

    /// Request-boundary net for changes made without a recorded cause.
    pub(super) fn log_unrecorded_setting_changes(&mut self) {
        self.begin_setting_change();
        self.log_setting_changes(UNRECORDED_CAUSE);
    }
}

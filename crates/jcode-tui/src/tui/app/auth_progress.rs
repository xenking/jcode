use super::*;
use crate::tui::app::login_card::encode_oauth_login_card;

pub(super) const OPENAI_LOGIN_TITLE: &str = "OpenAI login";
pub(super) const CLAUDE_LOGIN_TITLE: &str = "Claude login";
pub(super) const GEMINI_LOGIN_TITLE: &str = "Gemini login";
pub(super) const ANTIGRAVITY_LOGIN_TITLE: &str = "Antigravity login";
pub(super) const COPILOT_LOGIN_TITLE: &str = "Copilot login";
pub(super) const JCODE_LOGIN_TITLE: &str = "Jcode login";
pub(super) const GROK_BUILD_LOGIN_TITLE: &str = "Grok Build login";
pub(super) const MUSE_LOGIN_TITLE: &str = "Muse login";

pub(in crate::tui::app) fn login_card_title_for_provider(provider: &str) -> Option<&'static str> {
    if provider.starts_with("muse_subscription_code#") || provider.starts_with("muse-subscription#")
    {
        return Some(MUSE_LOGIN_TITLE);
    }
    match provider {
        "openai" | "openai_device_code" => Some(OPENAI_LOGIN_TITLE),
        "claude" => Some(CLAUDE_LOGIN_TITLE),
        "gemini" => Some(GEMINI_LOGIN_TITLE),
        "antigravity" => Some(ANTIGRAVITY_LOGIN_TITLE),
        "copilot" | "copilot_code" => Some(COPILOT_LOGIN_TITLE),
        "jcode" => Some(JCODE_LOGIN_TITLE),
        "grok-build" => Some(GROK_BUILD_LOGIN_TITLE),
        "muse-subscription" => Some(MUSE_LOGIN_TITLE),
        _ => None,
    }
}

pub(in crate::tui::app) fn login_card_title_for_pending(
    pending: &PendingLogin,
) -> Option<&'static str> {
    match pending {
        PendingLogin::OpenAiAccount { .. } => Some(OPENAI_LOGIN_TITLE),
        PendingLogin::ClaudeAccount { .. } => Some(CLAUDE_LOGIN_TITLE),
        PendingLogin::Gemini { .. } => Some(GEMINI_LOGIN_TITLE),
        PendingLogin::Antigravity { .. } => Some(ANTIGRAVITY_LOGIN_TITLE),
        PendingLogin::Copilot => Some(COPILOT_LOGIN_TITLE),
        PendingLogin::GrokBuild => Some(GROK_BUILD_LOGIN_TITLE),
        PendingLogin::MuseSubscription { .. } => Some(MUSE_LOGIN_TITLE),
        PendingLogin::Exchanging { provider } => login_card_title_for_provider(provider),
        _ => None,
    }
}

pub(in crate::tui::app) fn login_retry_command_for_provider(provider: &str) -> &'static str {
    if provider.starts_with("muse-subscription") {
        return "/login muse-subscription";
    }
    match provider {
        "openai" => "/login openai",
        "claude" => "/login claude",
        "gemini" => "/login gemini",
        "antigravity" => "/login antigravity",
        "copilot" => "/login copilot",
        "jcode" => "/account jcode login",
        "grok-build" => "/login grok-build",
        _ => "/login",
    }
}

impl App {
    pub(in crate::tui::app) fn upsert_login_progress_card(&mut self, title: &str, content: String) {
        if let Some(index) = self.display_messages.iter().rposition(|message| {
            message.role == "system" && message.title.as_deref() == Some(title)
        }) {
            self.replace_display_message_content(index, content);
            return;
        }
        let mut message = DisplayMessage::system(content);
        message.title = Some(title.to_string());
        self.push_display_message(message);
    }

    pub(in crate::tui::app) fn update_login_progress_exchange(
        &mut self,
        title: &str,
        account_label: Option<&str>,
    ) {
        let text = match account_label {
            Some(label) => {
                format!("{title} [{label}]: callback received, exchanging credentials...")
            }
            None => format!("{title}: callback received, exchanging credentials..."),
        };
        if let Some(index) = self.display_messages.iter().rposition(|message| {
            message.role == "system" && message.title.as_deref() == Some(title)
        }) {
            self.replace_display_message_content(index, text);
        } else {
            let mut message = DisplayMessage::system(text);
            message.title = Some(title.to_string());
            self.push_display_message(message);
        }
    }

    pub(in crate::tui::app) fn update_openai_login_exchange(&mut self, label: &str) {
        self.update_login_progress_exchange(OPENAI_LOGIN_TITLE, Some(label));
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "OAuth progress card accepts provider display metadata, browser/callback state, and details"
    )]
    pub(in crate::tui::app) fn show_oauth_login_progress(
        &mut self,
        title: &str,
        provider_label: &str,
        account_label: Option<&str>,
        url: &str,
        browser_opened: bool,
        callback_available: bool,
        paste_hint: &str,
        details: &str,
    ) {
        let content = encode_oauth_login_card(
            provider_label,
            account_label,
            url,
            browser_opened,
            callback_available,
            paste_hint,
            details,
        );
        self.upsert_login_progress_card(title, content);
    }

    pub(in crate::tui::app) fn show_openai_login_progress(
        &mut self,
        label: &str,
        url: &str,
        browser_opened: bool,
        callback_available: bool,
        details: &str,
    ) {
        self.show_oauth_login_progress(
            OPENAI_LOGIN_TITLE,
            "OpenAI",
            Some(label),
            url,
            browser_opened,
            callback_available,
            "Paste callback URL here if needed",
            details,
        );
    }

    /// Replace the pending card, removing stale links and instructions entirely.
    pub(in crate::tui::app) fn finish_login_progress(
        &mut self,
        title: &str,
        content: String,
    ) -> bool {
        let Some(index) = self.display_messages.iter().rposition(|message| {
            message.role == "system" && message.title.as_deref() == Some(title)
        }) else {
            return false;
        };
        self.replace_display_message_title_and_content(index, None, content)
    }

    pub(in crate::tui::app) fn finish_openai_login_progress(&mut self, content: String) -> bool {
        self.finish_login_progress(OPENAI_LOGIN_TITLE, content)
    }
}

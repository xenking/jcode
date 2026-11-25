//! `[ambient]`, `[notifications]` and `[safety]` sections.

use serde::{Deserialize, Serialize};

/// Ambient mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AmbientConfig {
    /// Enable ambient mode (default: false)
    pub enabled: bool,
    /// Provider override (default: auto-select)
    pub provider: Option<String>,
    /// Model override (default: provider's strongest)
    pub model: Option<String>,
    /// Allow API key usage (default: false, only OAuth)
    pub allow_api_keys: bool,
    /// Daily token budget when using API keys
    pub api_daily_budget: Option<u64>,
    /// Minimum interval between cycles in minutes (default: 5)
    pub min_interval_minutes: u32,
    /// Maximum interval between cycles in minutes (default: 120)
    pub max_interval_minutes: u32,
    /// Pause ambient when user has active session (default: true)
    pub pause_on_active_session: bool,
    /// Enable proactive work vs garden-only (default: true)
    pub proactive_work: bool,
    /// Proactive work branch prefix (default: "ambient/")
    pub work_branch_prefix: String,
    /// Show ambient cycle in a terminal window (default: true)
    pub visible: bool,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            model: None,
            allow_api_keys: false,
            api_daily_budget: None,
            min_interval_minutes: 5,
            max_interval_minutes: 120,
            pause_on_active_session: true,
            proactive_work: true,
            work_branch_prefix: "ambient/".to_string(),
            visible: true,
        }
    }
}

/// Desktop notification configuration for interactive sessions.
///
/// Unlike `[safety]` (ambient-mode ntfy/email/channel notifications), this
/// section controls lightweight local desktop notifications for the normal
/// interactive TUI, e.g. "agent finished a long turn".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Send a desktop notification when an agent turn completes (default: true).
    /// Notifications fire only for long turns (see thresholds below) and, by
    /// default, only while the terminal window is unfocused.
    pub turn_complete: bool,
    /// Minimum turn duration, in seconds, before a completed turn notifies
    /// (default: 120).
    pub turn_complete_min_secs: u64,
    /// Lower duration threshold, in seconds, used when the session has todos
    /// recorded, since todos indicate longer task-style work (default: 30).
    pub turn_complete_todo_min_secs: u64,
    /// Only notify while the terminal window is unfocused (default: true).
    /// Requires a terminal that reports focus events (most modern terminals).
    pub turn_complete_only_when_unfocused: bool,
    /// macOS Notification Center sound name played on turn completion
    /// (e.g. "Glass", "Ping", "Hero"). Empty string disables the sound.
    /// Ignored on non-macOS platforms. Default: "Glass".
    pub turn_complete_sound: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            turn_complete: true,
            turn_complete_min_secs: 120,
            turn_complete_todo_min_secs: 30,
            turn_complete_only_when_unfocused: true,
            turn_complete_sound: "Glass".to_string(),
        }
    }
}

/// Safety system & notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// ntfy.sh topic name (required for push notifications)
    pub ntfy_topic: Option<String>,
    /// ntfy.sh server URL (default: https://ntfy.sh)
    pub ntfy_server: String,
    /// Enable desktop notifications via notify-send (default: true)
    pub desktop_notifications: bool,
    /// Enable email notifications (default: false)
    pub email_enabled: bool,
    /// Email recipient
    pub email_to: Option<String>,
    /// SMTP host (e.g. smtp.gmail.com)
    pub email_smtp_host: Option<String>,
    /// SMTP port (default: 587)
    pub email_smtp_port: u16,
    /// Email sender address
    pub email_from: Option<String>,
    /// SMTP password (prefer JCODE_SMTP_PASSWORD env var)
    pub email_password: Option<String>,
    /// IMAP host for receiving email replies (e.g. imap.gmail.com)
    pub email_imap_host: Option<String>,
    /// IMAP port (default: 993)
    pub email_imap_port: u16,
    /// Enable email reply → agent directive feature (default: false)
    pub email_reply_enabled: bool,
    /// Enable Telegram notifications (default: false)
    pub telegram_enabled: bool,
    /// Telegram bot token (from @BotFather)
    pub telegram_bot_token: Option<String>,
    /// Telegram chat ID to send messages to
    pub telegram_chat_id: Option<String>,
    /// Enable Telegram reply → agent directive feature (default: false)
    pub telegram_reply_enabled: bool,
    /// Enable Discord notifications (default: false)
    pub discord_enabled: bool,
    /// Discord bot token
    pub discord_bot_token: Option<String>,
    /// Discord channel ID to send messages to
    pub discord_channel_id: Option<String>,
    /// Discord bot user ID (for filtering own messages in polling)
    pub discord_bot_user_id: Option<String>,
    /// Enable Discord reply → agent directive feature (default: false)
    pub discord_reply_enabled: bool,
    /// Enable the Jade cloud relay channel (remote control via cloud mailbox, default: false)
    pub jade_relay_enabled: bool,
    /// Jade relay API base URL (e.g. https://...lambda-url.us-east-1.on.aws/)
    pub jade_relay_api_base: Option<String>,
    /// Jade relay bearer token (prefer JCODE_JADE_RELAY_TOKEN env var)
    pub jade_relay_token: Option<String>,
    /// Jade relay token id header (x-jade-token-id), used for fast token lookup
    pub jade_relay_token_id: Option<String>,
    /// Jade relay user id (channel scope; defaults to the token's user when omitted)
    pub jade_relay_user_id: Option<String>,
    /// Jade relay session id to bind this laptop's listener to (the channel = user_id/session_id)
    pub jade_relay_session_id: Option<String>,
    /// Enable Jade relay prompt → agent directive feature (default: false)
    pub jade_relay_reply_enabled: bool,
    /// Enable Jade relay device launch commands that open headed local sessions (default: false)
    pub jade_relay_launch_enabled: bool,
    /// Default working directory for remotely launched headed sessions
    pub jade_relay_launch_working_dir: Option<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            ntfy_topic: None,
            ntfy_server: "https://ntfy.sh".to_string(),
            desktop_notifications: true,
            email_enabled: false,
            email_to: None,
            email_smtp_host: None,
            email_smtp_port: 587,
            email_from: None,
            email_password: None,
            email_imap_host: None,
            email_imap_port: 993,
            email_reply_enabled: false,
            telegram_enabled: false,
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_reply_enabled: false,
            discord_enabled: false,
            discord_bot_token: None,
            discord_channel_id: None,
            discord_bot_user_id: None,
            discord_reply_enabled: false,
            jade_relay_enabled: false,
            jade_relay_api_base: None,
            jade_relay_token: None,
            jade_relay_token_id: None,
            jade_relay_user_id: None,
            jade_relay_session_id: None,
            jade_relay_reply_enabled: false,
            jade_relay_launch_enabled: false,
            jade_relay_launch_working_dir: None,
        }
    }
}

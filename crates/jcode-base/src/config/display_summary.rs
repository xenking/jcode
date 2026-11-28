use super::*;

impl Config {
    pub fn display_string(&self) -> String {
        let path = Self::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        [
            format!("**Configuration** (`{path}`)"),
            self.keybindings_summary(),
            self.dictation_summary(),
            self.display.summary(),
            self.features_summary(),
            self.tools_summary(),
            self.provider_summary(),
            self.agent_models_summary(),
            self.gateway_summary(),
            self.ambient_summary(),
            self.notifications_summary(),
            FOOTER.to_string(),
        ]
        .join("\n\n")
    }

    fn keybindings_summary(&self) -> String {
        format!(
            r#"**Keybindings:**
- Scroll up: `{}`
- Scroll down: `{}`
- Scroll up fallback: `{}`
- Scroll down fallback: `{}`
- Page up: `{}`
- Page down: `{}`
- Model next: `{}`
- Model prev: `{}`
- Effort increase: `{}`
- Effort decrease: `{}`
- Speed increase: `{}`
- Speed decrease: `{}`
- Centered toggle: `{}`
- Prompt up: `{}`
- Prompt down: `{}`
- Scroll bookmark: `{}`
- Auto-poke toggle: `{}`
- Workspace left: `{}`
- Workspace down: `{}`
- Workspace up: `{}`
- Workspace right: `{}`"#,
            self.keybindings.scroll_up,
            self.keybindings.scroll_down,
            self.keybindings.scroll_up_fallback,
            self.keybindings.scroll_down_fallback,
            self.keybindings.scroll_page_up,
            self.keybindings.scroll_page_down,
            self.keybindings.model_switch_next,
            self.keybindings.model_switch_prev,
            self.keybindings.effort_increase,
            self.keybindings.effort_decrease,
            self.keybindings.speed_increase,
            self.keybindings.speed_decrease,
            self.keybindings.centered_toggle,
            self.keybindings.scroll_prompt_up,
            self.keybindings.scroll_prompt_down,
            self.keybindings.scroll_bookmark,
            if self.keybindings.auto_poke_toggle.trim().is_empty() {
                "disabled"
            } else {
                self.keybindings.auto_poke_toggle.trim()
            },
            self.keybindings.workspace_left,
            self.keybindings.workspace_down,
            self.keybindings.workspace_up,
            self.keybindings.workspace_right,
        )
    }

    fn dictation_summary(&self) -> String {
        format!(
            r#"**Dictation:**
- Command: `{}`
- Mode: `{}`
- Hotkey: `{}`
- Timeout: {}s"#,
            if self.dictation.command.trim().is_empty() {
                "(disabled)"
            } else {
                self.dictation.command.as_str()
            },
            match self.dictation.mode {
                crate::protocol::TranscriptMode::Insert => "insert",
                crate::protocol::TranscriptMode::Append => "append",
                crate::protocol::TranscriptMode::Replace => "replace",
                crate::protocol::TranscriptMode::Send => "send",
            },
            self.dictation.key,
            self.dictation.timeout_secs,
        )
    }

    fn features_summary(&self) -> String {
        format!(
            r#"**Features:**
- Check updates: {}
- Memory: {}
- Swarm: {}
- Auto-poke: {}
- Message timestamps: {}
- Persist memory injections: {}
- KV cache miss notices: {}
- Update channel: {}"#,
            self.features.check_updates,
            self.features.memory,
            self.features.swarm,
            self.features.auto_poke,
            self.features.message_timestamps,
            self.features.persist_memory_injections,
            self.features.kv_cache_miss_notices,
            self.features.update_channel,
        )
    }

    fn tools_summary(&self) -> String {
        let mut effective_disabled_tools: Vec<String> =
            self.tools.selection().disabled_tools.into_iter().collect();
        effective_disabled_tools.sort();
        format!(
            r#"**Tools:**
- Profile: {}
- Enabled allow-list: {}
- Disabled tools: {}
- Disable base tools: {}
- MCP tools: {}
- MCP auto threshold: {} tokens
- Codemode: {}"#,
            if self.tools.profile.trim().is_empty() {
                "full"
            } else {
                self.tools.profile.trim()
            },
            if self.tools.enabled.is_empty() {
                "(none)".to_string()
            } else {
                self.tools.enabled.join(", ")
            },
            if effective_disabled_tools.is_empty() {
                "(none)".to_string()
            } else {
                effective_disabled_tools.join(", ")
            },
            self.tools.disable_base_tools,
            self.tools.mcp_tools.as_str(),
            self.tools.mcp_tools_token_threshold,
            self.tools.codemode,
        )
    }

    fn provider_summary(&self) -> String {
        format!(
            r#"**Provider:**
- Default model: {}
- Default provider: {}
- OpenAI reasoning effort: {}
- Anthropic reasoning effort: {}
- OpenAI transport: {}
- OpenAI service tier: {}
- OpenAI native compaction: {}
- OpenAI native compaction threshold ratio: {:.2}
- Cross-provider failover: {}"#,
            self.provider
                .default_model
                .as_deref()
                .unwrap_or("(provider default)"),
            self.provider
                .default_provider
                .as_deref()
                .unwrap_or("(auto)"),
            self.provider
                .openai_reasoning_effort
                .as_deref()
                .unwrap_or("(provider default)"),
            self.provider
                .anthropic_reasoning_effort
                .as_deref()
                .unwrap_or("(provider default)"),
            self.provider
                .openai_transport
                .as_deref()
                .unwrap_or("(auto)"),
            self.provider
                .openai_service_tier
                .as_deref()
                .unwrap_or("(default)"),
            self.provider.openai_native_compaction_mode.as_str(),
            self.provider.openai_native_compaction_threshold_tokens,
            self.provider.cross_provider_failover.as_str(),
        )
    }

    fn agent_models_summary(&self) -> String {
        let agents = self.agents.summary();
        format!(
            r#"**Agent models:**
- Swarm / subagent: {}
- Swarm spawn mode: {}
- Swarm root effort: {}
- Spawn hook: {}
- Review: {}
- Judge: {}
- Memory recall: Jev ({})
- Memory extraction sidecar: {}
- Ambient: {}"#,
            agents.swarm_model,
            agents.swarm_spawn_mode,
            agents.swarm_root_effort,
            self.terminal
                .spawn_hook
                .as_deref()
                .unwrap_or("(built-in terminal detection)"),
            self.autoreview
                .model
                .as_deref()
                .unwrap_or("(inherit current session)"),
            self.autojudge
                .model
                .as_deref()
                .unwrap_or("(inherit current session)"),
            agents.memory_jev_provider,
            agents.memory_sidecar,
            self.ambient
                .model
                .as_deref()
                .unwrap_or("(provider default)"),
        )
    }

    fn gateway_summary(&self) -> String {
        format!(
            r#"**Gateway:**
- Enabled: {}
- Bind address: {}:{}"#,
            self.gateway.enabled, self.gateway.bind_addr, self.gateway.port,
        )
    }

    fn ambient_summary(&self) -> String {
        format!(
            r#"**Ambient:**
- Enabled: {}
- Provider: {}
- Model: {}
- Interval: {}-{} minutes
- Pause on active session: {}
- Proactive work: {}
- Work branch prefix: `{}`
- Visible mode: {}"#,
            self.ambient.enabled,
            self.ambient.provider.as_deref().unwrap_or("(auto)"),
            self.ambient
                .model
                .as_deref()
                .unwrap_or("(provider default)"),
            self.ambient.min_interval_minutes,
            self.ambient.max_interval_minutes,
            self.ambient.pause_on_active_session,
            self.ambient.proactive_work,
            self.ambient.work_branch_prefix,
            self.ambient.visible,
        )
    }

    fn notifications_summary(&self) -> String {
        format!(
            r#"**Notifications:**
- ntfy.sh: {}
- Desktop: {}
- Email: {}
- Email replies: {}
- Telegram: {}
- Telegram replies: {}
- Discord: {}
- Discord replies: {}"#,
            self.safety
                .ntfy_topic
                .as_deref()
                .map(|t| format!("enabled (topic: {})", t))
                .unwrap_or_else(|| "disabled".to_string()),
            if self.safety.desktop_notifications {
                "enabled"
            } else {
                "disabled"
            },
            if self.safety.email_enabled {
                self.safety
                    .email_to
                    .as_deref()
                    .unwrap_or("enabled (no recipient)")
            } else {
                "disabled"
            },
            if self.safety.email_reply_enabled {
                self.safety
                    .email_imap_host
                    .as_deref()
                    .unwrap_or("enabled (no IMAP host)")
            } else {
                "disabled"
            },
            if self.safety.telegram_enabled {
                self.safety
                    .telegram_chat_id
                    .as_deref()
                    .unwrap_or("enabled (no chat_id)")
            } else {
                "disabled"
            },
            if self.safety.telegram_reply_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if self.safety.discord_enabled {
                self.safety
                    .discord_channel_id
                    .as_deref()
                    .unwrap_or("enabled (no channel_id)")
            } else {
                "disabled"
            },
            if self.safety.discord_reply_enabled {
                "enabled"
            } else {
                "disabled"
            },
        )
    }
}

const FOOTER: &str = r#"*Edit the config file or set environment variables to customize.*
*Environment variables (e.g., `JCODE_SCROLL_UP_KEY`, `JCODE_GATEWAY_ENABLED`) override file settings.*"#;

impl Config {
    /// Keyboards the companion watches for global push-to-talk.
    ///
    /// `[dictation] global_devices` wins. When it is empty, the keyboards
    /// already chosen in Jcode Desktop (`[desktop.voice] global_devices`) are
    /// used, so push-to-talk keeps working while Desktop is closed.
    pub fn global_voice_devices(&self) -> Vec<std::path::PathBuf> {
        if !self.dictation.global_devices.is_empty() {
            return self.dictation.global_devices.clone();
        }
        self.desktop
            .as_ref()
            .and_then(|desktop| desktop.get("voice"))
            .and_then(|voice| voice.get("global_devices"))
            .and_then(|devices| devices.as_array())
            // Missing legacy desktop devices means there are no configured
            // devices, not a configuration read or parse failure.
            .into_iter()
            .flatten()
            .filter_map(|device| device.as_str())
            .filter(|device| !device.trim().is_empty())
            .map(std::path::PathBuf::from)
            .collect()
    }
}

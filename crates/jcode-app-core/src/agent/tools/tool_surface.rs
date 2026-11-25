//! The tool surface an agent presents to the model: which definitions are
//! visible, how MCP and skill changes are announced, and tool dispatch.

use super::*;

/// Cap on tools listed in one late-MCP transcript announcement.
const MAX_ANNOUNCED_MCP_TOOLS: usize = 64;
/// Per-tool description budget in the late MCP announcement.
const MAX_ANNOUNCED_DESCRIPTION_CHARS: usize = 120;

/// Stable header of the late-MCP transcript announcement; used to recognize
/// earlier announcements when a session is restored.
pub(super) const LATE_MCP_ANNOUNCEMENT_MARKER: &str = "New MCP tools are available.";

/// Stable header of the late-skill transcript announcement; used to recognize
/// earlier announcements when a session is restored.
pub(super) const LATE_SKILL_ANNOUNCEMENT_MARKER: &str = "New skills were installed.";

impl Agent {
    pub fn add_manual_tool_error(
        &mut self,
        tool_call_id: String,
        error: String,
        duration_ms: u64,
    ) -> Result<()> {
        self.add_message_with_duration(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id,
                content: error,
                is_error: Some(true),
            }],
            Some(duration_ms),
        );
        self.session.save()?;
        Ok(())
    }

    pub fn add_manual_tool_result(
        &mut self,
        tool_call_id: String,
        output: crate::tool::ToolOutput,
        duration_ms: u64,
    ) -> Result<()> {
        let blocks = tool_output_to_content_blocks(tool_call_id, output);
        self.add_message_with_duration(Role::User, blocks, Some(duration_ms));
        self.session.save()?;
        Ok(())
    }

    pub fn add_manual_tool_use(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        input: serde_json::Value,
    ) -> Result<String> {
        let message_id = self.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: tool_call_id,
                name: tool_name,
                input,
                thought_signature: None,
            }],
        );
        self.session.save()?;
        Ok(message_id)
    }

    /// Announce MCP tools that became available after the tool list locked,
    /// by appending a note to the transcript before the next user message.
    ///
    /// Only used on the fixed `mcp` search/call surface (providers
    /// without native deferred loading, in any mode but `eager`). The cached
    /// tool list must never change, so this appended note, which carries
    /// each tool's input schema, is how the model learns about servers that
    /// finished connecting mid-session. Each tool is announced once.
    pub(crate) async fn announce_late_mcp_tools(&mut self) {
        if self.mcp_tools_mode == crate::config::McpToolsMode::Eager || self.native_deferred_mcp() {
            return;
        }
        self.seed_announced_mcp_tools_from_transcript();
        let current = self.build_filtered_tool_definitions_raw().await;
        let fresh: Vec<ToolDefinition> = current
            .into_iter()
            .filter(|tool| {
                tool.name.starts_with("mcp__") && !self.announced_mcp_tools.contains(&tool.name)
            })
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut text = format!(
            "<system-reminder>\n{LATE_MCP_ANNOUNCEMENT_MARKER} They are not in your tool list; \
             first get the tool schema with mcp action=search (query: tool name), then call with mcp action=call (server, tool, arguments).\n",
        );
        for tool in fresh.iter().take(MAX_ANNOUNCED_MCP_TOOLS) {
            let (server, raw) = self
                .registry
                .mcp_identity_for_alias(&tool.name)
                .unwrap_or_else(|| {
                    let (server, raw) = split_mcp_dispatch_name(&tool.name);
                    (server.to_string(), raw.to_string())
                });
            // Names and short descriptions only: schemas stay out of the
            // transcript and are fetched on demand through `mcp search`.
            let description = tool.description.trim();
            let description = description.lines().next().unwrap_or_default();
            let description = if description.chars().count() > MAX_ANNOUNCED_DESCRIPTION_CHARS {
                let cut: String = description
                    .chars()
                    .take(MAX_ANNOUNCED_DESCRIPTION_CHARS)
                    .collect();
                format!("{}...", cut.trim_end())
            } else {
                description.to_string()
            };
            text.push_str(&format!(
                "- {}: server: {server}  tool: {raw}  ({description})\n",
                tool.name,
            ));
        }
        if fresh.len() > MAX_ANNOUNCED_MCP_TOOLS {
            text.push_str(&format!(
                "... and {} more; use mcp action=search to find them.\n",
                fresh.len() - MAX_ANNOUNCED_MCP_TOOLS
            ));
        }
        text.push_str("</system-reminder>");
        for tool in &fresh {
            self.announced_mcp_tools.insert(tool.name.clone());
        }
        logging::info(&format!(
            "Announcing {} late MCP tool(s) in the transcript (tool list stays cache-stable)",
            fresh.len()
        ));
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(crate::session::StoredDisplayRole::System),
        );
    }

    /// Announce skills installed after the session's system prompt was built.
    ///
    /// The "Available Skills" section is frozen per session (see
    /// `prompt_skills_snapshot`) because rewriting the system prompt forces a
    /// KV cache miss on the whole conversation. New skills are instead
    /// described once, in a note appended before the next user message.
    pub(crate) fn announce_late_skills(&mut self) {
        if self.session.system_prompt().is_some() {
            return;
        }
        self.seed_announced_skills_from_transcript();
        let fresh: Vec<crate::prompt::SkillInfo> = self
            .current_prompt_skill_infos()
            .into_iter()
            .filter(|skill| !self.announced_skills.contains(&skill.name))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut text = format!("<system-reminder>\n{LATE_SKILL_ANNOUNCEMENT_MARKER} ");
        text.push_str(
            "They are not in the Available Skills list above but can be invoked with `/skillname` \
             the same way:\n",
        );
        for skill in &fresh {
            let description = skill
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            text.push_str(&format!("- `/{} ` - {}\n", skill.name, description));
        }
        text.push_str("</system-reminder>");
        for skill in &fresh {
            self.announced_skills.insert(skill.name.clone());
        }
        logging::info(&format!(
            "Announcing {} late skill(s) in the transcript (system prompt stays cache-stable)",
            fresh.len()
        ));
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(crate::session::StoredDisplayRole::System),
        );
        self.announced_skills_scan_index = self.session.message_count();
    }

    /// Replace per-server MCP definitions with the fixed search/call surface
    /// according to the configured mode. Auto mode estimates the actual
    /// serialized, already-filtered definitions the provider would receive.
    pub(super) fn apply_mcp_tool_exposure(&self, tools: &mut Vec<ToolDefinition>) {
        if self.native_deferred_mcp() {
            // Keep the unified MCP tool eager for discovery and management.
            // Search results load matching server definitions via references.
            for tool in tools.iter_mut() {
                if tool.name.starts_with("mcp__") {
                    tool.defer_loading = true;
                }
            }
            return;
        }
        // Only explicit `eager` puts per-server definitions in the cached
        // prefix. `auto` used to switch on a token threshold, but any MCP
        // change (late connect, `mcp connect`, crossing the threshold) then
        // rewrote the tool list and busted the whole prompt cache. On
        // providers without native deferred loading, `auto` therefore always
        // uses the fixed `mcp` search/call surface; the definitions
        // reach the model through the transcript instead.
        let deferred = !matches!(self.mcp_tools_mode, crate::config::McpToolsMode::Eager);

        if deferred {
            tools.retain(|tool| !tool.name.starts_with("mcp__"));
        }
    }

    /// Expose the `selfdev` tool only while running in self-development mode.
    /// Self-dev agents use the working tree rather than bundled `jcode_docs`,
    /// which can lag behind the source they are editing.
    ///
    /// The registry keeps the implementation available for self-dev sessions,
    /// but regular agents should not spend tool-list context on an internal
    /// development surface.
    pub(super) fn apply_selfdev_tool_surface(
        tools: &mut Vec<ToolDefinition>,
        is_canary: bool,
        is_desktop: bool,
    ) {
        // Desktop development is a separate product mode, not a CLI canary.
        // Never advertise CLI build/reload or TUI debug sockets in that mode.
        if is_desktop {
            tools.retain(|tool| {
                !matches!(
                    tool.name.as_str(),
                    "selfdev" | "debug_socket" | "jcode_docs"
                )
            });
            return;
        }
        tools.retain(|tool| tool.name != "desktop_selfdev");
        if !is_canary {
            tools.retain(|tool| tool.name != "selfdev");
            return;
        }
        tools.retain(|tool| tool.name != "jcode_docs");
        for tool in tools.iter_mut() {
            if tool.name == "selfdev" {
                tool.description =
                    crate::tool::selfdev::SelfDevTool::description_for(true).to_string();
                tool.input_schema = crate::tool::selfdev::SelfDevTool::schema_for(true);
            }
        }
    }

    /// Build the agent's tool definitions from the registry, applying the
    /// session's `allowed_tools`, `disabled_tools`, and self-dev filters.
    pub(super) async fn build_filtered_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.build_filtered_tool_definitions_with(true).await
    }

    /// Filtered definitions before MCP exposure: includes per-server
    /// `mcp__*` definitions (with policy filters applied) even when the
    /// provider sees only the fixed search/call surface.
    pub(super) async fn build_filtered_tool_definitions_raw(&self) -> Vec<ToolDefinition> {
        self.build_filtered_tool_definitions_with(false).await
    }

    pub(super) async fn build_filtered_tool_definitions_with(
        &self,
        apply_mcp_exposure: bool,
    ) -> Vec<ToolDefinition> {
        let sdk = crate::tool::sdk::config(self.session.id());
        let enabled = sdk
            .as_ref()
            .and_then(|c| c.enabled.as_ref())
            .map(|names| names.iter().cloned().collect());
        let allowed = enabled.as_ref().or(self.allowed_tools.as_ref());
        let mut tools = self.registry.definitions(allowed).await;
        if sdk
            .as_ref()
            .is_some_and(|config| self.is_jarvis_root(config))
            && let Some(swarm) = tools.iter_mut().find(|tool| tool.name == "swarm")
        {
            // Replace only the stock policy. Preserve a user's swarm-prompt.md
            // and let the SDK overlay below retain any custom tool definition.
            swarm.description = swarm.description.replace(
                crate::prompt::DEFAULT_SWARM_PROMPT.trim(),
                "Follow Jarvis root coordination. Delegate substantive execution, including single-lane work. Worker-owned todo lanes launch automatically, so do not spawn another worker for the same lane.",
            );
        }
        if enabled.is_none() && !self.disabled_tools.is_empty() {
            tools.retain(|tool| {
                !self
                    .registry
                    .tool_is_disabled(&self.disabled_tools, &tool.name)
            });
        }
        Self::apply_selfdev_tool_surface(
            &mut tools,
            self.session.is_canary(),
            self.is_desktop_selfdev(),
        );
        if apply_mcp_exposure {
            self.apply_mcp_tool_exposure(&mut tools);
        }
        let mut tools = crate::tool::sdk::apply_definitions(self.session.id(), tools);
        if let Some(config) = sdk.as_ref() {
            let disabled = config.disabled.iter().cloned().collect();
            tools.retain(|tool| !self.registry.tool_is_disabled(&disabled, &tool.name));
        }
        // Rarely used built-ins ride the same native deferred path as MCP
        // tools, so the snapshot rebuild on a provider change covers them too.
        // An SDK tool that reuses a built-in name is the caller's own and stays.
        let loader = crate::tool::load_tools::LOAD_TOOLS_NAME;
        let sdk_disabled: HashSet<String> = sdk
            .iter()
            .flat_map(|config| config.disabled.iter().cloned())
            .collect();
        let loader_allowed = allowed.is_none_or(|set| self.registry.tool_is_allowed(set, loader))
            && !self.registry.tool_is_disabled(&self.disabled_tools, loader)
            && !self.registry.tool_is_disabled(&sdk_disabled, loader);
        crate::tool::load_tools::apply_deferral(
            &mut tools,
            self.native_deferred_mcp(),
            loader_allowed,
            |name| crate::tool::sdk::custom(self.session.id(), name),
        );
        tools
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<crate::tool::ToolOutput> {
        self.validate_tool_allowed(name)?;

        let call_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| format!("debug-{}", d.as_millis()))
            .unwrap_or_else(|_| "debug".to_string());
        let ctx = ToolContext {
            session_id: self.session.id().to_string(),
            message_id: self.session.id().to_string(),
            tool_call_id: call_id,
            working_dir: self.working_dir().map(PathBuf::from),
            stdin_request_tx: self.stdin_request_tx.clone(),
            graceful_shutdown_signal: Some(self.graceful_shutdown.clone()),
            execution_mode: ToolExecutionMode::Direct,
        };
        self.registry.execute(name, input, ctx).await
    }

    pub(crate) fn invalidate_sdk_tools(&mut self) {
        self.mcp_late_register_resolved = false;
        self.locked_tools = None;
        self.cache_tracker.reset();
        self.kv_cache_monitor.reset();
    }

    /// Whether this session exposes MCP tools through provider-native deferred
    /// loading (see [`ToolDefinition::defer_loading`]). Eager mode keeps
    /// top-level definitions; every other mode prefers the native path when
    /// the active provider supports it, and falls back to the fixed
    /// `mcp` search/call surface otherwise.
    pub(crate) fn native_deferred_mcp(&self) -> bool {
        self.mcp_tools_mode != crate::config::McpToolsMode::Eager
            && self.provider.supports_deferred_tools()
    }

    /// Tool list for the native deferred MCP path.
    ///
    /// The eager part is locked exactly like the regular path. MCP
    /// definitions are appended as deferred on every turn from the live
    /// registry. They never enter the cached prefix, so adding, removing, or
    /// reconnecting servers costs no prompt-cache miss.
    pub(super) async fn native_deferred_tool_definitions(&mut self) -> Vec<ToolDefinition> {
        let current = self.build_filtered_tool_definitions().await;
        let eager = match &self.locked_tools {
            // A snapshot locked before switching to the native path may still
            // carry eager MCP definitions; keep them eager for cache stability
            // (they are still callable) but never duplicate them as deferred.
            Some(locked) => locked.clone(),
            None => {
                let eager = ToolDefinition::eager(&current);
                logging::info(&format!(
                    "Locking eager tool list at {} tools for cache stability (MCP tools deferred)",
                    eager.len()
                ));
                self.locked_tools = Some(eager.clone());
                self.locked_tools_native_deferred = true;
                eager
            }
        };
        self.mcp_late_register_resolved = true;
        let mut tools = eager;
        let eager_names: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
        tools.extend(
            current
                .into_iter()
                .filter(|t| t.defer_loading && !eager_names.contains(&t.name)),
        );
        tools
    }

    /// Returns true if the registry contains `mcp__*` tools (subject to the
    /// session's `allowed_tools` filter) that are not present in the currently
    /// locked snapshot. Used to detect the async MCP-registration race (#206).
    pub(super) async fn registry_has_new_mcp_tools(&self, locked: &[ToolDefinition]) -> bool {
        let registry_names = self.registry.tool_names().await;
        let allowed = self.allowed_tools.as_ref();
        registry_names.iter().any(|name| {
            name.starts_with("mcp__")
                && allowed
                    .map(|set| self.registry.tool_is_allowed(set, name))
                    .unwrap_or(true)
                && !self.registry.tool_is_disabled(&self.disabled_tools, name)
                && !locked.iter().any(|t| &t.name == name)
        })
    }

    /// Mark MCP tools the transcript already describes as announced: tools
    /// loaded by a tool reference (`mcp connect` / `mcp search` results carry
    /// their schemas) and tools named in earlier announcements (which matters
    /// after a session restore, when the in-memory set starts empty).
    pub(super) fn seed_announced_mcp_tools_from_transcript(&mut self) {
        let start = self
            .announced_mcp_scan_index
            .min(self.session.message_count());
        for message in self.session.iter_messages_from(start) {
            for block in &message.content {
                match block {
                    ContentBlock::ToolReference { tool_name, .. } => {
                        self.announced_mcp_tools.insert(tool_name.clone());
                    }
                    ContentBlock::Text { text, .. }
                        if text.contains(LATE_MCP_ANNOUNCEMENT_MARKER) =>
                    {
                        for line in text.lines() {
                            if let Some(name) = line
                                .strip_prefix("- ")
                                .and_then(|rest| rest.split(':').next())
                                .filter(|name| name.starts_with("mcp__"))
                            {
                                self.announced_mcp_tools.insert(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.announced_mcp_scan_index = self.session.message_count();
    }

    /// Mark skills named in earlier announcements as announced, so a
    /// restored session does not describe them again.
    pub(super) fn seed_announced_skills_from_transcript(&mut self) {
        let start = self
            .announced_skills_scan_index
            .min(self.session.message_count());
        for message in self.session.iter_messages_from(start) {
            for block in &message.content {
                if let ContentBlock::Text { text, .. } = block
                    && text.contains(LATE_SKILL_ANNOUNCEMENT_MARKER)
                {
                    for line in text.lines() {
                        if let Some(name) = line
                            .strip_prefix("- `/")
                            .and_then(|rest| rest.split(' ').next())
                            .filter(|name| !name.is_empty())
                        {
                            self.announced_skills.insert(name.to_string());
                        }
                    }
                }
            }
        }
        self.announced_skills_scan_index = self.session.message_count();
    }

    pub(super) async fn tool_definitions(&mut self) -> Vec<ToolDefinition> {
        if self.session.is_canary() {
            self.registry.register_selfdev_tools().await;
        }

        // Provider-native deferred MCP loading: MCP definitions live outside
        // the cached prefix, so the eager snapshot never changes when servers
        // connect, reconnect, or register late. Refresh the deferred subset
        // every turn instead of unlocking.
        let native = self.native_deferred_mcp();
        if self.locked_tools.is_some() && self.locked_tools_native_deferred != native {
            // The active provider's deferred-loading capability changed (model
            // or provider switch). The prefix changes with the provider anyway,
            // so rebuilding costs nothing extra and keeps MCP usable.
            logging::info(&format!(
                "Rebuilding tool snapshot: native deferred MCP loading {} after provider change",
                if native { "enabled" } else { "disabled" }
            ));
            self.locked_tools = None;
            self.mcp_late_register_resolved = false;
            self.cache_tracker.reset();
            self.kv_cache_monitor.reset();
        }
        if native {
            return self.native_deferred_tool_definitions().await;
        }

        // Return locked tools if available (prevents cache invalidation from
        // tools arriving asynchronously after the first API request).
        //
        // Exception: MCP servers connect on a background task and register
        // `mcp__*` tools seconds after the session starts — typically *after*
        // the first turn has already locked the snapshot. We deliberately do
        // NOT block the first turn on MCP connection: servers can be slow or
        // hang, and we want the user to be able to talk to the agent the moment
        // the session spawns. The price is that the first locked snapshot is
        // missing MCP tools, and the only other unlock path fires when the model
        // calls the `mcp` management tool — which it cannot do without first
        // seeing MCP tools (#206).
        //
        // So, exactly once per locked snapshot, if MCP tools have since appeared
        // in the registry, we rebuild. This is a single intentional provider
        // prompt-cache miss (the turn MCP tools first appear). The
        // `mcp_late_register_resolved` flag makes this a one-shot check so we do
        // not rescan the registry on every subsequent turn.
        // Every mode except explicit `eager` keeps a cache-stable MCP surface:
        // per-server tools registering later never change the locked list.
        let locked_uses_fixed_mcp_surface =
            self.mcp_tools_mode != crate::config::McpToolsMode::Eager;
        if locked_uses_fixed_mcp_surface && let Some(locked) = self.locked_tools.clone() {
            // Per-server tools may continue registering in the background, but
            // the fixed surface cannot change as a result. Avoid a provider
            // cache reset and registry scan; new tools are announced in the
            // transcript instead (see `announce_late_mcp_tools`).
            self.mcp_late_register_resolved = true;
            return locked;
        }
        if let Some(ref locked) = self.locked_tools {
            if self.mcp_late_register_resolved {
                return locked.clone();
            }
            if self.registry_has_new_mcp_tools(locked).await {
                logging::info(
                    "MCP tools registered after first turn locked the tool snapshot — \
                     rebuilding once to expose them. This is one intentional prompt-cache \
                     miss; we accept it so the agent is reachable immediately at spawn \
                     instead of blocking on MCP connection (#206).",
                );
                // Latch the one-shot guard and drop the stale snapshot directly.
                // We intentionally do NOT call `unlock_tools()` here, because that
                // re-arms the guard (it is the explicit-reload path) and would let
                // the recheck fire again on every later turn.
                self.mcp_late_register_resolved = true;
                self.locked_tools = None;
                self.cache_tracker.reset();
                self.kv_cache_monitor.reset();
            } else {
                // No MCP tools have appeared. They may still be connecting, so
                // leave the guard unset and re-check on the next turn. Once they
                // appear (or never do, after the registry settles) we stop.
                return locked.clone();
            }
        }

        let tools = self.build_filtered_tool_definitions().await;

        // Lock the tool list to prevent cache invalidation when more tools
        // arrive asynchronously mid-session.
        logging::info(&format!(
            "Locking tool list at {} tools for cache stability",
            tools.len()
        ));
        self.locked_tools = Some(tools.clone());
        self.locked_tools_native_deferred = false;
        // Per-server definitions in the snapshot itself (eager mode) are
        // already described to the model; never announce them again.
        self.announced_mcp_tools.extend(
            tools
                .iter()
                .filter(|tool| tool.name.starts_with("mcp__"))
                .map(|tool| tool.name.clone()),
        );
        tools
    }

    /// Get full tool definitions for debug introspection (bypasses lock)
    pub async fn tool_definitions_for_debug(&self) -> Vec<crate::message::ToolDefinition> {
        if self.session.is_canary() {
            self.registry.register_selfdev_tools().await;
        }
        self.build_filtered_tool_definitions().await
    }

    pub async fn tool_names(&self) -> Vec<String> {
        self.tool_definitions_for_debug()
            .await
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    /// Unlock the tool list so the next API request picks up any new tools.
    /// Called after MCP reload or when the user explicitly wants new tools.
    pub fn unlock_tools(&mut self) {
        if self.locked_tools.is_some() {
            logging::info("Tool list unlocked — next request will pick up current tools");
            self.locked_tools = None;
            self.cache_tracker.reset();
            self.kv_cache_monitor.reset();
        }
        // Allow the late-MCP-registration recheck to fire once for the next
        // snapshot (e.g. after an explicit `mcp` reload).
        self.mcp_late_register_resolved = false;
    }

    /// Unlock tools if a tool execution may have changed the registry
    /// (e.g., mcp connect/disconnect/reload)
    pub(super) fn unlock_tools_if_needed(&mut self, tool_name: &str) {
        // Only explicit `eager` exposes per-server definitions in the cached
        // tool list. Every other mode keeps that list fixed (native deferred
        // loading or the `mcp` search/call surface), so an `mcp`
        // connect/disconnect must not unlock it: that would be a needless
        // full prompt-cache miss.
        if tool_name == "mcp" && self.mcp_tools_mode == crate::config::McpToolsMode::Eager {
            self.unlock_tools();
        }
    }

    pub(super) fn validate_tool_allowed(&self, name: &str) -> Result<()> {
        let unqualified_name = name.strip_prefix("functions.").unwrap_or(name);
        let name = if crate::tool::sdk::custom(self.session.id(), unqualified_name) {
            unqualified_name
        } else {
            Registry::resolve_tool_name(unqualified_name)
        };
        let mut sdk_enabled = false;
        if let Some(config) = crate::tool::sdk::config(self.session.id()) {
            let disabled = config.disabled.into_iter().collect();
            anyhow::ensure!(
                !self.registry.tool_is_disabled(&disabled, name),
                "Tool '{}' is disabled",
                name
            );
            if config.custom.iter().any(|t| t.name == name) {
                return Ok(());
            }
            if let Some(enabled) = config.enabled {
                let allowed = enabled.into_iter().collect();
                anyhow::ensure!(
                    self.registry.tool_is_allowed(&allowed, name),
                    "Tool '{}' is not allowed",
                    name
                );
                sdk_enabled = true;
            }
        }
        let is_desktop = self.is_desktop_selfdev();
        if is_desktop && matches!(name, "selfdev" | "debug_socket") {
            return Err(anyhow::anyhow!(
                "Tool '{}' targets Jcode CLI, not Desktop. Use 'desktop_selfdev' in Desktop self-development mode.",
                name
            ));
        }
        if !is_desktop && name == "desktop_selfdev" {
            return Err(anyhow::anyhow!(
                "Tool 'desktop_selfdev' is only available in a Jcode Desktop source checkout."
            ));
        }
        if (self.session.is_canary() || is_desktop) && name == "jcode_docs" {
            return Err(anyhow::anyhow!(
                "Tool 'jcode_docs' is disabled in self-development mode. Read the working tree documentation instead."
            ));
        }
        if sdk_enabled {
            return Ok(());
        }
        if let Some(allowed) = self.allowed_tools.as_ref()
            && !self.registry.tool_is_allowed(allowed, name)
        {
            return Err(anyhow::anyhow!("Tool '{}' is not allowed", name));
        }
        if self.registry.tool_is_disabled(&self.disabled_tools, name) {
            return Err(anyhow::anyhow!("Tool '{}' is disabled", name));
        }
        Ok(())
    }

    /// Per-server MCP tool names the session can see, for the client header's
    /// `mcp:` inventory.
    ///
    /// Equivalent to filtering [`Self::tool_names`] for `mcp__*`, but reads only
    /// registry keys: building full definitions (every schema, sorted, SDK
    /// overlay applied) just to count names took up to ~40ms on the bootstrap
    /// History path that gates the client's first settled frame.
    pub async fn visible_mcp_tool_names(&self) -> Vec<String> {
        // Mirrors `apply_mcp_tool_exposure`: on the unified `mcp` surface
        // per-server definitions are not exposed.
        if !self.native_deferred_mcp()
            && !matches!(self.mcp_tools_mode, crate::config::McpToolsMode::Eager)
        {
            return Vec::new();
        }
        let sdk = crate::tool::sdk::config(self.session.id());
        let enabled: Option<HashSet<String>> = sdk
            .as_ref()
            .and_then(|c| c.enabled.as_ref())
            .map(|names| names.iter().cloned().collect());
        let allowed = enabled.as_ref().or(self.allowed_tools.as_ref());
        let sdk_disabled: HashSet<String> = sdk
            .as_ref()
            .map(|c| c.disabled.iter().cloned().collect())
            .unwrap_or_default();
        let mut names: Vec<String> = self
            .registry
            .tool_names()
            .await
            .into_iter()
            .filter(|name| name.starts_with("mcp__"))
            .filter(|name| allowed.is_none_or(|set| self.registry.tool_is_allowed(set, name)))
            .filter(|name| {
                enabled.is_some()
                    || self.disabled_tools.is_empty()
                    || !self.registry.tool_is_disabled(&self.disabled_tools, name)
            })
            .filter(|name| {
                sdk_disabled.is_empty() || !self.registry.tool_is_disabled(&sdk_disabled, name)
            })
            .collect();
        names.sort();
        names
    }
}

/// Split a registry key `mcp__<server>__<tool>` into (server, tool).
/// Server names cannot contain `__`, so the first separator is the split.
pub(super) fn split_mcp_dispatch_name(name: &str) -> (&str, &str) {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .unwrap_or(("", name))
}

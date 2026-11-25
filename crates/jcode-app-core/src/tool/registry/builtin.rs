//! Composition root of the tool registry: which tools exist, how they are
//! constructed, and how MCP, self-dev and ambient tools are registered.
//!
//! `Registry` itself only stores and runs tools. Concrete tools depend on it,
//! never the other way round, so everything that names a concrete tool lives here.

use super::*;

impl Registry {
    pub(super) fn shared_skills_registry() -> Arc<RwLock<SkillRegistry>> {
        SkillRegistry::shared_registry()
    }

    pub(super) fn insert_tool<T>(tools: &mut HashMap<String, Arc<dyn Tool>>, name: &str, tool: T)
    where
        T: Tool + 'static,
    {
        tools.insert(name.into(), Arc::new(tool) as Arc<dyn Tool>);
    }

    pub(super) fn insert_tool_timed<T>(
        tools: &mut HashMap<String, Arc<dyn Tool>>,
        timings: &mut Vec<(String, u128)>,
        name: &str,
        make_tool: impl FnOnce() -> T,
    ) where
        T: Tool + 'static,
    {
        let start = std::time::Instant::now();
        Self::insert_tool(tools, name, make_tool());
        timings.push((name.to_string(), start.elapsed().as_millis()));
    }

    /// Base tools that are stateless and can be shared across sessions.
    /// Created once and cached in a OnceLock, then cloned (cheap Arc bumps) per session.
    pub(super) fn base_tools(
        skills: &Arc<RwLock<SkillRegistry>>,
    ) -> HashMap<String, Arc<dyn Tool>> {
        use std::sync::OnceLock;
        static BASE: OnceLock<HashMap<String, Arc<dyn Tool>>> = OnceLock::new();
        let base = BASE.get_or_init(|| {
            let init_start = std::time::Instant::now();
            let mut timings = Vec::new();
            let mut m = HashMap::new();
            Self::insert_tool_timed(&mut m, &mut timings, "read", read::ReadTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "write", write::WriteTool::new);
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "agentgrep",
                agentgrep::AgentGrepTool::new,
            );
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "side_panel",
                side_panel::SidePanelTool::new,
            );
            Self::insert_tool_timed(&mut m, &mut timings, "panel", panel::PanelTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "applet", applet::AppletTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "user_response", || {
                user_response::UserResponseTool
            });
            Self::insert_tool_timed(&mut m, &mut timings, "edit", edit::EditTool::new);
            // `multiedit` merged into `edit`, and `patch` into `apply_patch`.
            // Both old names still resolve through `resolve_tool_name`.
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "apply_patch",
                apply_patch::ApplyPatchTool::new,
            );
            Self::insert_tool_timed(&mut m, &mut timings, "bash", bash::BashTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "browser", browser::BrowserTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "open", open::OpenTool::new);
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                load_tools::LOAD_TOOLS_NAME,
                load_tools::LoadToolsTool::new,
            );
            #[cfg(target_os = "macos")]
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "macos_computer_use",
                computer::ComputerTool::new,
            );
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "webfetch",
                webfetch::WebFetchTool::new,
            );
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "websearch",
                websearch::WebSearchTool::new,
            );
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "maintainer_feedback",
                feedback::MaintainerFeedbackTool::new,
            );
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "jcode_docs",
                jcode_docs::JcodeDocsTool::new,
            );
            Self::insert_tool_timed(&mut m, &mut timings, "todo", todo::TodoTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "ask", || ask::AskTool);
            Self::insert_tool_timed(&mut m, &mut timings, "bg", bg::BgTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, rollout_wait::NAME, || {
                rollout_wait::RolloutWait
            });
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "session_search",
                session_search::SessionSearchTool::new,
            );
            Self::insert_tool_timed(&mut m, &mut timings, "memory", memory::MemoryTool::new);
            // Initiative is temporarily unavailable. Keep its implementation and
            // saved data intact so it can be restored without a migration.
            Self::insert_tool_timed(&mut m, &mut timings, "gmail", gmail::GmailTool::new);
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "calendar",
                calendar::CalendarTool::new,
            );
            Self::insert_tool_timed(&mut m, &mut timings, "schedule", crate::ambient_tools::ScheduleTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "desktop", desktop::DesktopTool::new);
            Self::insert_tool_timed(&mut m, &mut timings, "selfdev", selfdev::SelfDevTool::new);
            Self::insert_tool_timed(
                &mut m,
                &mut timings,
                "desktop_selfdev",
                desktop_selfdev::DesktopSelfDevTool::new,
            );
            let nonzero: Vec<String> = timings
                .iter()
                .filter(|(_, ms)| *ms > 0)
                .map(|(name, ms)| format!("{name}={ms}ms"))
                .collect();
            crate::logging::info(&format!(
                "[TIMING] registry_base_tools_init: total={}ms, nonzero=[{}]",
                init_start.elapsed().as_millis(),
                nonzero.join(", ")
            ));
            m
        });
        // Clone the Arc entries (cheap refcount bumps, not deep copies)
        let mut tools = base.clone();
        // SkillTool needs the skills registry reference (shared across sessions)
        Self::insert_tool(
            &mut tools,
            "skill_manage",
            skill::SkillTool::new(skills.clone()),
        );
        // The swarm tool captures the user-editable swarm prompt in its
        // description. Construct it once per session rather than sharing the
        // process-wide instance. Existing sessions keep their stable tool
        // definition (and provider KV cache), while newly created agents see
        // prompt edits immediately.
        Self::insert_tool(&mut tools, "swarm", communicate::CommunicateTool::new());
        tools
    }

    pub async fn new(provider: Arc<dyn Provider>) -> Self {
        let start = std::time::Instant::now();
        let skills_start = std::time::Instant::now();
        let skills = Self::shared_skills_registry();
        let skills_ms = skills_start.elapsed().as_millis();
        let compaction_start = std::time::Instant::now();
        let compaction = Arc::new(RwLock::new(CompactionManager::new()));
        let compaction_ms = compaction_start.elapsed().as_millis();
        let registry_struct_start = std::time::Instant::now();
        let registry = Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
            mcp_policy: Arc::default(),
            skills: skills.clone(),
            compaction: compaction.clone(),
        };
        let registry_struct_ms = registry_struct_start.elapsed().as_millis();

        let base_start = std::time::Instant::now();
        let mut tools_map = Self::base_tools(&skills);
        let base_ms = base_start.elapsed().as_millis();

        // Per-session tools that need provider/registry references
        let session_tools_start = std::time::Instant::now();
        Self::insert_tool(
            &mut tools_map,
            "batch",
            batch::BatchTool::new(registry.downgrade()),
        );
        Self::insert_tool(
            &mut tools_map,
            "conversation_search",
            conversation_search::ConversationSearchTool::new(compaction),
        );
        // Codemode is opt-in (`[tools] codemode = true`). When off, the tool
        // is never registered, so it costs nothing and never reaches a model.
        if codemode::enabled() {
            Self::insert_tool(
                &mut tools_map,
                "codemode",
                codemode::CodemodeTool::new(registry.downgrade()),
            );
        }
        // The advisor is opt-in (`[agents.advisor] model`). Unconfigured, it
        // is never registered and costs nothing.
        if crate::advisor::AdvisorSettings::from_config().is_some() {
            Self::insert_tool(
                &mut tools_map,
                "advisor",
                advisor::AdvisorTool::new(Arc::clone(&provider)),
            );
        }
        let session_tools_ms = session_tools_start.elapsed().as_millis();

        let write_start = std::time::Instant::now();
        *registry.tools.write().await = tools_map;
        let write_ms = write_start.elapsed().as_millis();
        crate::logging::info(&format!(
            "[TIMING] registry_new: skills={}ms, compaction={}ms, registry_struct={}ms, base_tools={}ms, session_tools={}ms, write={}ms, total={}ms",
            skills_ms,
            compaction_ms,
            registry_struct_ms,
            base_ms,
            session_tools_ms,
            write_ms,
            start.elapsed().as_millis()
        ));
        registry
    }

    /// Enable test mode for memory tools (isolated storage)
    /// Called when session is marked as debug
    pub async fn enable_memory_test_mode(&self) {
        let mut tools = self.tools.write().await;

        // Replace memory tool with test version
        tools.insert(
            "memory".to_string(),
            Arc::new(memory::MemoryTool::new_test()) as Arc<dyn Tool>,
        );

        crate::logging::info("Memory test mode enabled - using isolated storage");
    }

    /// Like [`Self::register_mcp_tools`], but resolves project-local MCP config
    /// (`.mcp.json`, `.jcode/mcp.json`, `.claude/mcp.json`) against
    /// `working_dir` instead of the server process cwd. Remote/client sessions
    /// must pass their session working directory here (issue #420).
    pub async fn register_mcp_tools_for_dir(
        &self,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::protocol::ServerEvent>>,
        shared_pool: Option<std::sync::Arc<crate::mcp::SharedMcpPool>>,
        session_id: Option<String>,
        working_dir: Option<std::path::PathBuf>,
    ) {
        use crate::mcp::McpManager;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let mcp_manager = if let Some(pool) = shared_pool {
            let sid = session_id.unwrap_or_else(|| "unknown".to_string());
            Arc::new(RwLock::new(McpManager::with_shared_pool_for_dir(
                pool,
                sid,
                working_dir,
            )))
        } else {
            Arc::new(RwLock::new(McpManager::new()))
        };

        // Register MCP management tool immediately (with registry for dynamic tool registration)
        let mcp_tool =
            mcp::McpManagementTool::new(Arc::clone(&mcp_manager)).with_registry(self.clone());
        self.register("mcp".to_string(), Arc::new(mcp_tool) as Arc<dyn Tool>)
            .await;
        self.register(
            "mcp_search".to_string(),
            Arc::new(mcp::McpSearchTool::new(Arc::clone(&mcp_manager)).with_registry(self.clone()))
                as Arc<dyn Tool>,
        )
        .await;
        self.register(
            "mcp_call".to_string(),
            Arc::new(mcp::McpCallTool::new(Arc::clone(&mcp_manager)).with_registry(self.clone()))
                as Arc<dyn Tool>,
        )
        .await;

        // Check if we have enabled servers to connect to. Disabled servers stay
        // configured (visible to the mcp management tool, connectable by name)
        // but are not spawned, advertised, or shown as connecting (issue #436).
        let (enabled_count, disabled_count) = {
            let manager = mcp_manager.read().await;
            let enabled = manager
                .config()
                .servers
                .values()
                .filter(|cfg| cfg.is_enabled())
                .count();
            (enabled, manager.config().servers.len() - enabled)
        };

        if disabled_count > 0 {
            crate::logging::info(&format!(
                "MCP: {} disabled server(s) in config (kept, not spawned)",
                disabled_count
            ));
        }

        if enabled_count > 0 {
            crate::logging::info(&format!("MCP: Found {} server(s) in config", enabled_count));

            // Send immediate "connecting" status so the TUI shows loading state
            // Server names with count 0 means "connecting..."
            if let Some(ref tx) = event_tx {
                let server_names: Vec<String> = {
                    let manager = mcp_manager.read().await;
                    manager
                        .config()
                        .servers
                        .iter()
                        .filter(|(_, cfg)| cfg.is_enabled())
                        .map(|(name, _)| format!("{}:0", name))
                        .collect()
                };
                let _ = tx.send(crate::protocol::ServerEvent::McpStatus {
                    servers: server_names,
                });
            }

            // Advertise-early: register proxy tools for each configured server
            // from the on-disk schema cache *before* connections settle, so the
            // first locked tool snapshot already contains MCP tools and we avoid
            // the intentional prompt-cache miss entirely (#206 Phase 2). The
            // proxies connect-on-first-call. Servers with no cached schemas yet
            // (cold start, or reconfigured) fall back to the post-connect
            // registration + one-shot late-register rebuild below.
            let schema_cache = crate::mcp::McpSchemaCache::load();
            let mut advertised_servers: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            {
                let config_servers: Vec<(String, crate::mcp::McpServerConfig)> = {
                    let manager = mcp_manager.read().await;
                    manager
                        .config()
                        .servers
                        .iter()
                        .filter(|(_, cfg)| cfg.is_enabled())
                        .map(|(name, cfg)| (name.clone(), cfg.clone()))
                        .collect()
                };
                let mut advertised_tool_count = 0usize;
                let mut cached_tools = Vec::new();
                for (server, cfg) in &config_servers {
                    if let Some(cached) = schema_cache.tools_for(server, cfg) {
                        advertised_tool_count += cached.len();
                        cached_tools
                            .extend(cached.iter().cloned().map(|tool| (server.clone(), tool)));
                        advertised_servers.insert(server.clone());
                    }
                }
                self.reconcile_mcp_tools(crate::mcp::create_mcp_tools_from_cached_many(
                    &cached_tools,
                    Arc::clone(&mcp_manager),
                ))
                .await;
                if advertised_tool_count > 0 {
                    crate::logging::info(&format!(
                        "MCP: advertised {} cached tool(s) from {} server(s) at spawn \
                         (connect-on-first-call); zero prompt-cache miss expected (#206)",
                        advertised_tool_count,
                        advertised_servers.len()
                    ));
                    // Reflect the advertised tools in the status indicator
                    // immediately so the UI shows them before connections settle.
                    if let Some(ref tx) = event_tx {
                        let mut counts: std::collections::BTreeMap<String, usize> =
                            std::collections::BTreeMap::new();
                        for (server, cfg) in &config_servers {
                            if let Some(cached) = schema_cache.tools_for(server, cfg) {
                                counts.insert(server.clone(), cached.len());
                            }
                        }
                        let servers: Vec<String> = counts
                            .into_iter()
                            .map(|(name, count)| format!("{}:{}", name, count))
                            .collect();
                        let _ = tx.send(crate::protocol::ServerEvent::McpStatus { servers });
                    }
                }
            }

            // Publish each completed connection independently. A slow handshake
            // must not hide an already-connected peer until an entire batch ends.
            let registry = self.clone();
            // Scripts that run before a never-listed server finishes its first
            // connection wait for it (bounded) instead of finding no tools.
            let pending: Vec<String> = mcp_manager
                .read()
                .await
                .config()
                .servers
                .iter()
                .filter(|(_, cfg)| cfg.is_enabled())
                .map(|(name, _)| name.clone())
                .collect();
            registry.mark_mcp_connecting(pending);
            tokio::spawn(async move {
                use futures::StreamExt;

                /// Leaves `connecting` on every path out of one connection's
                /// handling, including the early `continue`s.
                struct Settled<'a>(&'a Registry, String);
                impl Drop for Settled<'_> {
                    fn drop(&mut self) {
                        self.0.mark_mcp_settled(&self.1);
                    }
                }

                let configs: Vec<_> = mcp_manager
                    .read()
                    .await
                    .config()
                    .servers
                    .iter()
                    .filter(|(_, cfg)| cfg.is_enabled())
                    .map(|(name, cfg)| (name.clone(), cfg.clone()))
                    .collect();
                let mut connections = futures::stream::FuturesUnordered::new();
                for (name, config) in configs {
                    let manager = Arc::clone(&mcp_manager);
                    connections.push(async move {
                        let result = manager.read().await.connect(&name, &config).await;
                        (name, result)
                    });
                }

                // Prune cache entries for servers no longer configured, once.
                {
                    let configured: Vec<String> = mcp_manager
                        .read()
                        .await
                        .config()
                        .servers
                        .keys()
                        .cloned()
                        .collect();
                    let mut cache = crate::mcp::McpSchemaCache::load();
                    if cache.retain_servers(&configured) {
                        cache.save();
                    }
                }
                let mut server_counts: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                while let Some((name, result)) = connections.next().await {
                    let _settled = Settled(&registry, name.clone());
                    if let Err(error) = result {
                        crate::logging::event_rate_limited(
                            crate::logging::LogLevel::Error,
                            &format!("mcp_register_failed:{name}"),
                            std::time::Duration::from_secs(60),
                            "MCP_REGISTER_FAILED",
                            vec![("server", name), ("error", format!("{error:#}"))],
                        );
                        continue;
                    }
                    crate::logging::info(&format!("MCP: Connected to '{name}'"));

                    // Only this server changed. Rebuilding every server's
                    // proxies and re-reconciling the whole schema cache on each
                    // of ~13 connections per session made a reload storm spend
                    // most of its CPU here.
                    let handler_cpu = crate::cpu_time::thread_cpu_ms();
                    let handler_start = std::time::Instant::now();
                    let Some(defs) = mcp_manager.read().await.server_tools(&name).await else {
                        continue;
                    };
                    let defs_cpu = crate::cpu_time::thread_cpu_ms() - handler_cpu;
                    let tools = crate::mcp::create_mcp_tools_from_cached(
                        &name,
                        &defs,
                        Arc::clone(&mcp_manager),
                    );
                    // Servers with no tools are not listed, matching the
                    // previous count-by-tool-identity status.
                    if tools.is_empty() {
                        server_counts.remove(&name);
                    } else {
                        server_counts.insert(name.clone(), tools.len());
                    }
                    let build_cpu = crate::cpu_time::thread_cpu_ms() - handler_cpu;
                    registry.refresh_connected_mcp_server(tools, &name).await;
                    let refresh_cpu = crate::cpu_time::thread_cpu_ms() - handler_cpu;

                    let refresh_ms = handler_start.elapsed().as_millis();
                    // Keep the on-disk schema cache in sync so the next spawn can
                    // advertise up-to-date tools with zero cache miss (#206).
                    // A connected server with zero tools is still cached so a
                    // later startup cannot advertise removed tools.
                    let config = mcp_manager
                        .read()
                        .await
                        .config()
                        .servers
                        .get(&name)
                        .cloned();
                    if let Some(config) = config
                        && crate::mcp::McpSchemaCache::sync_server(&name, &config, &defs)
                    {
                        crate::logging::info(
                            "MCP: updated on-disk tool-schema cache from live connection (#206)",
                        );
                    }

                    crate::logging::info(&format!(
                        "[TIMING] mcp_connected: server={}, tools={}, refresh={}ms, total={}ms, cpu~{:.0}ms (defs~{:.0} build~{:.0} refresh~{:.0})",
                        name,
                        server_counts.get(&name).copied().unwrap_or(0),
                        refresh_ms,
                        handler_start.elapsed().as_millis(),
                        crate::cpu_time::thread_cpu_ms() - handler_cpu,
                        defs_cpu,
                        build_cpu,
                        refresh_cpu,
                    ));
                    // Notify client of MCP status
                    if let Some(ref tx) = event_tx {
                        let servers: Vec<String> = server_counts
                            .iter()
                            .map(|(name, count)| format!("{}:{}", name, count))
                            .collect();
                        let _ = tx.send(crate::protocol::ServerEvent::McpStatus { servers });
                    }
                }
            });
        }
    }

    /// Register self-dev tools (only for canary/self-dev sessions)
    pub async fn register_selfdev_tools(&self) {
        // Self-dev management tool
        let selfdev_tool = selfdev::SelfDevTool::new();
        self.register(
            "selfdev".to_string(),
            Arc::new(selfdev_tool) as Arc<dyn Tool>,
        )
        .await;

        // Debug socket tool for direct debug socket access
        let debug_socket_tool = debug_socket::DebugSocketTool::new();
        self.register(
            "debug_socket".to_string(),
            Arc::new(debug_socket_tool) as Arc<dyn Tool>,
        )
        .await;
    }

    /// Register ambient-mode tools (only for ambient sessions)
    pub async fn register_ambient_tools(&self) {
        self.register(
            "end_ambient_cycle".to_string(),
            Arc::new(crate::ambient_tools::EndAmbientCycleTool::new()) as Arc<dyn Tool>,
        )
        .await;

        self.register(
            "schedule_ambient".to_string(),
            Arc::new(crate::ambient_tools::ScheduleAmbientTool::new()) as Arc<dyn Tool>,
        )
        .await;

        self.register(
            "request_permission".to_string(),
            Arc::new(crate::ambient_tools::RequestPermissionTool::new()) as Arc<dyn Tool>,
        )
        .await;

        self.register(
            "send_message".to_string(),
            Arc::new(crate::ambient_tools::SendChannelMessageTool::new()) as Arc<dyn Tool>,
        )
        .await;
    }
}

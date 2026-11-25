use super::{Agent, HashSet, Instant, Provider, Registry, Session, logging};
use std::sync::Arc;

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, registry: Registry) -> Self {
        Self::new_with_initial_working_dir(provider, registry, None)
    }

    pub fn new_ephemeral(provider: Arc<dyn Provider>, registry: Registry) -> Self {
        Self::new_with_initial_ownership(
            provider,
            registry,
            None,
            Session::create_ephemeral(),
            true,
        )
    }

    pub(crate) fn mark_agent_started(&mut self) {
        self.session.agent_started = true;
    }

    pub(crate) fn new_with_initial_working_dir(
        provider: Arc<dyn Provider>,
        registry: Registry,
        working_dir: Option<&str>,
    ) -> Self {
        Self::new_with_parent_and_initial_working_dir(provider, registry, working_dir, None)
    }

    /// A connection may only be a viewer attaching to an existing Agent.
    /// Do not count its provisional session before that choice is resolved.
    pub(crate) fn new_provisional_with_initial_working_dir(
        provider: Arc<dyn Provider>,
        registry: Registry,
        working_dir: Option<&str>,
    ) -> Self {
        let mut agent = Self::new_with_initial_ownership(
            provider,
            registry,
            working_dir,
            Session::create(None, None),
            false,
        );
        agent.apply_root_profile_default();
        agent
    }

    pub(crate) fn new_with_parent_and_initial_working_dir(
        provider: Arc<dyn Provider>,
        registry: Registry,
        working_dir: Option<&str>,
        parent_id: Option<String>,
    ) -> Self {
        let mut agent = Self::new_with_initial_ownership(
            provider,
            registry,
            working_dir,
            Session::create(parent_id, None),
            true,
        );
        agent.apply_root_profile_default();
        agent
    }

    fn new_with_initial_ownership(
        provider: Arc<dyn Provider>,
        registry: Registry,
        working_dir: Option<&str>,
        mut session: Session,
        track_concurrency: bool,
    ) -> Self {
        let start = Instant::now();
        let mut marks: Vec<(&'static str, u128)> = Vec::with_capacity(10);
        let mut lap = Instant::now();
        let mut mark = |name: &'static str, lap: &mut Instant| {
            marks.push((name, lap.elapsed().as_millis()));
            *lap = Instant::now();
        };
        let tool_selection = crate::config::config().tools.selection();
        if let Some(working_dir) = working_dir {
            session.working_dir = Some(working_dir.to_string());
        }
        mark("session", &mut lap);
        let mut agent = Self::build_base(
            provider,
            registry,
            session,
            tool_selection.allowed_tools,
            tool_selection.disabled_tools,
        );
        mark("build_base", &mut lap);
        // activate_concurrency_tracking writes the presence markers when the
        // session is committed. A provisional agent replaced by a resumed
        // session never touches the marker files.
        agent.session.mark_active_unregistered();
        mark("mark_active", &mut lap);
        agent.session.model = Some(agent.provider_model());
        agent.session.provider_key = agent.provider_key_for_new_session();
        agent.reconcile_explicit_provider_pin_route();
        agent.restore_speed_from_session();
        mark("provider", &mut lap);
        // A provisional Agent is usually replaced by a resumed session (every
        // reconnect after a reload), so do not fork the git/hardware probes
        // for its context message until it actually becomes the session.
        if track_concurrency {
            agent.session.ensure_initial_session_context_message();
            agent.migrate_legacy_user_asks();
        }
        mark("context_msg", &mut lap);
        agent.seed_compaction_from_session();
        mark("compaction", &mut lap);
        agent.log_env_snapshot("create");
        mark("env_snapshot", &mut lap);
        agent.fire_session_lifecycle_hook("session_start", "create");
        if track_concurrency {
            agent.activate_concurrency_tracking();
        }
        mark("hooks_concurrency", &mut lap);
        let breakdown = marks
            .iter()
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect::<Vec<_>>()
            .join(", ");
        logging::info(&format!("[TIMING] agent_new breakdown: {breakdown}"));
        let setup_ms = start.elapsed().as_millis();
        let telemetry_start = Instant::now();
        if track_concurrency {
            agent.begin_telemetry_session();
        } else {
            agent.telemetry_session_deferred = true;
        }
        logging::info(&format!(
            "[TIMING] agent_new: setup={}ms, telemetry={}ms, total={}ms",
            setup_ms,
            telemetry_start.elapsed().as_millis(),
            start.elapsed().as_millis(),
        ));
        agent
    }

    /// Reopen a persisted worker session: soft interrupts queued before the
    /// previous runtime went away are restored, and a runtime brought back
    /// after an idle unload may be driven by a daemon wake without a client.
    pub fn reopen_session(
        provider: Arc<dyn Provider>,
        registry: Registry,
        session: Session,
        restored_from_idle_unload: bool,
    ) -> Self {
        let mut agent = Self::new_with_session(provider, registry, session, None);
        agent.restore_persisted_soft_interrupts();
        agent.restored_from_idle_unload = restored_from_idle_unload;
        agent
    }

    pub fn new_with_session(
        provider: Arc<dyn Provider>,
        registry: Registry,
        session: Session,
        allowed_tools: Option<HashSet<String>>,
    ) -> Self {
        let tool_selection = if let Some(allowed_tools) = allowed_tools {
            crate::config::ToolSelection {
                allowed_tools: Some(allowed_tools),
                disabled_tools: HashSet::new(),
            }
        } else {
            crate::config::config().tools.selection()
        };
        let mut agent = Self::build_base(
            provider,
            registry,
            session,
            tool_selection.allowed_tools,
            tool_selection.disabled_tools,
        );
        // begin_concurrency_tracking below writes the presence markers.
        agent.session.mark_active_unregistered();
        if agent.session.provider_key.is_none() {
            agent.session.provider_key = agent.provider_key_for_new_session();
        }
        agent.restore_session_model_from_session();
        agent.restore_reasoning_effort_from_session();
        agent.restore_speed_from_session();
        agent.restore_cache_sidecars();
        agent.session.ensure_initial_session_context_message();
        agent.migrate_legacy_user_asks();
        agent.sync_memory_dedup_state_from_session();
        agent.seed_compaction_from_session();
        agent.log_env_snapshot("attach");
        agent.fire_session_lifecycle_hook("session_start", "attach");
        agent.begin_concurrency_tracking();
        crate::telemetry::begin_session_with_parent(
            agent.provider.name(),
            &agent.provider.model(),
            agent.session.parent_id.clone(),
            false,
        );
        agent
    }
}

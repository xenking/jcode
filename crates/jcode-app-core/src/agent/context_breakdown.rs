use super::tool_surface::{LATE_MCP_ANNOUNCEMENT_MARKER, LATE_SKILL_ANNOUNCEMENT_MARKER};
use super::*;
use crate::compaction::{self, CompactionManager, CompactionProjection, ManualSnapcompactPreview};
use crate::prompt::{self, SplitSystemPrompt};
use crate::provider::models::{
    context_limit_for_model_with_provider, provider_for_model_with_hint,
};
use crate::session_telemetry::{self, SessionTelemetry};
use crate::util::APPROX_CHARS_PER_TOKEN;
use jcode_session_types::{
    ContextActionForecast, ContextActionMeasurement, ContextActionSavings, ContextActionStatus,
    ContextBreakdown, ContextBreakdownCategory, ContextBreakdownKind as Kind, ContextMeter,
    ContextMeterState as State, ContextOccupancySource as Source,
};
use std::path::Path;

pub(crate) struct ContextBreakdownSnapshot {
    breakdown: ContextBreakdown,
    compact: Option<ManualSnapcompactPreview>,
}

impl ContextBreakdownSnapshot {
    pub(crate) async fn complete(mut self) -> ContextBreakdown {
        if let Some(compact) = self.compact
            && let Ok(Some((before_tokens, after_tokens))) =
                tokio::task::spawn_blocking(|| compact.measure()).await
            && let Some(forecast) = self.breakdown.action_forecast.as_mut()
        {
            forecast.compact_measurement = Some(ContextActionMeasurement {
                before_tokens,
                after_tokens,
            });
            forecast.compact_status = None;
        }
        self.breakdown
    }
}

impl Agent {
    #[cfg(test)]
    pub(crate) async fn inspect_context_breakdown(
        &self,
        summary_only: bool,
        include_snapcompact: bool,
    ) -> ContextBreakdown {
        self.snapshot_context_breakdown(summary_only, include_snapcompact)
            .await
            .complete()
            .await
    }

    pub(crate) async fn snapshot_context_breakdown(
        &self,
        summary_only: bool,
        include_snapcompact: bool,
    ) -> ContextBreakdownSnapshot {
        let model = self.provider_model();
        let provider = self.provider.name();
        let telemetry = session_telemetry::read_context(&self.session.id);
        let mut meter = self.context_policy().await;
        apply_occupancy(&mut meter, &self.session, telemetry.as_ref(), State::Live);
        if summary_only && meter.occupied_tokens.is_some() {
            return ContextBreakdownSnapshot {
                breakdown: meter_only(Some(self.session.id.clone()), Some(model), meter),
                compact: None,
            };
        }
        let source = self.session.messages_for_provider_uncached();
        let (action_forecast, compact) = if summary_only {
            (None, None)
        } else {
            let (forecast, compact) = self.forecast_context_actions(&source);
            (Some(forecast), compact)
        };
        let projection = inspect_messages(
            &self.session,
            source,
            compaction::snapcompact_pages_usable(self.provider.supports_image_input(), &model),
            compaction::openai_native_replay_route(provider, &model),
        );
        let tools = self.inspect_tool_definitions(&projection.messages).await;
        let (prompt, skills_chars) = self.system_prompt_with_accounting(None, true);
        let mut counts = Counts {
            include_snapcompact,
            action_forecast,
            ..Counts::default()
        };
        counts.prompt(&prompt, skills_chars);
        counts.messages(&self.session, &projection, true);
        counts.tools(&tools, &projection.messages, |name| {
            crate::tool::sdk::custom(self.session_id(), name)
        });
        ContextBreakdownSnapshot {
            breakdown: counts.finish(Some(self.session.id.clone()), Some(model), meter),
            compact,
        }
    }

    pub(super) async fn record_completed_context_observation(&mut self, prompt_tokens: u64) {
        if prompt_tokens == 0 {
            return;
        }
        self.sync_bound_account_route();
        let policy = self.context_policy().await;
        session_telemetry::save_context(
            &self.session,
            prompt_tokens,
            policy.effective_budget_tokens.map(|n| n as usize),
            self.provider.name(),
            self.provider.active_resolved_credential(),
            self.provider.serving_oauth_account_label(),
            Some(policy),
        );
    }

    async fn context_policy(&self) -> ContextMeter {
        let mut meter = ContextMeter::default();
        let mode = self.registry.compaction().read().await.explicit_mode();
        configure_meter(
            &mut meter,
            Some(self.provider.name()),
            Some(&self.provider_model()),
            self.session.context_cap,
            Some(self.provider_context_window()),
            mode,
        );
        if let jcode_provider_core::context_pricing::LongContextPricing::Cutoff {
            above_input_tokens,
            ..
        } = self.provider.active_long_context_pricing()
        {
            meter.long_context_pricing_above_input_tokens = Some(above_input_tokens);
        }
        let jcode_compaction = self.provider.uses_jcode_compaction();
        if !jcode_compaction {
            meter.compaction_threshold_tokens = self
                .provider
                .native_compaction_threshold_tokens()
                .map(|n| n as u64);
            meter.compaction_mode = meter
                .compaction_threshold_tokens
                .map(|_| "provider_native".into());
        }
        if self.is_swarm_worker() && jcode_compaction {
            meter.compaction_threshold_tokens = None;
            meter.compaction_mode = Some("worker".into());
        }
        if !jcode_compaction || self.is_swarm_worker() {
            meter.idle_precompaction_tokens = None;
        }
        meter
    }
}

impl Agent {
    fn forecast_context_actions(
        &self,
        messages: &[Message],
    ) -> (ContextActionForecast, Option<ManualSnapcompactPreview>) {
        let compaction = self.registry.compaction();
        let Ok(manager) = compaction.try_read() else {
            return (
                ContextActionForecast::unavailable(ContextActionStatus::Busy),
                None,
            );
        };
        if manager.is_compacting() {
            return (
                ContextActionForecast::unavailable(ContextActionStatus::Busy),
                None,
            );
        }
        let (compact_status, compact) =
            if self.is_swarm_worker() || !self.provider.supports_compaction() {
                (ContextActionStatus::Unsupported, None)
            } else {
                match manager.snapshot_manual_snapcompact(messages, &*self.provider) {
                    Ok(compact) => (ContextActionStatus::SavingsUnknown, compact),
                    Err(_) => (ContextActionStatus::NoEligible, None),
                }
            };
        let start = manager.active_start(messages);
        let config = manager.manual_tool_shake_config();
        let (shake, shake_status) = match self.session.preview_shake_stored_messages(start, &config)
        {
            compaction::ShakeOutcome::Shaken { tokens_saved, .. } => (
                Some(ContextActionSavings::Estimate {
                    tokens: tokens_saved as u64,
                }),
                None,
            ),
            compaction::ShakeOutcome::NoChange => (None, Some(ContextActionStatus::NoEligible)),
        };
        (
            ContextActionForecast {
                current_tokens: None,
                compact: None,
                shake,
                compact_measurement: None,
                compact_status: Some(compact_status),
                shake_status,
            },
            compact,
        )
    }
}

/// No Agent or provider route is constructed to inspect an unloaded session.
pub(crate) fn inspect_saved(
    session: &Session,
    summary_only: bool,
    include_snapcompact: bool,
) -> ContextBreakdown {
    let model = session.model.as_deref();
    let telemetry = session_telemetry::read_context(&session.id);
    let matching = telemetry
        .as_ref()
        .filter(|sample| sample.matches_session(session));
    let provider = matching
        .and_then(|sample| sample.serving_provider.as_deref())
        .or_else(|| {
            model.and_then(|model| {
                provider_for_model_with_hint(model, session.provider_key.as_deref())
            })
        })
        .or(session.provider_key.as_deref());
    let cap = session.context_cap.filter(|n| *n > 0).map(|n| n as u64);
    let historical_policy = matching.and_then(|sample| sample.context_policy.as_ref());
    let mut meter = historical_policy
        .filter(|policy| policy.session_cap_tokens == cap)
        .cloned()
        .unwrap_or_else(|| ContextMeter {
            session_cap_tokens: cap,
            model_window_tokens: model
                .and_then(|model| context_limit_for_model_with_provider(model, provider))
                .map(|n| n as u64),
            effective_budget_tokens: matching
                .filter(|_| historical_policy.is_none())
                .and_then(|sample| sample.context_limit)
                .map(|n| n as u64),
            ..Default::default()
        });
    apply_occupancy(&mut meter, session, telemetry.as_ref(), State::Saved);
    if summary_only && meter.occupied_tokens.is_some() {
        return meter_only(Some(session.id.clone()), session.model.clone(), meter);
    }
    let past_request = meter.occupied_tokens.is_some()
        || session.last_provider_request_at.is_some()
        || session
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant);
    let mut counts = Counts {
        include_snapcompact,
        ..Counts::default()
    };
    let recorded = crate::session::load_prompt_inputs(&session.id)
        .filter(|inputs| inputs.working_dir == session.working_dir);
    if let Some((inputs, snapshot)) = recorded
        .as_ref()
        .and_then(|inputs| Some((inputs, inputs.static_prompt.as_ref()?)))
    {
        let prompt = SplitSystemPrompt {
            static_part: snapshot.text.clone(),
            dynamic_part: String::new(),
        };
        counts.prompt(&prompt, snapshot.skills_chars(&inputs.skills));
    } else if !past_request {
        let (prompt, skills_chars) = bootstrap_prompt(
            session.working_dir.as_deref().map(Path::new),
            session.is_canary,
            session.system_prompt.as_deref(),
        );
        counts.prompt(&prompt, Some(skills_chars));
    }
    let native = provider
        .zip(model)
        .is_some_and(|(provider, model)| compaction::openai_native_replay_route(provider, model));
    // Saved sessions do not carry the provider's image capability. Only a
    // model policy that explicitly rules pages out makes this projection known.
    let snapcompact_known =
        model.is_some_and(|model| !compaction::snapcompact_pages_usable(true, model));
    let projection = inspect_messages(
        session,
        session.messages_for_provider_uncached(),
        false,
        native,
    );
    counts.messages(session, &projection, snapcompact_known);
    if let Some(tools) = super::tool_replay::saved_context_tools(session) {
        counts.tools(&tools, &projection.messages, |_| false);
    }
    counts.finish(Some(session.id.clone()), session.model.clone(), meter)
}

pub(crate) async fn inspect_draft(
    provider_template: &Arc<dyn Provider>,
    model: Option<&str>,
    cwd: Option<&Path>,
    include_snapcompact: bool,
) -> ContextBreakdown {
    let default_model = provider_template.model();
    let model = model.unwrap_or(&default_model);
    let selected_template = model == default_model;
    let provider = if selected_template {
        Some(provider_template.name())
    } else {
        provider_for_model_with_hint(model, None)
    };
    let window = if selected_template {
        Some(provider_template.context_window())
    } else {
        context_limit_for_model_with_provider(model, provider)
    };
    let (prompt, skills_chars) = bootstrap_prompt(cwd, false, None);
    let registry = Registry::new(provider_template.clone()).await;
    let selection = crate::config::config().tools.selection();
    let mut tools = registry.definitions(selection.allowed_tools.as_ref()).await;
    tools.retain(|tool| !registry.tool_is_disabled(&selection.disabled_tools, &tool.name));
    Agent::apply_selfdev_tool_surface(
        &mut tools,
        false,
        cwd.and_then(jcode_selfdev_types::desktop_repo_root)
            .is_some(),
    );
    if selected_template {
        let loader = crate::tool::load_tools::LOAD_TOOLS_NAME;
        let loader_allowed = selection
            .allowed_tools
            .as_ref()
            .is_none_or(|set| registry.tool_is_allowed(set, loader))
            && !registry.tool_is_disabled(&selection.disabled_tools, loader);
        let native = crate::config::config().tools.mcp_tools != crate::config::McpToolsMode::Eager
            && provider_template.supports_deferred_tools();
        crate::tool::load_tools::apply_deferral(&mut tools, native, loader_allowed, |_| false);
    }
    let mut counts = Counts {
        conversation_chars: Some(0),
        snapcompact_chars: Some(0),
        include_snapcompact,
        ..Counts::default()
    };
    counts.prompt(&prompt, Some(skills_chars));
    counts.tools(&tools, &[], |_| false);
    // The draft has a local baseline, not a discovered per-session tool surface.
    counts.forget_dynamic_tools();
    let mut meter = ContextMeter {
        state: State::Draft,
        ..Default::default()
    };
    configure_meter(&mut meter, provider, Some(model), None, window, None);
    counts.finish(None, Some(model.to_owned()), meter)
}

pub(crate) fn unavailable(session_id: String) -> ContextBreakdown {
    Counts::default().finish(Some(session_id), None, ContextMeter::default())
}

fn meter_only(
    session_id: Option<String>,
    model: Option<String>,
    meter: ContextMeter,
) -> ContextBreakdown {
    ContextBreakdown {
        session_id,
        model,
        total_tokens: meter.occupied_tokens,
        limit_tokens: meter.effective_budget_tokens,
        estimated: true,
        categories: Vec::new(),
        meter: Some(meter),
        residual_tokens: None,
        action_forecast: None,
    }
}

fn bootstrap_prompt(
    cwd: Option<&Path>,
    canary: bool,
    custom: Option<&str>,
) -> (SplitSystemPrompt, usize) {
    if let Some(custom) = custom {
        return (
            SplitSystemPrompt {
                static_part: custom.to_owned(),
                dynamic_part: String::new(),
            },
            0,
        );
    }
    let global = SkillRegistry::shared_snapshot();
    let registry = SkillRegistry::effective_for_working_dir(&global, cwd);
    let skills = Agent::prompt_skill_infos(&registry);
    let (prompt, info) = prompt::build_system_prompt_split(None, &skills, canary, None, cwd);
    (prompt, info.skills_chars)
}

fn configure_meter(
    meter: &mut ContextMeter,
    provider: Option<&str>,
    model: Option<&str>,
    cap: Option<usize>,
    window: Option<usize>,
    mode: Option<crate::config::CompactionMode>,
) {
    meter.session_cap_tokens = cap.filter(|cap| *cap > 0).map(|cap| cap as u64);
    let Some(window) = window.filter(|window| *window > 0) else {
        return;
    };
    let mut compaction = CompactionManager::new().with_budget(window);
    compaction.set_active_model(provider, model);
    compaction.set_context_cap_override(cap);
    if let Some(mode) = mode {
        compaction.set_mode(mode);
    }
    meter.model_window_tokens = Some(window as u64);
    meter.effective_budget_tokens = Some(compaction.token_budget() as u64);
    meter.compaction_mode = Some(compaction.mode().as_str().into());
    meter.compaction_threshold_tokens = compaction
        .reactive_compaction_threshold_tokens()
        .map(|n| n as u64);
    meter.idle_precompaction_tokens = compaction.idle_precompaction_tokens().map(|n| n as u64);
}

fn apply_occupancy(
    meter: &mut ContextMeter,
    session: &Session,
    telemetry: Option<&SessionTelemetry>,
    state: State,
) {
    meter.state = state;
    meter.occupied_tokens = None;
    meter.source = Source::Unknown;
    meter.measured_at_ms = None;
    meter.route = None;
    let report = if let Some(telemetry) = telemetry {
        (telemetry.context_tokens > 0 && telemetry.matches_session(session)).then(|| {
            meter.route = telemetry
                .route_api_method
                .clone()
                .or_else(|| telemetry.serving_provider.clone());
            (
                Some(telemetry.measured_at),
                telemetry.context_tokens,
                Source::ProviderReported,
            )
        })
    } else {
        session.messages.iter().rev().find_map(|message| {
            let tokens = message.token_usage.as_ref()?.prompt_tokens?;
            (tokens > 0).then_some((message.timestamp, tokens, Source::HistoricalReport))
        })
    };
    let compacted = session
        .replay_events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            crate::session::StoredReplayEventKind::Compaction { post_tokens, .. } => {
                Some((event.timestamp, *post_tokens))
            }
            _ => None,
        });
    if let Some((when, post_tokens)) = compacted.filter(|(when, _)| {
        !report.is_some_and(|(reported_at, _, _)| {
            reported_at.is_some_and(|reported_at| reported_at > *when)
        })
    }) {
        meter.occupied_tokens = post_tokens;
        if post_tokens.is_some() {
            meter.source = Source::CompactionEstimate;
            meter.measured_at_ms = Some(when.timestamp_millis());
        }
        meter.route = None;
    } else if let Some((when, tokens, source)) = report {
        meter.occupied_tokens = Some(tokens);
        meter.source = source;
        meter.measured_at_ms = when.map(|when| when.timestamp_millis());
    }
}

fn inspect_messages(
    session: &Session,
    messages: Vec<Message>,
    archive: bool,
    native: bool,
) -> CompactionProjection {
    let mut projection = if let Some(state) = session.compaction.as_deref() {
        let mut manager = CompactionManager::new();
        manager.restore_persisted_state(state, messages.len());
        manager.inspect_provider_messages(session, &messages, archive, native)
    } else {
        CompactionProjection {
            messages,
            snapcompact_message_index: None,
        }
    };
    projection.messages = Agent::bounded_outbound_images(projection.messages);
    projection
}

#[derive(Default)]
struct Counts {
    include_snapcompact: bool,
    prompt_present: bool,
    system_chars: Option<usize>,
    skill_chars: Option<usize>,
    builtin_tokens: Option<usize>,
    dynamic_tokens: Option<usize>,
    agent_tokens: Option<usize>,
    conversation_chars: Option<usize>,
    snapcompact_chars: Option<usize>,
    dynamic_reminder_chars: usize,
    unattributed_chars: usize,
    action_forecast: Option<ContextActionForecast>,
}

impl Counts {
    fn forget_dynamic_tools(&mut self) {
        self.dynamic_tokens = None;
    }

    fn prompt(&mut self, prompt: &SplitSystemPrompt, skills_chars: Option<usize>) {
        self.prompt_present = true;
        let chars = prompt.chars();
        if let Some(skills) = skills_chars.filter(|&skills| skills <= prompt.static_part.len()) {
            self.system_chars = Some(chars - skills);
            self.skill_chars = Some(skills);
        } else {
            self.unattributed_chars += chars;
        }
    }

    fn messages(
        &mut self,
        session: &Session,
        projection: &CompactionProjection,
        snapcompact_known: bool,
    ) {
        let reminders: HashSet<_> = session
            .messages
            .iter()
            .filter_map(|message| {
                if message.display_role != Some(StoredDisplayRole::System) {
                    return None;
                }
                let [ContentBlock::Text { text, .. }] = message.content.as_ref() else {
                    return None;
                };
                reminder_kind(text).map(|kind| ((message.timestamp, text.as_str()), kind))
            })
            .collect();
        let mut chars = 0;
        self.snapcompact_chars = projection.snapcompact_message_index.is_none().then_some(0);
        for (index, message) in projection.messages.iter().enumerate() {
            if self.include_snapcompact && projection.snapcompact_message_index == Some(index) {
                self.snapcompact_chars =
                    snapcompact_known.then(|| compaction::message_char_count(message));
                continue;
            }
            let size = compaction::message_char_count(message);
            let kind = match (&message.role, message.content.as_ref()) {
                (Role::User, [ContentBlock::Text { text, .. }]) => {
                    reminder_kind(text).filter(|kind| {
                        reminders.contains(&((message.timestamp, text.as_str()), *kind))
                    })
                }
                _ => None,
            };
            match kind {
                Some(Reminder::Skill) => {
                    if let Some(skills) = &mut self.skill_chars {
                        *skills += size;
                    } else {
                        self.unattributed_chars += size;
                    }
                }
                Some(Reminder::Mcp) => self.dynamic_reminder_chars += size,
                None => chars += size,
            }
        }
        self.conversation_chars = Some(chars);
    }

    fn tools(
        &mut self,
        tools: &[ToolDefinition],
        messages: &[Message],
        custom: impl Fn(&str) -> bool,
    ) {
        let references: HashSet<_> = messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolReference { tool_name, .. } => Some(tool_name.as_str()),
                _ => None,
            })
            .collect();
        let (mut builtin, mut dynamic, mut agents) = (0, 0, 0);
        for tool in tools
            .iter()
            .filter(|tool| !tool.defer_loading || references.contains(tool.name.as_str()))
        {
            let tokens = tool.prompt_token_estimate();
            if tool.name.starts_with("mcp__") || custom(&tool.name) {
                dynamic += tokens;
            } else {
                let instructions = crate::tool::swarm_instruction_tokens(tool);
                agents += instructions;
                builtin += tokens - instructions;
            }
        }
        self.builtin_tokens = Some(builtin);
        self.dynamic_tokens = Some(dynamic);
        self.agent_tokens = Some(agents);
    }

    fn finish(
        self,
        session_id: Option<String>,
        model: Option<String>,
        mut meter: ContextMeter,
    ) -> ContextBreakdown {
        fn estimate(chars: usize) -> u64 {
            if chars == 0 {
                0
            } else {
                (chars / APPROX_CHARS_PER_TOKEN).max(1) as u64
            }
        }
        let counts = [
            (Kind::SystemPrompt, self.system_chars.map(estimate)),
            (
                Kind::BuiltinTools,
                self.builtin_tokens.map(|tokens| tokens as u64),
            ),
            (Kind::SkillsModes, self.skill_chars.map(estimate)),
            (
                Kind::DynamicTools,
                self.dynamic_tokens
                    .map(|tokens| tokens as u64 + estimate(self.dynamic_reminder_chars)),
            ),
            (Kind::Agents, self.agent_tokens.map(|tokens| tokens as u64)),
            (Kind::Conversation, self.conversation_chars.map(estimate)),
        ];
        let not_attributed = estimate(
            self.unattributed_chars
                + if self.dynamic_tokens.is_none() {
                    self.dynamic_reminder_chars
                } else {
                    0
                },
        );
        let categories: Vec<_> = counts
            .into_iter()
            .chain(
                self.include_snapcompact
                    .then_some((Kind::Snapcompact, self.snapcompact_chars.map(estimate))),
            )
            .chain([(
                Kind::Unattributed,
                (not_attributed > 0).then_some(not_attributed),
            )])
            .map(|(kind, tokens)| ContextBreakdownCategory { kind, tokens })
            .collect();
        let estimated_total = categories
            .iter()
            .filter_map(|category| category.tokens)
            .sum::<u64>();
        let local_total = (self.prompt_present
            && self.conversation_chars.is_some()
            && (!self.include_snapcompact || self.snapcompact_chars.is_some())
            && self.builtin_tokens.is_some())
        .then_some(estimated_total);
        if meter.occupied_tokens.is_none()
            && let Some(local_total) = local_total
        {
            meter.occupied_tokens = Some(local_total);
            meter.source = Source::LocalEstimate;
        }
        let residual_tokens = (categories.iter().any(|category| category.tokens.is_some())
            && matches!(
                meter.source,
                Source::ProviderReported | Source::HistoricalReport | Source::CompactionEstimate
            ))
        .then(|| {
            i64::try_from(
                i128::from(meter.occupied_tokens.unwrap_or_default()) - i128::from(estimated_total),
            )
            .ok()
        })
        .flatten();
        ContextBreakdown {
            session_id,
            model,
            total_tokens: meter.occupied_tokens,
            limit_tokens: meter.effective_budget_tokens,
            estimated: true,
            categories,
            action_forecast: self.action_forecast.map(|mut forecast| {
                forecast.current_tokens = local_total;
                forecast
            }),
            meter: Some(meter),
            residual_tokens,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Reminder {
    Skill,
    Mcp,
}

fn reminder_kind(text: &str) -> Option<Reminder> {
    let body = text.strip_prefix("<system-reminder>\n")?;
    if body.starts_with("# Active Skill\n")
        || body.starts_with(LATE_SKILL_ANNOUNCEMENT_MARKER)
        || body.starts_with(crate::skill::MODE_OFF_REMINDER)
    {
        Some(Reminder::Skill)
    } else if body.starts_with(LATE_MCP_ANNOUNCEMENT_MARKER) {
        Some(Reminder::Mcp)
    } else {
        None
    }
}

impl Agent {
    pub(super) async fn inspect_tool_definitions(
        &self,
        messages: &[Message],
    ) -> Vec<ToolDefinition> {
        let native = self.native_deferred_mcp();
        let locked = self
            .locked_tools
            .as_ref()
            .filter(|_| self.locked_tools_native_deferred == native);
        if !native
            && let Some(locked) = locked
            && (self.mcp_tools_mode != crate::config::McpToolsMode::Eager
                || self.mcp_late_register_resolved
                || !self.registry_has_new_mcp_tools(locked).await)
        {
            return locked.clone();
        }
        let mut tools = self.build_filtered_tool_definitions().await;
        if native && let Some(locked) = locked {
            let names: HashSet<_> = locked.iter().map(|tool| tool.name.as_str()).collect();
            tools.retain(|tool| tool.defer_loading && !names.contains(tool.name.as_str()));
            tools.splice(..0, locked.iter().cloned());
        }
        self.inspect_revealed_tools(tools, messages)
    }
}

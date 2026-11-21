use super::turn_execution::{LATE_MCP_ANNOUNCEMENT_MARKER, LATE_SKILL_ANNOUNCEMENT_MARKER};
use super::*;
use crate::compaction::{self, CompactionManager};
use crate::prompt::{self, SplitSystemPrompt};
use crate::provider::models::{
    context_limit_for_model_with_provider, provider_for_model_with_hint,
};
use crate::util::APPROX_CHARS_PER_TOKEN;
use jcode_session_types::{
    ContextBreakdown, ContextBreakdownCategory, ContextBreakdownKind as Kind,
};
use std::path::Path;

impl Agent {
    pub(crate) async fn inspect_context_breakdown(&self) -> ContextBreakdown {
        let model = self.provider_model();
        let provider = self.provider.name();
        let messages = inspect_messages(
            &self.session,
            compaction::snapcompact_pages_usable(self.provider.supports_image_input(), &model),
            compaction::openai_native_replay_route(provider, &model),
        );
        let tools = self.inspect_tool_definitions(&messages).await;
        let (prompt, skills_chars) = self.system_prompt_with_accounting(None, true);
        let mut counts = Counts::default();
        counts.prompt(&prompt, skills_chars);
        counts.messages(&self.session, &messages);
        counts.tools(&tools, &messages, |name| {
            crate::tool::sdk::custom(self.session_id(), name)
        });
        counts.finish(
            Some(self.session.id.clone()),
            Some(model.clone()),
            effective_limit(
                Some(provider),
                Some(&model),
                self.session.context_cap,
                Some(self.provider_context_window()),
            ),
            self.session.context_occupancy_tokens(),
        )
    }
}

/// No Agent or provider route is constructed to inspect an unloaded session.
pub(crate) fn inspect_saved(session: &Session) -> ContextBreakdown {
    let model = session.model.as_deref();
    let provider = session.provider_key.as_deref();
    let reported = session.context_occupancy_tokens();
    let past_request = reported.is_some()
        || session.last_provider_request_at.is_some()
        || session
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant);
    let mut counts = Counts::default();
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
    // The saved route may not have a restored image capability. The index-only
    // projection is an estimate, without inventing a number of image pages.
    let messages = inspect_messages(session, false, native);
    counts.messages(session, &messages);
    if let Some(tools) = super::tool_replay::saved_context_tools(session) {
        counts.tools(&tools, &messages, |_| false);
    }
    counts.finish(
        Some(session.id.clone()),
        session.model.clone(),
        effective_limit(
            provider,
            model,
            session.context_cap,
            model.and_then(|model| context_limit_for_model_with_provider(model, provider)),
        ),
        reported,
    )
}

pub(crate) async fn inspect_draft(
    provider_template: &Arc<dyn Provider>,
    model: Option<&str>,
    cwd: Option<&Path>,
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
        ..Counts::default()
    };
    counts.prompt(&prompt, Some(skills_chars));
    counts.tools(&tools, &[], |_| false);
    // The draft has a local baseline, not a discovered per-session tool surface.
    counts.dynamic_tokens = None;
    counts.finish(
        None,
        Some(model.to_owned()),
        effective_limit(provider, Some(model), None, window),
        None,
    )
}

pub(crate) fn unavailable(session_id: String) -> ContextBreakdown {
    Counts::default().finish(Some(session_id), None, None, None)
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

fn effective_limit(
    provider: Option<&str>,
    model: Option<&str>,
    cap: Option<usize>,
    window: Option<usize>,
) -> Option<u64> {
    let window = window.filter(|window| *window > 0)?;
    let mut compaction = CompactionManager::new().with_budget(window);
    compaction.set_active_model(provider, model);
    compaction.set_context_cap_override(cap);
    Some(compaction.token_budget() as u64)
}

fn inspect_messages(session: &Session, archive: bool, native: bool) -> Vec<Message> {
    let mut messages = session.messages_for_provider_uncached();
    if let Some(state) = session.compaction.as_deref() {
        let mut manager = CompactionManager::new();
        manager.restore_persisted_state(state, messages.len());
        messages = manager.inspect_provider_messages(session, &messages, archive, native);
    }
    Agent::bounded_outbound_images(messages)
}

#[derive(Default)]
struct Counts {
    prompt_present: bool,
    system_chars: Option<usize>,
    skill_chars: Option<usize>,
    builtin_tokens: Option<usize>,
    dynamic_tokens: Option<usize>,
    agent_tokens: Option<usize>,
    conversation_chars: Option<usize>,
    dynamic_reminder_chars: usize,
    unattributed_chars: usize,
}

impl Counts {
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

    fn messages(&mut self, session: &Session, messages: &[Message]) {
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
        for message in messages {
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
        limit_tokens: Option<u64>,
        reported: Option<u64>,
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
        let unknown = counts.iter().any(|(_, tokens)| tokens.is_none());
        let not_attributed = estimate(
            self.unattributed_chars
                + if self.dynamic_tokens.is_none() {
                    self.dynamic_reminder_chars
                } else {
                    0
                },
        );
        let attributed_total = counts.iter().filter_map(|(_, tokens)| *tokens).sum::<u64>();
        let lower_bound = attributed_total + not_attributed;
        // Last observed occupancy includes the prefix. Do not add it to the
        // categories. A smaller report may predate the current material.
        let total_tokens = reported
            .filter(|reported| *reported >= lower_bound)
            .or_else(|| {
                (self.prompt_present
                    && self.conversation_chars.is_some()
                    && self.builtin_tokens.is_some())
                .then_some(lower_bound)
            });
        let unattributed = total_tokens
            .map(|total| total - attributed_total)
            .filter(|&tokens| tokens > 0 || !unknown)
            .or_else(|| (not_attributed > 0).then_some(not_attributed));
        let categories = counts
            .into_iter()
            .chain([(Kind::Unattributed, unattributed)])
            .map(|(kind, tokens)| ContextBreakdownCategory { kind, tokens })
            .collect();
        ContextBreakdown {
            session_id,
            model,
            total_tokens,
            limit_tokens,
            estimated: true,
            categories,
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

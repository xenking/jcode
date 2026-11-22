use super::Agent;
use crate::logging;
use crate::message::{Message, ToolDefinition};
use crate::prompt::SplitSystemPrompt;

pub(super) enum StaticPromptState {
    Bootstrap(crate::prompt::StaticPromptSources),
    Frozen(crate::prompt::StaticPromptSnapshot),
}

impl StaticPromptState {
    pub(super) fn bootstrap(working_dir: Option<&std::path::Path>) -> Self {
        Self::Bootstrap(crate::prompt::StaticPromptSources::load(working_dir))
    }
}

impl Agent {
    /// Adopt the exact bytes this session sent, and its AGENTS.md and skill
    /// inputs. Legacy snapshots have only the latter and upgrade on next send.
    /// An explicit move to another directory keeps the existing recapture rule.
    pub(super) fn adopt_recorded_prompt_inputs(&mut self) -> bool {
        let Some(recorded) = crate::session::load_prompt_inputs(&self.session.id) else {
            return false;
        };
        if recorded.working_dir != self.session.working_dir {
            return false;
        }
        let info = crate::prompt::ContextInfo {
            has_project_agents_md: recorded.has_project_agents_md,
            project_agents_md_chars: recorded.project_agents_md_chars,
            has_global_agents_md: recorded.has_global_agents_md,
            global_agents_md_chars: recorded.global_agents_md_chars,
            ..Default::default()
        };
        self.agents_md_snapshot = (recorded.agents_md, info);
        self.prompt_skills_snapshot = recorded.skills;
        self.prompt_inputs_recorded = recorded.static_prompt.is_some();
        self.static_prompt = match recorded.static_prompt {
            Some(snapshot) => StaticPromptState::Frozen(snapshot),
            None => StaticPromptState::bootstrap(
                self.session
                    .working_dir
                    .as_deref()
                    .map(std::path::Path::new),
            ),
        };
        // Skills installed since then are announced in the transcript, which
        // leaves the cached system prefix untouched.
        self.announced_skills = self
            .prompt_skills_snapshot
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        self.announced_skills_scan_index = 0;
        true
    }

    /// Record the inputs of the system prompt about to be sent, once per
    /// capture, so [`Self::adopt_recorded_prompt_inputs`] can restore them.
    pub(super) fn record_prompt_inputs(&mut self) -> anyhow::Result<()> {
        if self.prompt_inputs_recorded || self.session.is_ephemeral() {
            return Ok(());
        }
        let StaticPromptState::Frozen(snapshot) = &self.static_prompt else {
            return Ok(());
        };
        let (agents_md, info) = &self.agents_md_snapshot;
        let inputs = crate::session::PersistedPromptInputs {
            working_dir: self.session.working_dir.clone(),
            agents_md: agents_md.clone(),
            has_project_agents_md: info.has_project_agents_md,
            project_agents_md_chars: info.project_agents_md_chars,
            has_global_agents_md: info.has_global_agents_md,
            global_agents_md_chars: info.global_agents_md_chars,
            skills: self.prompt_skills_snapshot.clone(),
            static_prompt: Some(snapshot.clone()),
        };
        crate::session::save_prompt_inputs(&self.session.id, &inputs)?;
        self.prompt_inputs_recorded = true;
        Ok(())
    }

    pub(crate) fn runtime_skills_snapshot(&self) -> serde_json::Value {
        let registry = self.current_skills_snapshot();
        let mut loaded = registry.list();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        let skills: Vec<_> = loaded
            .iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.name, "description": skill.description,
                    "path": skill.path, "allowed_tools": skill.allowed_tools,
                    "icon": skill.icon, "mode": skill.mode,
                })
            })
            .collect();
        let endorsed: Vec<_> = crate::skill::endorsed_skills().iter().map(|skill| serde_json::json!({
            "name": skill.name, "description": skill.description, "category": skill.category,
            "source": skill.source, "install": skill.install, "installed": registry.contains(skill.name),
        })).collect();
        serde_json::json!({"skills": skills, "endorsed": endorsed, "active_skill": self.active_skill, "pinned_skills": self.session_for_split().pinned_skill_names()})
    }

    pub(crate) fn runtime_agent_model_overrides(&self) -> serde_json::Value {
        serde_json::json!(self.session.agent_model_overrides)
    }

    pub(crate) async fn runtime_context_snapshot(&self) -> serde_json::Value {
        let mut context = self.debug_context().await;
        context["messages"] = serde_json::json!(self.session.visible_conversation_messages());
        context["context_window"] = serde_json::json!(self.provider_context_window());
        context
    }

    /// Explicitly prepare/freeze the same tool surface used by provider turns.
    /// Unlike `debug_context`, this may update the tool cache. It never calls a provider.
    pub async fn prepare_debug_context(&mut self) -> serde_json::Value {
        let prepared_tools = self.tool_definitions().await;
        let mut context = self.debug_context().await;
        context["prepared_tools"] = serde_json::json!(prepared_tools);
        context
    }

    /// Inspect the next request's static context without inference, prewarming,
    /// or locking a new tool snapshot. Pending memory is deliberately not consumed.
    pub async fn debug_context(&self) -> serde_json::Value {
        let prompt = self.build_system_prompt_split(None);
        let current_tools = self.tool_definitions_for_debug().await;
        let effective_tools = self.locked_tools.as_ref().unwrap_or(&current_tools);
        let locked_tool_names = self.locked_tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
        });
        serde_json::json!({
            "session_id": self.session.id,
            "working_dir": self.session.working_dir,
            "mode": if self.is_desktop_selfdev() { "desktop" }
                else if self.session.is_canary { "cli" } else { "regular" },
            "is_canary": self.session.is_canary,
            "system_prompt": {
                "static": prompt.static_part,
                "dynamic": prompt.dynamic_part,
                "pending_memory_included": false,
            },
            "tools_locked": self.locked_tools.is_some(),
            "locked_tool_names": locked_tool_names,
            "effective_tools": effective_tools,
            "current_tools": current_tools,
        })
    }

    pub(super) fn log_prompt_prefix_accounting(
        &self,
        split: &crate::prompt::SplitSystemPrompt,
        tools: &[ToolDefinition],
    ) {
        logging::debug_lazy(|| {
            let system_tokens = split.estimated_tokens();
            let tool_tokens = ToolDefinition::aggregate_prompt_token_estimate(tools);
            let prefix_tokens = system_tokens + tool_tokens;
            format!(
                "Prompt prefix estimate: total={prefix_tokens} tokens (system={system_tokens} tools={tool_tokens})"
            )
        });
    }

    pub(super) fn build_memory_prompt_nonblocking_shared(
        &self,
        messages: std::sync::Arc<[Message]>,
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        if !self.memory_enabled {
            return None;
        }

        let session_id = &self.session.id;

        let fresh_user_turn = crate::message::ends_with_fresh_user_turn(&messages);
        let pending = if fresh_user_turn {
            crate::memory::take_pending_memory_for_project(
                session_id,
                self.session.working_dir.as_deref(),
            )
        } else {
            None
        };

        // Use the persistent memory-agent pipeline as the single source of truth.
        // Running both this and the legacy MemoryManager background retrieval path
        // can prepare overlapping pending prompts for the same turn, which makes
        // memory injection feel overly aggressive.
        // Relevance results are consumed only at the start of a fresh user turn.
        // Enqueuing again after every tool result runs the local embedding model
        // for each provider continuation without creating an additional injection
        // opportunity. One update per user turn keeps memory current while avoiding
        // redundant 512-token inference during tool-heavy agent loops.
        if fresh_user_turn {
            crate::memory_agent::update_context_sync_with_dir(
                session_id,
                messages,
                self.session.working_dir.clone(),
            );
        }

        pending
    }

    /// Build split system prompt for better caching
    /// Keep live screen context separate so provider adapters can attach it
    /// without changing the stable instructions. Anthropic sends it at the tail.
    pub(super) fn build_system_prompt_split(
        &self,
        memory_prompt: Option<&str>,
    ) -> SplitSystemPrompt {
        self.system_prompt_with_accounting(memory_prompt, false).0
    }

    pub(super) fn system_prompt_with_accounting(
        &self,
        memory_prompt: Option<&str>,
        account_skills: bool,
    ) -> (SplitSystemPrompt, Option<usize>) {
        let (static_part, custom_prompt, skills_chars) = match &self.static_prompt {
            StaticPromptState::Frozen(snapshot) => (
                snapshot.text.clone(),
                snapshot.file_versions.is_none(),
                account_skills
                    .then(|| snapshot.skills_chars(&self.prompt_skills_snapshot))
                    .flatten(),
            ),
            StaticPromptState::Bootstrap(sources) => {
                let (text, skills_chars) = self.render_static_prompt(sources);
                (
                    text,
                    self.session.system_prompt.is_some(),
                    Some(skills_chars),
                )
            }
        };
        let mut dynamic_part = match (custom_prompt, memory_prompt) {
            (false, Some(memory)) => memory.to_owned(),
            _ => String::new(),
        };
        dynamic_part.push_str(&crate::screen_context::dynamic_prompt(self.session_id()));
        (
            SplitSystemPrompt {
                static_part,
                dynamic_part,
            },
            skills_chars,
        )
    }

    fn render_static_prompt(
        &self,
        sources: &crate::prompt::StaticPromptSources,
    ) -> (String, usize) {
        if let Some(ref override_prompt) = self.session.system_prompt {
            return (
                self.append_session_system_prompt(override_prompt.clone()),
                0,
            );
        }
        let working_dir = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new);
        let (split, info) = crate::prompt::build_system_prompt_split_with_sources(
            None,
            &self.prompt_skills_snapshot,
            self.session.is_canary,
            None,
            working_dir,
            sources,
            self.agents_md_snapshot.clone(),
        );
        (
            self.append_session_system_prompt(split.static_part),
            info.skills_chars,
        )
    }

    /// Finalize at the first real request, after the SDK has had a chance to
    /// supply creation options. Debug inspection and speculative warmup do not
    /// commit the prefix.
    pub(super) fn freeze_static_prompt(&mut self) {
        if let StaticPromptState::Bootstrap(sources) = &self.static_prompt {
            let (text, skills_chars) = self.render_static_prompt(sources);
            self.static_prompt = StaticPromptState::Frozen(crate::prompt::StaticPromptSnapshot {
                text,
                file_versions: self
                    .session
                    .system_prompt
                    .is_none()
                    .then(|| sources.files.versions()),
                skills_chars: Some(skills_chars),
            });
        }
    }

    pub(super) fn announce_prompt_file_changes(&mut self) -> anyhow::Result<()> {
        let StaticPromptState::Frozen(snapshot) = &self.static_prompt else {
            return Ok(());
        };
        let Some(previous) = snapshot.file_versions.as_ref() else {
            return Ok(());
        };
        let working_dir = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new);
        let files = crate::prompt::PromptFiles::load(working_dir);
        let Some((reminder, versions)) = files.change_reminder(previous) else {
            return Ok(());
        };
        self.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: reminder,
                cache_control: None,
            }],
            Some(crate::session::StoredDisplayRole::System),
        );
        // Persist the model-visible message before committing its baseline.
        self.session.save()?;
        if let StaticPromptState::Frozen(snapshot) = &mut self.static_prompt {
            snapshot.file_versions = Some(versions);
        }
        self.prompt_inputs_recorded = false;
        self.record_prompt_inputs()
    }

    pub(super) fn is_jarvis_root(&self, config: &crate::protocol::SessionToolConfig) -> bool {
        self.session.parent_id.is_none()
            && config.custom.iter().any(|tool| tool.name == "speak")
            && config
                .system_prompt_append
                .as_deref()
                .is_some_and(|p| p.lines().any(|line| line == "## Jarvis voice interaction"))
    }

    fn append_session_system_prompt(&self, mut prompt: String) -> String {
        if let Some(config) = crate::tool::sdk::config(self.session_id())
            && let Some(append) = &config.system_prompt_append
        {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(append);
            if self.is_jarvis_root(&config) {
                prompt.push_str("\n\n");
                prompt.push_str(include_str!("jarvis_coordinator.md").trim());
            }
        }
        prompt
    }

    /// Non-blocking memory prompt - takes pending result and spawns check for next turn
    #[cfg(test)]
    pub(super) fn build_memory_prompt_nonblocking(
        &self,
        messages: &[Message],
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        self.build_memory_prompt_nonblocking_shared(messages.to_vec().into(), _memory_event_tx)
    }
}

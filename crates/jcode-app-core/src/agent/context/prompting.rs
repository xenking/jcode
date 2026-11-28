use super::*;
use crate::logging;
use crate::message::{Message, ToolDefinition};
use crate::prompt::SplitSystemPrompt;

impl Agent {
    /// Adopt the exact bytes this session sent, and its AGENTS.md and skill
    /// inputs. Legacy snapshots have only the latter and upgrade on next send.
    /// An explicit move to another directory keeps the existing recapture rule.
    pub(super) fn adopt_recorded_prompt_inputs(&mut self) -> bool {
        let Some(recorded) = crate::session::load_prompt_inputs(self.session.id()) else {
            return false;
        };
        if recorded.working_dir != self.session.working_dir().map(str::to_owned) {
            return false;
        }
        self.prompt.adopt(
            recorded,
            self.session.working_dir().map(std::path::Path::new),
        );
        true
    }

    /// Record the inputs of the system prompt about to be sent, once per
    /// capture, so [`Self::adopt_recorded_prompt_inputs`] can restore them.
    pub(super) fn record_prompt_inputs(&mut self) -> anyhow::Result<()> {
        if self.session.is_ephemeral() {
            return Ok(());
        }
        let Some(inputs) = self
            .prompt
            .unrecorded_inputs(self.session.working_dir().map(str::to_owned))
        else {
            return Ok(());
        };
        crate::session::save_prompt_inputs(self.session.id(), &inputs)?;
        self.prompt.mark_recorded();
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
                    "icon": skill.icon, "mode": skill.is_mode(),
                })
            })
            .collect();
        let endorsed: Vec<_> = crate::skill::endorsed_skills().iter().map(|skill| serde_json::json!({
            "name": skill.name, "description": skill.description, "category": skill.category,
            "source": skill.source, "install": skill.install, "installed": registry.contains(skill.name),
        })).collect();
        serde_json::json!({"skills": skills, "endorsed": endorsed, "active_skill": self.active_skill, "pinned_skills": self.session.pinned_skill_names()})
    }

    pub(crate) fn runtime_agent_model_overrides(&self) -> serde_json::Value {
        serde_json::json!(self.session.agent_model_overrides())
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
        let locked = self.tool_lock.request_snapshot();
        let effective_tools = locked.as_deref().unwrap_or(&current_tools);
        let locked_tool_names = locked.as_deref().map(|tools| {
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
        });
        serde_json::json!({
            "session_id": self.session.id(),
            "working_dir": self.session.working_dir(),
            "mode": if self.is_desktop_selfdev() { "desktop" }
                else if self.session.is_canary() { "cli" } else { "regular" },
            "is_canary": self.session.is_canary(),
            "system_prompt": {
                "static": prompt.static_part,
                "dynamic": prompt.dynamic_part,
                "pending_memory_included": false,
            },
            "tools_locked": self.tool_lock.is_locked(),
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
            let family = jcode_message_types::TokenFamily::from_model(&self.provider_model());
            let system_tokens = split.estimated_tokens(family);
            let tool_tokens = ToolDefinition::aggregate_prompt_token_estimate(tools, family);
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

        let session_id = self.session.id();

        let fresh_user_turn = crate::message::ends_with_fresh_user_turn(&messages);
        let pending = if fresh_user_turn {
            crate::memory::take_pending_memory_for_project(session_id, self.session.working_dir())
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
                self.session.working_dir().map(str::to_owned),
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
        let (static_part, custom_prompt, skills_chars) = self
            .prompt
            .frozen_prompt(account_skills)
            .unwrap_or_else(|| {
                let sources = self
                    .prompt
                    .bootstrap_sources()
                    .expect("unfrozen prompt has bootstrap sources");
                let (text, skills_chars) = self.render_static_prompt(sources);
                (
                    text,
                    self.session.system_prompt().is_some(),
                    Some(skills_chars),
                )
            });
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
        if let Some(override_prompt) = self.session.system_prompt() {
            return (
                self.append_session_system_prompt(override_prompt.to_string()),
                0,
            );
        }
        let working_dir = self.session.working_dir().map(std::path::Path::new);
        let session_id = self.session_id().to_string();
        // A line about a tool stays only when the session may call it and the
        // tool exists here (opt-in tools such as codemode or advisor).
        let registry = &self.registry;
        let tool_gate = move |name: &str| {
            crate::tool::session_allows_tool(&session_id, name)
                && registry.try_has_tool(name).unwrap_or(true)
        };
        let profile = self.profile_prompt();
        let model_prompt = self.model_prompt();
        let composition = crate::prompt::PromptComposition {
            tool_gate: Some(&tool_gate),
            profile: profile.as_ref(),
            model_prompt: Some(&model_prompt),
        };
        let (text, skills_chars) =
            self.prompt
                .render(sources, self.session.is_canary(), working_dir, &composition);
        (self.append_session_system_prompt(text), skills_chars)
    }

    /// `[[agents.model_prompts]]` for the model this session runs on now.
    pub(super) fn model_prompt(&self) -> crate::model_prompt::ModelPrompt {
        crate::model_prompt::resolve(
            &crate::config::config().agents,
            self.provider.name(),
            &self.provider.model(),
        )
    }

    /// Finalize at the first real request, after the SDK has had a chance to
    /// supply creation options. Debug inspection and speculative warmup do not
    /// commit the prefix.
    pub(super) fn freeze_static_prompt(&mut self) {
        self.rerender_after_model_prompt_change();
        if let Some(sources) = self.prompt.bootstrap_sources() {
            let (text, skills_chars) = self.render_static_prompt(sources);
            let snapshot = crate::prompt::StaticPromptSnapshot {
                text,
                file_versions: self
                    .session
                    .system_prompt()
                    .is_none()
                    .then(|| sources.files.versions()),
                skills_chars: Some(skills_chars),
                model_prompt_key: self.model_prompt().key,
                rendered_for_model: self.provider.model(),
            };
            self.prompt.freeze(snapshot);
        }
    }

    /// A model switch already starts a new provider cache, so when it changes
    /// which model prompt rules match, the prefix is rendered again for the
    /// new model. A switch within the same rules keeps the frozen bytes, and
    /// a config edit alone never changes a running session.
    fn rerender_after_model_prompt_change(&mut self) {
        let Some(snapshot) = self.prompt.frozen() else {
            return;
        };
        if self.session.system_prompt().is_some()
            || snapshot.rendered_for_model.is_empty()
            || snapshot.rendered_for_model == self.provider.model()
            || snapshot.model_prompt_key == self.model_prompt().key
        {
            return;
        }
        self.prompt
            .rebootstrap(self.session.working_dir().map(std::path::Path::new));
    }

    pub(super) fn announce_prompt_file_changes(&mut self) -> anyhow::Result<()> {
        let Some(snapshot) = self.prompt.frozen() else {
            return Ok(());
        };
        let Some(previous) = snapshot.file_versions.as_ref() else {
            return Ok(());
        };
        let working_dir = self.session.working_dir().map(std::path::Path::new);
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
        self.prompt.set_file_versions(versions);
        self.record_prompt_inputs()
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

// Pinned skills and modes: which are active for a session, how a client's
// selection is reconciled with it, and how a change is announced to the model.
impl Agent {
    /// Synchronize the remote client's selected skill, accepting only names
    /// present in the daemon's own registry snapshot.
    pub(crate) fn set_remote_active_skill(&mut self, active_skill: Option<String>) -> bool {
        match active_skill {
            // Clients report only the primary pinned skill. When the session
            // already pins it, together with other modes, keep the whole set.
            Some(name) if self.session.pinned_skill_names().contains(&name) => true,
            Some(name) => self.set_pinned_skill(Some(&name)).is_ok(),
            None => true,
        }
    }

    pub(crate) fn set_pinned_skill(&mut self, name: Option<&str>) -> Result<()> {
        self.set_pinned_skills(name.map(str::to_owned).into_iter().collect())
    }

    pub(crate) fn invoke_pinned_skill(&mut self, name: &str, arguments: &str) -> Result<()> {
        if self.current_skills_snapshot().get(name).is_none() {
            anyhow::bail!(crate::skill::unknown_skill_message(name));
        }
        let previous = self.active_skill_announcement();
        self.begin_setting_change();
        self.session.set_pinned_skill_invocation(name, arguments);
        self.refresh_profile_tool_policy();
        self.active_skill = Some(name.to_owned());
        if self.active_skill_announcement() != previous {
            self.announce_active_skill_inner();
        }
        self.log_setting_changes("a skill invocation pinned it");
        self.session.save()?;
        Ok(())
    }

    pub(super) fn set_pinned_skills(&mut self, next: Vec<String>) -> Result<()> {
        let skills = self.current_skills_snapshot();
        if let Some(missing) = next.iter().find(|name| skills.get(name).is_none()) {
            anyhow::bail!(crate::skill::unknown_skill_message(missing));
        }
        if self.session.pinned_skill_names() == next && self.active_skill.as_ref() == next.first() {
            return Ok(());
        }
        self.begin_setting_change();
        self.session.set_pinned_skill_names(next);
        self.refresh_profile_tool_policy();
        self.active_skill = self.session.pinned_skill().map(str::to_owned);
        if self.active_skill.is_some() {
            self.announce_active_skill();
        } else {
            self.add_message_with_display_role(
                Role::User,
                vec![ContentBlock::Text {
                    text: format!(
                        "<system-reminder>\n{}\n</system-reminder>",
                        crate::skill::MODE_OFF_REMINDER
                    ),
                    cache_control: None,
                }],
                Some(StoredDisplayRole::System),
            );
        }
        self.log_setting_changes(SettingCause::User);
        self.session.save()?;
        Ok(())
    }

    pub(crate) fn inherit_pinned_skill(&mut self, parent: &Session) -> Result<()> {
        self.session.inherit_pinned_skill(parent);
        self.refresh_profile_tool_policy();
        self.restore_pinned_skill();
        self.session.save()?;
        Ok(())
    }

    pub(crate) fn mode_command(&mut self, argument: &str) -> Result<String> {
        match argument.trim() {
            "" => {}
            "off" => self.set_pinned_skill(None)?,
            toggle if toggle.starts_with("toggle ") => {
                let name = toggle.trim_start_matches("toggle ");
                let mut names = self.session.pinned_skill_names().to_vec();
                if let Some(index) = names.iter().position(|active| active == name) {
                    names.remove(index);
                } else {
                    names.push(name.to_owned());
                }
                self.set_pinned_skills(names)?;
            }
            name => self.set_pinned_skill(Some(name))?,
        }
        Ok(format!(
            "Session mode: {}",
            if self.session.pinned_skill_names().is_empty() {
                "off".into()
            } else {
                self.session.pinned_skill_names().join(", ")
            }
        ))
    }

    pub(super) fn restore_pinned_skill(&mut self) {
        self.active_skill = self.session.pinned_skill().map(str::to_owned);
        let Some(announcement) = self.active_skill_announcement() else {
            return;
        };
        let start = self
            .session
            .compaction()
            .as_ref()
            .map_or(0, |state| state.compacted_count);
        let announced = self
            .session
            .iter_messages()
            .skip(start)
            .rev()
            .flat_map(|m| &m.content)
            .find_map(|block| match block {
                ContentBlock::Text { text, .. }
                    if text.starts_with("<system-reminder>\n# Active Skill\n") =>
                {
                    Some(text)
                }
                _ => None,
            })
            .is_some_and(|text| text == &announcement);
        if !announced {
            self.announce_active_skill();
        }
    }

    pub(super) fn set_queued_active_skill(&mut self, active_skill: Option<String>) -> bool {
        let Some(name) = active_skill else {
            return true;
        };
        if self.current_skills_snapshot().get(&name).is_none() {
            return false;
        }
        if !self.session.pinned_skill_names().contains(&name) {
            self.session.set_pinned_skill_names(vec![name.clone()]);
            self.refresh_profile_tool_policy();
        }
        self.active_skill = Some(name);
        self.announce_active_skill_inner()
    }

    pub(super) fn active_skill_announcement(&self) -> Option<String> {
        self.active_skill.as_ref()?;
        let skills = self.current_skills_snapshot();
        Session::render_pinned_skill_announcement(self.session.pinned_skill_names(), |name| {
            skills.get(name).map(|skill| {
                skill.render_prompt(
                    self.session.pinned_skill_arguments(name),
                    self.session.working_dir().map(std::path::Path::new),
                )
            })
        })
        .or_else(|| {
            self.session
                .stored_pinned_skill_announcement()
                .map(str::to_owned)
        })
    }

    /// Record the newly active skill's instructions as a hidden transcript
    /// message. The system prompt stays byte-identical across skill changes.
    pub(crate) fn announce_active_skill(&mut self) {
        if self.announce_active_skill_inner() {
            self.persist_session_best_effort("active skill announcement");
        }
    }

    pub(super) fn announce_active_skill_inner(&mut self) -> bool {
        let Some(text) = self.active_skill_announcement() else {
            return false;
        };
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
        true
    }
}

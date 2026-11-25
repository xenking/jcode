//! The system-prompt inputs an agent froze for its session, and the skills
//! the model has been told about since.
//!
//! The static system prompt, the AGENTS.md text and the "Available Skills"
//! list are session bootstrap input: rewriting any of them mid-session forces
//! a KV-cache miss on the whole conversation. This layer owns the frozen
//! copies and the record of what has been announced instead. It knows nothing
//! of the provider, the registry or the live session: the agent hands in the
//! working directory and the transcript, and renders the prompt itself.

use crate::message::ContentBlock;
use crate::prompt::{ContextInfo, SkillInfo, StaticPromptSnapshot, StaticPromptSources};
use crate::session::{PersistedPromptInputs, StoredMessage};
use std::collections::HashSet;
use std::path::Path;

/// Stable header of the late-skill transcript announcement; used to recognize
/// earlier announcements when a session is restored.
pub(in crate::agent) const LATE_SKILL_ANNOUNCEMENT_MARKER: &str = "New skills were installed.";

pub(in crate::agent) enum StaticPromptState {
    Bootstrap(StaticPromptSources),
    Frozen(StaticPromptSnapshot),
}

impl StaticPromptState {
    pub(in crate::agent) fn bootstrap(working_dir: Option<&Path>) -> Self {
        Self::Bootstrap(StaticPromptSources::load(working_dir))
    }
}

pub(in crate::agent) struct PromptContext {
    /// AGENTS.md is session bootstrap input. The captured text stays stable so
    /// tool writes do not mutate the provider's cacheable prefix mid-session.
    agents_md: (Option<String>, ContextInfo),
    /// The "Available Skills" system-prompt section, frozen so installing a
    /// skill mid-session does not rewrite the cached system prefix. Later
    /// installs are announced in the transcript.
    skills: Vec<SkillInfo>,
    static_prompt: StaticPromptState,
    /// Skill names already described to the model, either in the frozen
    /// system-prompt list or by a late-skill transcript announcement.
    announced_skills: HashSet<String>,
    /// Transcript index already scanned for late-skill announcements.
    announced_skills_scanned: usize,
    /// Whether the current prompt inputs have been recorded for reload.
    inputs_recorded: bool,
}

impl PromptContext {
    pub(in crate::agent) fn new(
        agents_md: (Option<String>, ContextInfo),
        static_prompt: StaticPromptState,
    ) -> Self {
        Self {
            agents_md,
            skills: Vec::new(),
            static_prompt,
            announced_skills: HashSet::new(),
            announced_skills_scanned: 0,
            inputs_recorded: false,
        }
    }

    pub(in crate::agent) fn agents_md(&self) -> &(Option<String>, ContextInfo) {
        &self.agents_md
    }

    pub(in crate::agent) fn skills(&self) -> &[SkillInfo] {
        &self.skills
    }

    pub(in crate::agent) fn static_prompt(&self) -> &StaticPromptState {
        &self.static_prompt
    }

    /// The not-yet-sent static prompt inputs, while the prompt is still being
    /// bootstrapped.
    pub(in crate::agent) fn bootstrap_sources(&self) -> Option<&StaticPromptSources> {
        match &self.static_prompt {
            StaticPromptState::Bootstrap(sources) => Some(sources),
            StaticPromptState::Frozen(_) => None,
        }
    }

    /// The exact static prompt this session sent, once it has been sent.
    pub(in crate::agent) fn frozen(&self) -> Option<&StaticPromptSnapshot> {
        match &self.static_prompt {
            StaticPromptState::Frozen(snapshot) => Some(snapshot),
            StaticPromptState::Bootstrap(_) => None,
        }
    }

    pub(in crate::agent) fn freeze(&mut self, snapshot: StaticPromptSnapshot) {
        self.static_prompt = StaticPromptState::Frozen(snapshot);
    }

    /// Start over from the files in `working_dir`; the next send captures them.
    pub(in crate::agent) fn rebootstrap(&mut self, working_dir: Option<&Path>) {
        self.static_prompt = StaticPromptState::bootstrap(working_dir);
        self.inputs_recorded = false;
    }

    /// Replace the captured AGENTS.md text and restart the static prompt from
    /// `working_dir`.
    pub(in crate::agent) fn reset_sources(
        &mut self,
        agents_md: (Option<String>, ContextInfo),
        working_dir: Option<&Path>,
    ) {
        self.agents_md = agents_md;
        self.static_prompt = StaticPromptState::bootstrap(working_dir);
        self.inputs_recorded = false;
    }

    /// Re-capture the frozen skills list. Only called at session boundaries
    /// (new/restored session, working directory change), never mid-turn.
    pub(in crate::agent) fn set_skills(&mut self, skills: Vec<SkillInfo>) {
        self.announced_skills = skills.iter().map(|skill| skill.name.clone()).collect();
        self.skills = skills;
        self.announced_skills_scanned = 0;
    }

    /// Adopt the exact bytes a session sent, and its AGENTS.md and skill
    /// inputs. Legacy snapshots have only the latter and upgrade on next send.
    pub(in crate::agent) fn adopt(
        &mut self,
        recorded: PersistedPromptInputs,
        working_dir: Option<&Path>,
    ) {
        let info = ContextInfo {
            has_project_agents_md: recorded.has_project_agents_md,
            project_agents_md_chars: recorded.project_agents_md_chars,
            has_global_agents_md: recorded.has_global_agents_md,
            global_agents_md_chars: recorded.global_agents_md_chars,
            ..Default::default()
        };
        self.agents_md = (recorded.agents_md, info);
        self.skills = recorded.skills;
        self.inputs_recorded = recorded.static_prompt.is_some();
        self.static_prompt = match recorded.static_prompt {
            Some(snapshot) => StaticPromptState::Frozen(snapshot),
            None => StaticPromptState::bootstrap(working_dir),
        };
        // Skills installed since then are announced in the transcript, which
        // leaves the cached system prefix untouched.
        self.announced_skills = self.skills.iter().map(|skill| skill.name.clone()).collect();
        self.announced_skills_scanned = 0;
    }

    /// The inputs to persist for a later reload, until they have been recorded.
    pub(in crate::agent) fn unrecorded_inputs(
        &self,
        working_dir: Option<String>,
    ) -> Option<PersistedPromptInputs> {
        if self.inputs_recorded {
            return None;
        }
        let StaticPromptState::Frozen(snapshot) = &self.static_prompt else {
            return None;
        };
        let (agents_md, info) = &self.agents_md;
        Some(PersistedPromptInputs {
            working_dir,
            agents_md: agents_md.clone(),
            has_project_agents_md: info.has_project_agents_md,
            project_agents_md_chars: info.project_agents_md_chars,
            has_global_agents_md: info.has_global_agents_md,
            global_agents_md_chars: info.global_agents_md_chars,
            skills: self.skills.clone(),
            static_prompt: Some(snapshot.clone()),
        })
    }

    pub(in crate::agent) fn mark_recorded(&mut self) {
        self.inputs_recorded = true;
    }

    /// A prompt file changed: commit its new baseline and require the inputs
    /// to be recorded again.
    pub(in crate::agent) fn set_file_versions(
        &mut self,
        versions: crate::prompt::PromptFileVersions,
    ) {
        if let StaticPromptState::Frozen(snapshot) = &mut self.static_prompt {
            snapshot.file_versions = Some(versions);
        }
        self.inputs_recorded = false;
    }

    pub(in crate::agent) fn is_skill_announced(&self, name: &str) -> bool {
        self.announced_skills.contains(name)
    }

    pub(in crate::agent) fn announce_skill(&mut self, name: String) {
        self.announced_skills.insert(name);
    }

    /// Record that the transcript up to `len` messages has been scanned for
    /// late-skill announcements.
    pub(in crate::agent) fn mark_skills_scanned(&mut self, len: usize) {
        self.announced_skills_scanned = len;
    }

    /// Mark skills named in earlier announcements as announced, so a
    /// restored session does not describe them again.
    pub(in crate::agent) fn seed_announced_skills_from_transcript<'a>(
        &mut self,
        len: usize,
        messages_from: impl FnOnce(usize) -> std::slice::Iter<'a, StoredMessage>,
    ) {
        let start = self.announced_skills_scanned.min(len);
        for message in messages_from(start) {
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
        self.announced_skills_scanned = len;
    }

    #[cfg(test)]
    pub(in crate::agent) fn set_agents_md_text(&mut self, text: Option<String>) {
        self.agents_md.0 = text;
    }

    /// Simulate a restored agent whose in-memory announcements start empty.
    #[cfg(test)]
    pub(in crate::agent) fn forget_announced_skills_except(&mut self, names: &[&str]) {
        self.announced_skills = names.iter().map(|name| (*name).to_string()).collect();
        self.announced_skills_scanned = 0;
    }
}

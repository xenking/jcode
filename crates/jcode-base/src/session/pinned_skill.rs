use super::*;

impl Session {
    /// Keep inherited instructions usable when a worker has another project
    /// directory, or the skill's source file is no longer installed.
    pub fn stored_pinned_skill_announcement(&self) -> Option<&str> {
        self.pinned_skill.as_ref()?;
        self.messages
            .iter()
            .rev()
            .filter(|message| message.display_role == Some(StoredDisplayRole::System))
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::Text { text, .. }
                    if text.starts_with("<system-reminder>\n# Active Skill\n\n")
                        && !text.contains(crate::skill::MODE_OFF_REMINDER) =>
                {
                    Some(text.as_str())
                }
                _ => None,
            })
    }

    pub fn inherit_pinned_skill(&mut self, parent: &Session) {
        let Some(name) = parent.pinned_skill.as_deref() else {
            return;
        };
        self.pinned_skill = Some(name.to_owned());
        let global = crate::skill::SkillRegistry::shared_snapshot();
        let child_skills = crate::skill::SkillRegistry::effective_for_working_dir(
            &global,
            self.working_dir.as_deref().map(Path::new),
        );
        let parent_skills = crate::skill::SkillRegistry::effective_for_working_dir(
            &global,
            parent.working_dir.as_deref().map(Path::new),
        );
        let announcement = child_skills
            .get(name)
            .or_else(|| parent_skills.get(name))
            .map(|skill| {
                format!(
                    "<system-reminder>\n# Active Skill\n\n{}\n</system-reminder>",
                    skill.get_prompt().trim()
                )
            })
            .or_else(|| parent.stored_pinned_skill_announcement().map(str::to_owned));
        if let Some(text) = announcement {
            self.add_message_with_display_role(
                Role::User,
                vec![ContentBlock::Text {
                    text,
                    cache_control: None,
                }],
                Some(StoredDisplayRole::System),
            );
        }
    }
}

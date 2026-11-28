use super::*;

impl Session {
    pub fn pinned_skill_names(&self) -> &[String] {
        if self.pinned_skills.is_empty() {
            self.pinned_skill.as_slice()
        } else {
            &self.pinned_skills
        }
    }

    pub fn set_pinned_skill_names(&mut self, names: Vec<String>) {
        self.pinned_skill = names.first().cloned();
        self.pinned_skill_arguments
            .retain(|name, _| names.contains(name));
        self.pinned_skills = names;
    }

    pub fn pinned_skill_arguments(&self, name: &str) -> &str {
        self.pinned_skill_arguments
            .get(name)
            .map_or("", String::as_str)
    }

    pub fn set_pinned_skill_invocation(&mut self, name: &str, arguments: &str) {
        self.set_pinned_skill_names(vec![name.to_owned()]);
        if arguments.is_empty() {
            self.pinned_skill_arguments.remove(name);
        } else {
            self.pinned_skill_arguments
                .insert(name.to_owned(), arguments.to_owned());
        }
    }

    pub fn render_pinned_skill_announcement(
        names: &[String],
        mut prompt: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        let bodies = names
            .iter()
            .map(|name| prompt(name).map(|body| (name, body)))
            .collect::<Option<Vec<_>>>()?;
        match bodies.as_slice() {
            [] => None,
            [(name, body)] => Some(format!(
                "<system-reminder>\n# Active Skill\n\nActive mode: {name}. This replaces all earlier mode instructions.\n\n{}\n</system-reminder>",
                body.trim()
            )),
            _ => {
                let roster = bodies
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let sections = bodies
                    .iter()
                    .map(|(name, body)| format!("## {name}\n\n{}", body.trim()))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Some(format!(
                    "<system-reminder>\n# Active Skill\n\nActive modes: {roster}. This complete list replaces all earlier mode instructions.\n\n{sections}\n</system-reminder>"
                ))
            }
        }
    }
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
        let names = parent.pinned_skill_names();
        if names.is_empty() {
            return;
        }
        self.set_pinned_skill_names(names.to_vec());
        self.pinned_skill_arguments
            .clone_from(&parent.pinned_skill_arguments);
        let global = crate::skill::SkillRegistry::shared_snapshot();
        let child_skills = crate::skill::SkillRegistry::effective_for_working_dir(
            &global,
            self.working_dir.as_deref().map(Path::new),
        );
        let parent_skills = crate::skill::SkillRegistry::effective_for_working_dir(
            &global,
            parent.working_dir.as_deref().map(Path::new),
        );
        let announcement = Self::render_pinned_skill_announcement(names, |name| {
            child_skills
                .get(name)
                .or_else(|| parent_skills.get(name))
                .map(|skill| {
                    skill.render_prompt(
                        self.pinned_skill_arguments(name),
                        self.working_dir.as_deref().map(Path::new),
                    )
                })
        })
        .or_else(|| parent.stored_pinned_skill_announcement().map(str::to_owned))
        .or_else(|| {
            // Spawn normally uses a metadata-only parent. Read its transcript
            // only when the installed sources cannot reconstruct the mode.
            if !parent.persist_state.transcript_stripped {
                return None;
            }
            let saved_parent = Self::load(&parent.id).ok()?;
            saved_parent
                .stored_pinned_skill_announcement()
                .map(str::to_owned)
        });
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

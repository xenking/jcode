//! Pinned skills and modes: which are active for a session, how a client's
//! selection is reconciled with it, and how a change is announced to the model.

use super::*;

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
        self.session.set_pinned_skill_invocation(name, arguments);
        self.active_skill = Some(name.to_owned());
        if self.active_skill_announcement() != previous {
            self.announce_active_skill_inner();
        }
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
        self.session.set_pinned_skill_names(next);
        self.active_skill = self.session.pinned_skill.clone();
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
        self.session.save()?;
        Ok(())
    }

    pub(crate) fn inherit_pinned_skill(&mut self, parent: &Session) -> Result<()> {
        self.session.inherit_pinned_skill(parent);
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
        self.active_skill = self.session.pinned_skill.clone();
        let Some(announcement) = self.active_skill_announcement() else {
            return;
        };
        let start = self
            .session
            .compaction
            .as_ref()
            .map_or(0, |state| state.compacted_count);
        let announced = self
            .session
            .messages
            .iter()
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
                    self.session
                        .working_dir
                        .as_deref()
                        .map(std::path::Path::new),
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

use super::App;

impl App {
    pub(super) fn activate_pinned_skill(
        &mut self,
        skill: &crate::skill::Skill,
        arguments: Option<&str>,
    ) {
        let name = &skill.name;
        let names = vec![name.clone()];
        let project = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new);
        let previous_arguments = self.session.pinned_skill_arguments(name);
        let previous = skill.render_prompt(previous_arguments, project);
        let prompt = skill.render_prompt(arguments.unwrap_or(previous_arguments), project);
        let changed = self.session.pinned_skill_names() != names || prompt != previous;
        if let Some(arguments) = arguments {
            self.session.set_pinned_skill_invocation(name, arguments);
        } else {
            self.session.set_pinned_skill_names(names);
        }
        self.active_skill = Some(name.clone());
        if changed && !self.is_remote {
            self.append_turn_system_reminder(Some(format!("# Active Skill\n\n{}", prompt.trim())));
        }
        self.session_save_pending = true;
    }

    pub(in crate::tui::app) fn reannounce_pinned_skill(&mut self) {
        if self.is_remote {
            return;
        }
        self.active_skill = self.session.pinned_skill.clone();
        let skills = self.current_skills_snapshot();
        let rendered = crate::session::Session::render_pinned_skill_announcement(
            self.session.pinned_skill_names(),
            |name| {
                skills.get(name).map(|skill| {
                    skill.render_prompt(
                        self.session.pinned_skill_arguments(name),
                        self.session
                            .working_dir
                            .as_deref()
                            .map(std::path::Path::new),
                    )
                })
            },
        );
        let reminder = rendered
            .as_deref()
            .or_else(|| self.session.stored_pinned_skill_announcement())
            .and_then(|text| text.strip_prefix("<system-reminder>\n"))
            .and_then(|text| text.strip_suffix("\n</system-reminder>"))
            .map(str::to_owned);
        self.append_turn_system_reminder(reminder);
    }
}

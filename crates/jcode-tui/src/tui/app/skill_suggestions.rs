//! Skill and session-mode rows of the command palette.
//!
//! A mode is a skill pinned to the session. `/mode ` lists every skill, modes
//! first, each toggleable, so the user never has to remember which skills are
//! modes. The same details feed the plain `/skill` rows of the palette.

use super::App;
use crate::tui::CommandSuggestionInfo;

/// Help shown for a row whose skill details are unavailable.
const TOGGLE_HELP: &str = "Toggle session mode";

impl App {
    /// Skills known to this client, as `(name, description, is_mode)`.
    ///
    /// A remote client may know names the local registry does not. Those keep
    /// the name-based mode rule and an empty description.
    fn known_skills(&self) -> Vec<(String, String, bool)> {
        let snapshot = self.current_skills_snapshot();
        let mut skills: Vec<(String, String, bool)> = snapshot
            .list()
            .into_iter()
            .map(|skill| {
                (
                    skill.name.clone(),
                    skill.description.clone(),
                    skill.is_mode(),
                )
            })
            .collect();
        if self.is_remote {
            for name in &self.remote_skills {
                if !skills.iter().any(|(known, _, _)| known == name) {
                    skills.push((
                        name.clone(),
                        String::new(),
                        crate::skill::name_implies_mode(name),
                    ));
                }
            }
        }
        skills.sort_by(|a, b| a.0.cmp(&b.0));
        skills
    }

    /// Rows for `/mode <query>`: pinned modes, then other modes, then plain
    /// skills, each as `/mode toggle <name>`, followed by `/mode off` when
    /// anything is pinned. Matching is a case-insensitive substring of the name.
    pub(super) fn mode_picker_suggestions(&self, input: &str) -> Vec<(String, &'static str)> {
        let query = input
            .trim_start()
            .strip_prefix("/mode")
            .unwrap_or_default()
            .trim();
        let query = if query == "toggle" {
            ""
        } else {
            query.strip_prefix("toggle ").map_or(query, str::trim)
        }
        .to_lowercase();
        let pinned = self.session.pinned_skill_names();
        let mut rows: Vec<(u8, String)> = self
            .known_skills()
            .into_iter()
            .filter(|(name, _, _)| name.to_lowercase().contains(&query))
            .map(|(name, _, mode)| {
                let rank = if pinned.contains(&name) {
                    0
                } else if mode {
                    1
                } else {
                    2
                };
                (rank, name)
            })
            .collect();
        // Stable: names are already sorted, so each group stays alphabetical.
        rows.sort_by_key(|(rank, _)| *rank);
        let mut suggestions: Vec<(String, &'static str)> = rows
            .into_iter()
            .map(|(_, name)| (format!("/mode toggle {name}"), TOGGLE_HELP))
            .collect();
        if !pinned.is_empty() && "off".contains(&query) {
            suggestions.push(("/mode off".into(), "Clear all active modes"));
        }
        suggestions
    }

    /// Details for a palette row that names a skill, by its command text.
    pub(crate) fn skill_suggestion_info(&self, command: &str) -> Option<CommandSuggestionInfo> {
        let registered = |command: &str| {
            super::state_ui_input_helpers::registered_command_names().any(|name| name == command)
        };
        let name = command
            .strip_prefix("/mode toggle ")
            .or_else(|| command.strip_prefix("/mode ").filter(|name| *name != "off"))
            .or_else(|| command.strip_prefix('/').filter(|_| !registered(command)))?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return None;
        }
        let snapshot = self.current_skills_snapshot();
        let (description, mode) = match snapshot.get(name) {
            Some(skill) => (skill.description.clone(), skill.is_mode()),
            None if self.is_remote && self.remote_skills.iter().any(|known| known == name) => {
                (String::new(), crate::skill::name_implies_mode(name))
            }
            None => return None,
        };
        Some(CommandSuggestionInfo {
            description,
            mode,
            active: self
                .session
                .pinned_skill_names()
                .iter()
                .any(|pinned| pinned == name),
        })
    }
}

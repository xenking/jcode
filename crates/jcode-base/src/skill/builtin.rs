//! Skills compiled into the binary so they exist on every install, whatever is
//! in the user's skills dirs or cwd. They load first, so any same-named skill
//! from a plugin, `~/.agents`, `~/.jcode` or the project overrides them whole.

use super::{Arc, Path, PathBuf, SkillRegistry};

const BUILTIN_SKILLS: &[(&str, &str)] =
    &[("native-ui", include_str!("../builtin_skills/native-ui.md"))];

pub(super) fn is_builtin_path(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.starts_with("builtin:"))
}

impl SkillRegistry {
    pub(super) fn insert_builtin_skills(&mut self) {
        for (name, content) in BUILTIN_SKILLS {
            match Self::parse_skill_content(
                content,
                PathBuf::from(format!("builtin:{name}/SKILL.md")),
            ) {
                Ok(skill) => {
                    self.skills.insert(skill.name.clone(), Arc::new(skill));
                }
                Err(error) => {
                    crate::logging::warn(&format!("Bad built-in skill '{name}': {error}"));
                }
            }
        }
    }
}

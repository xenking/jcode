use super::*;
use crate::config::{AgentProfileConfig, AgentsConfig};

fn agents_with(name: &str, profile: AgentProfileConfig) -> AgentsConfig {
    let mut agents = AgentsConfig::default();
    agents.profiles.insert(name.to_string(), profile);
    agents
}

#[test]
fn every_builtin_resolves_with_instructions() {
    let agents = AgentsConfig::default();
    for name in BUILTIN_PROFILE_NAMES {
        let profile = resolve_profile(name, &agents).expect("built-in resolves");
        assert!(
            profile
                .instructions
                .as_deref()
                .is_some_and(|text| text.len() > 80),
            "{name} needs a persona"
        );
    }
}

#[test]
fn read_only_builtins_cannot_edit() {
    let agents = AgentsConfig::default();
    for name in ["scout", "reviewer", "orchestrator"] {
        let tools = resolve_profile(name, &agents).unwrap().tools.unwrap();
        for write_tool in ["edit", "write", "apply_patch"] {
            assert!(
                !tools.contains(write_tool),
                "{name} must not get {write_tool}"
            );
        }
        assert!(
            tools.contains("todo") && tools.contains("swarm"),
            "{name} must report"
        );
    }
    assert!(
        !resolve_profile("orchestrator", &agents)
            .unwrap()
            .tools
            .unwrap()
            .contains("bash")
    );
}

#[test]
fn user_profile_overrides_builtin_field_by_field() {
    let agents = agents_with(
        "scout",
        AgentProfileConfig {
            model: Some("claude-haiku-5-5".into()),
            effort: Some("medium".into()),
            ..Default::default()
        },
    );
    let scout = resolve_profile("scout", &agents).unwrap();
    assert_eq!(scout.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(scout.effort.as_deref(), Some("medium"));
    // Unset fields keep the built-in values.
    assert!(scout.tools.unwrap().contains("agentgrep"));
    assert!(!scout.include_skills);
    assert!(scout.instructions.unwrap().contains("read-only"));
}

#[test]
fn custom_profile_and_unknown_name() {
    let agents = agents_with(
        "researcher",
        AgentProfileConfig {
            instructions: Some("Research only.".into()),
            tools: Some(vec!["read".into(), "websearch".into()]),
            ..Default::default()
        },
    );
    let researcher = resolve_profile("researcher", &agents).unwrap();
    assert!(researcher.include_skills);
    assert_eq!(researcher.tools.unwrap().len(), 2);

    let error = resolve_profile("nope", &agents).unwrap_err();
    assert!(
        error.contains("researcher") && error.contains("scout"),
        "{error}"
    );
}

#[test]
fn instructions_file_wins_and_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persona.md");
    std::fs::write(&path, "  From file.  \n").unwrap();
    let agents = agents_with(
        "task",
        AgentProfileConfig {
            instructions_file: Some(path.display().to_string()),
            ..Default::default()
        },
    );
    assert_eq!(
        resolve_profile("task", &agents)
            .unwrap()
            .instructions
            .as_deref(),
        Some("From file.")
    );

    let agents = agents_with(
        "task",
        AgentProfileConfig {
            instructions_file: Some(dir.path().join("missing.md").display().to_string()),
            ..Default::default()
        },
    );
    assert!(resolve_profile("task", &agents).is_err());
}

#[test]
fn narrowing_only_removes_tools() {
    let agents = AgentsConfig::default();
    let scout = resolve_profile("scout", &agents).unwrap();

    // Global allowlist without `websearch`: the profile cannot add it back.
    let mut allowed = Some(HashSet::from(["read".to_string(), "edit".to_string()]));
    let mut disabled = HashSet::new();
    scout.narrow_selection(&mut allowed, &mut disabled);
    assert_eq!(allowed, Some(HashSet::from(["read".to_string()])));

    // No global allowlist: the profile list becomes the allowlist, and
    // global disables stay in force.
    let mut allowed = None;
    let mut disabled = HashSet::from(["bash".to_string()]);
    scout.narrow_selection(&mut allowed, &mut disabled);
    let allowed = allowed.unwrap();
    assert!(allowed.contains("agentgrep") && !allowed.contains("edit"));
    assert!(disabled.contains("bash"));

    let task = resolve_profile("task", &agents).unwrap();
    let mut allowed = None;
    let mut disabled = HashSet::new();
    task.narrow_selection(&mut allowed, &mut disabled);
    assert!(allowed.is_none());
    assert!(disabled.contains("macos_computer_use"));
}

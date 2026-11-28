use super::{DaemonSession, Request, Result, ServerEvent, Value, json};

pub(super) async fn invoke_acp_skill(
    session: &DaemonSession,
    name: &str,
    arguments: String,
) -> Result<()> {
    let request_id = session.next_id();
    session
        .send(&Request::RuntimeCommand {
            id: request_id,
            command: "mode".into(),
            args: vec!["--invoke-skill".into(), name.to_owned(), arguments],
        })
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match session.read_event().await? {
                ServerEvent::RuntimeCommandResult { id, .. } if id == request_id => return Ok(()),
                ServerEvent::Error { id, message, .. } if id == request_id => {
                    anyhow::bail!("Could not activate skill /{name}: {message}");
                }
                _ => {}
            }
        }
    })
    .await?
}

/// Resolve `/name [prompt]` or `$name [prompt]` to a registered skill. Returns
/// `None` for unknown names and path-like text so it reaches the model as-is.
pub(super) fn resolve_acp_skill_invocation(
    skills: &crate::skill::SkillRegistry,
    text: &str,
) -> Option<(String, Option<String>)> {
    let trimmed = text.trim_end();
    // Like built-in commands, a leading space escapes skill interpretation.
    let normalized = if let Some(rest) = trimmed.strip_prefix('$') {
        format!("/{rest}")
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        return None;
    };
    let invocation = skills.resolve_invocation(&normalized)?;
    skills.get(invocation.name)?;
    Some((
        invocation.name.to_string(),
        invocation.prompt.map(str::to_string),
    ))
}

/// ACP `availableCommands` entries for installed skills. Skills whose names
/// contain whitespace or shadow built-in commands are not advertised (they can
/// still be invoked by typing the slash command).
pub(super) fn acp_skill_commands(skills: &crate::skill::SkillRegistry) -> Vec<Value> {
    const BUILTIN: [&str; 3] = ["model", "models", "effort"];
    skills
        .list()
        .into_iter()
        .filter(|skill| {
            !skill.name.is_empty()
                && !skill.name.contains(char::is_whitespace)
                && !BUILTIN.contains(&skill.name.as_str())
        })
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "input": { "hint": "request for this skill (optional)" },
            })
        })
        .collect()
}

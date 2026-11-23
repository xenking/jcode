use crate::session::Session;
use anyhow::Result;

pub(super) fn create_coordinator_session(
    parent: &Session,
    mission: &Option<String>,
) -> Result<Session> {
    let title = Some(match mission {
        Some(mission) => format!("Overnight: {}", crate::util::truncate_str(mission, 48)),
        None => "Overnight coordinator".to_string(),
    });
    let mut child = Session::create(Some(parent.id.clone()), title);
    child.agent_started = true;
    child.replace_messages(parent.messages.clone());
    child.compaction = parent.compaction.clone();
    child.provider_key = parent.provider_key.clone();
    child.route_api_method = parent.route_api_method.clone();
    child.reasoning_effort = parent.reasoning_effort.clone();
    child.subagent_model = parent.subagent_model.clone();
    child.improve_mode = parent.improve_mode;
    child.autoreview_enabled = Some(false);
    child.autojudge_enabled = Some(false);
    child.is_canary = parent.is_canary;
    child.testing_build = parent.testing_build.clone();
    child.working_dir = parent.working_dir.clone();
    child.provider_session_id = None;
    Ok(child)
}

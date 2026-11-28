use crate::session::{Inherit, Session};
use anyhow::Result;

pub(super) fn create_coordinator_session(
    parent: &Session,
    mission: &Option<String>,
) -> Result<Session> {
    let title = Some(match mission {
        Some(mission) => format!("Overnight: {}", crate::util::truncate_str(mission, 48)),
        None => "Overnight coordinator".to_string(),
    });
    let mut child = Session::create(Some(parent.id().to_owned()), title);
    child.mark_agent_started();
    child.inherit_from(
        parent,
        Inherit::MESSAGES
            | Inherit::COMPACTION
            | Inherit::ACCOUNT_ROUTE
            | Inherit::REASONING_EFFORT
            | Inherit::SUBAGENT_MODEL
            | Inherit::IMPROVE_MODE
            | Inherit::BUILD_UNDER_TEST
            | Inherit::WORKING_DIR,
    );
    child.apply_autonomy(crate::session::Autonomy {
        autoreview_enabled: Some(false),
        ..child.autonomy()
    });
    child.apply_autonomy(crate::session::Autonomy {
        autojudge_enabled: Some(false),
        ..child.autonomy()
    });
    child.forget_provider_session();
    Ok(child)
}

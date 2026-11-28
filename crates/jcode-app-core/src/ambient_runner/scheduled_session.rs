use super::{ScheduledItem, Session, logging};
use crate::session::Inherit;

pub(super) fn scheduled_child_session(item: &ScheduledItem, parent_session_id: &str) -> Session {
    let mut child = match Session::load(parent_session_id) {
        Ok(parent) => {
            let mut child = Session::create(
                Some(parent_session_id.to_string()),
                Some(
                    item.task_description
                        .clone()
                        .unwrap_or_else(|| "Scheduled task".to_string()),
                ),
            );
            child.inherit_from(
                &parent,
                Inherit::MESSAGES
                    | Inherit::COMPACTION
                    | Inherit::ACCOUNT_ROUTE
                    | Inherit::AGENT_MODEL_OVERRIDES
                    | Inherit::SUBAGENT_MODEL
                    | Inherit::AUTONOMY
                    | Inherit::BUILD_UNDER_TEST
                    | Inherit::DEBUG_FLAG
                    | Inherit::REPLAY,
            );
            child.apply_route(crate::session::Route {
                model: parent
                    .effective_agent_model("ambient", crate::config::Config::load().ambient.model)
                    .filter(|model| model != "inherit")
                    .or_else(|| parent.model().map(str::to_owned)),
                ..child.route()
            });
            child.apply_workspace(crate::session::Workspace {
                working_dir: item
                    .working_dir
                    .clone()
                    .or(parent.working_dir().map(str::to_owned)),
                ..child.workspace()
            });
            child
        }
        Err(err) => {
            logging::warn(&format!(
                "Ambient runner: failed to load parent session {} for spawned scheduled task {}; creating a fresh child instead: {}",
                parent_session_id, item.id, err
            ));
            let mut child = Session::create(
                Some(parent_session_id.to_string()),
                Some(
                    item.task_description
                        .clone()
                        .unwrap_or_else(|| "Scheduled task".to_string()),
                ),
            );
            child.apply_workspace(crate::session::Workspace {
                working_dir: item.working_dir.clone(),
                ..child.workspace()
            });
            child
        }
    };
    child.mark_agent_started();
    child
}

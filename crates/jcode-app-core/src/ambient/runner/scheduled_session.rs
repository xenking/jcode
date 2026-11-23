use super::{ScheduledItem, Session, logging};

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
            child.replace_messages(parent.messages.clone());
            child.compaction = parent.compaction.clone();
            child.provider_key = parent.provider_key.clone();
            child.route_api_method = parent.route_api_method.clone();
            child.model = parent
                .effective_agent_model("ambient", crate::config::Config::load().ambient.model)
                .filter(|model| model != "inherit")
                .or_else(|| parent.model.clone());
            child.agent_model_overrides = parent.agent_model_overrides.clone();
            child.subagent_model = parent.subagent_model.clone();
            child.improve_mode = parent.improve_mode;
            child.autoreview_enabled = parent.autoreview_enabled;
            child.autojudge_enabled = parent.autojudge_enabled;
            child.is_canary = parent.is_canary;
            child.testing_build = parent.testing_build.clone();
            child.is_debug = parent.is_debug;
            child.memory_injections = parent.memory_injections.clone();
            child.replay_events = parent.replay_events.clone();
            child.working_dir = item.working_dir.clone().or(parent.working_dir.clone());
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
            child.working_dir = item.working_dir.clone();
            child
        }
    };
    child.agent_started = true;
    child
}

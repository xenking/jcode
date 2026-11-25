//! Snapshot preparation for session split and transfer actions.

use super::*;
use crate::session::Inherit;

pub(super) fn clone_split_session(
    parent_session_id: &str,
    live_parent: Option<&Session>,
) -> anyhow::Result<(String, String)> {
    let child = snapshot_child_session(parent_session_id, live_parent, false)?;
    Ok((child.id().to_owned(), child.display_name().to_string()))
}

fn snapshot_child_session(
    parent_session_id: &str,
    live_parent: Option<&Session>,
    side_question: bool,
) -> anyhow::Result<Session> {
    // Keep the persisted snapshot authoritative, including while the parent is
    // busy. A brand-new Agent may not have saved anything yet, however. Only a
    // missing snapshot permits an in-memory fallback, never corrupt/unreadable
    // history or a session belonging to a different client.
    let parent = Session::load(parent_session_id).or_else(|error| {
        let missing = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        match live_parent.filter(|parent| missing && parent.id() == parent_session_id) {
            Some(parent) => Ok(parent.clone()),
            None => Err(error),
        }
    })?;

    let mut child = Session::create(Some(parent_session_id.to_string()), None);
    child.inherit_from(
        &parent,
        Inherit::MESSAGES
            | Inherit::COMPACTION
            | Inherit::SYSTEM_PROMPT
            | Inherit::WORKING_DIR
            // Keep the same provider account. Replayed provider-native blocks
            // (Anthropic web search `encrypted_content`) are bound to the account
            // that produced them, and another account rejects the whole request
            // with a 400.
            | Inherit::ROUTE,
    );
    child.set_status(crate::session::SessionStatus::Closed);
    // The parent agent keeps ownership of any in-flight request; tell the
    // forked agent so it treats the next prompt as fresh work instead of
    // continuing (and duplicating) the parent's current turn.
    child.append_fork_notice(parent_session_id, parent.display_name());
    if side_question {
        child.inherit_from(&parent, Inherit::LIMITS);
        child.save_new_side_question()?;
    } else {
        child.save()?;
    }

    crate::session_env::inherit_session_env(parent_session_id, child.id());
    Ok(child)
}

pub(in crate::server) fn handle_split_side_question(
    id: u64,
    parent_session_id: &str,
    agent: &Arc<Mutex<Agent>>,
) -> ServerEvent {
    let idle_agent = agent.try_lock().ok();
    let built = match idle_agent.as_ref() {
        Some(idle) => Agent::read_session(idle, |session| {
            snapshot_child_session(parent_session_id, Some(session), true)
        }),
        None => snapshot_child_session(parent_session_id, None, true),
    };
    match built {
        Ok(child) => ServerEvent::SideQuestionForked {
            id,
            topic: child
                .side_question_topic()
                .expect("new side-question child"),
        },
        Err(error) => ServerEvent::Error {
            id,
            message: format!("Could not fork side question: {error:#}"),
            retry_after_secs: None,
        },
    }
}

pub(super) fn transfer_active_messages(session: &Session) -> Vec<crate::message::Message> {
    let start = session
        .compaction()
        .map(|state| state.compacted_count.min(session.messages().len()))
        .unwrap_or(0);
    session.messages()[start..]
        .iter()
        .map(crate::session::StoredMessage::to_message)
        .collect()
}

pub(super) fn create_transfer_child_session(
    parent_session_id: &str,
    parent: &Session,
    compaction: Option<Arc<crate::session::StoredCompactionState>>,
) -> anyhow::Result<(String, String)> {
    let mut todos = crate::todo::load_todos(parent_session_id)?;
    // Background tasks belong to the source session. A fresh session must
    // reassess those results, not inherit foreign task IDs or their wakes.
    for todo in &mut todos {
        todo.waiting_on.clear();
        todo.waiting_reason = None;
    }
    let mut child = Session::create(Some(parent_session_id.to_string()), None);
    child.inherit_from(
        &parent,
        Inherit::SYSTEM_PROMPT
            | Inherit::WORKING_DIR
            | Inherit::ROUTE
            | Inherit::SUBAGENT_MODEL
            | Inherit::AUTONOMY
            | Inherit::BUILD_UNDER_TEST,
    );
    child.set_compaction(compaction);
    child.set_provider_session_id(None);
    child.set_status(crate::session::SessionStatus::Closed);
    child.save()?;
    crate::todo::save_todos(child.id(), &todos)?;
    crate::session_env::inherit_session_env(parent_session_id, child.id());
    Ok((child.id().to_owned(), child.display_name().to_string()))
}

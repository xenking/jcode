use super::{Agent, SwarmMember};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::{Mutex, RwLock};

pub(super) async fn live_session_working_dir(
    agent: &Mutex<Agent>,
    session_id: &str,
    members: &RwLock<HashMap<String, SwarmMember>>,
) -> Option<String> {
    if let Ok(agent) = agent.try_lock() {
        return agent.working_dir().map(str::to_owned);
    }
    // Do not wait for a turn's Agent lock. A member can describe unsaved
    // sessions, and sessions that never joined a swarm have a disk fallback.
    let member_dir = members
        .read()
        .await
        .get(session_id)
        .and_then(|member| member.working_dir.as_ref())
        .map(|path| path.to_string_lossy().into_owned());
    member_dir.or_else(
        || match crate::session::Session::load_startup_stub(session_id) {
            Ok(session) => session.working_dir().map(str::to_owned),
            Err(error) => {
                crate::logging::debug(&format!(
                    "Could not read working directory for busy session {session_id}: {error:#}"
                ));
                None
            }
        },
    )
}

/// Resolve the working directory a subscribe should actually bind to.
///
/// Returns the reported dir when it is acceptable, or the session's existing
/// dir when the report is rejected by [`subscribe_working_dir_replacement`].
/// Every consumer of a subscribe cwd (agent state, swarm id, project-local MCP
/// resolution) must agree on this one answer, otherwise the session's tools,
/// swarm grouping, and MCP config can each resolve against a different
/// directory (issue #481).
pub(super) fn effective_subscribe_working_dir(
    current: Option<&str>,
    reported: &str,
    home: Option<&Path>,
) -> String {
    match subscribe_working_dir_replacement(current, reported, home) {
        Some(accepted) => accepted,
        None => current
            .map(str::to_string)
            .unwrap_or_else(|| reported.trim().to_string()),
    }
}

/// Decide whether a client-reported subscribe cwd may replace the session's
/// current working directory.
///
/// Requiring a subscribe cwd to be non-empty and absolute (the earlier
/// require-cwd change) is necessary but not sufficient: a client that launches
/// with an inherited environment can report the user's *home* directory even
/// though the real project lives elsewhere. Accepting that silently re-pins the
/// session to home, so bash/file tools run against home while the header still
/// shows the project path (issue #481).
///
/// The rule is deliberately narrow so it cannot break legitimate directory
/// changes: a reported cwd that is exactly the home directory is ignored *only*
/// when the session already has a different working directory. Working in home
/// on purpose (no prior cwd, or a session already pinned to home) still works,
/// and every other path is accepted as before.
pub(super) fn subscribe_working_dir_replacement(
    current: Option<&str>,
    reported: &str,
    home: Option<&Path>,
) -> Option<String> {
    let reported_trimmed = reported.trim();
    if reported_trimmed.is_empty() {
        return None;
    }
    let current = current.map(str::trim).filter(|dir| !dir.is_empty());
    if current == Some(reported_trimmed) {
        return None;
    }
    if let (Some(current), Some(home)) = (current, home)
        && Path::new(reported_trimmed) == home
        && Path::new(current) != home
    {
        return None;
    }
    Some(reported_trimmed.to_string())
}

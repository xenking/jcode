//! Queries over the swarm report-back graph.

use super::SwarmMember;
use std::collections::{HashMap, HashSet};

/// Walk the `report_back_to_session_id` chain upward from `session_id`,
/// returning the list of ancestor session ids (parent first, root last).
///
/// The spawner/parent edge is encoded by `report_back_to_session_id`: a child
/// spawned by `P` reports back to `P`. Walking that chain reconstructs the spawn
/// tree without persisting a separate parent field. Cycles (which should never
/// happen) are guarded against with a visited set.
pub(super) fn swarm_ancestors(
    members: &HashMap<String, SwarmMember>,
    session_id: &str,
) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(session_id.to_string());
    let mut current = session_id.to_string();
    while let Some(parent) = members
        .get(&current)
        .and_then(|member| member.report_back_to_session_id.clone())
    {
        if parent == current || !visited.insert(parent.clone()) {
            break;
        }
        ancestors.push(parent.clone());
        current = parent;
    }
    ancestors
}

/// True when `ancestor` is `session_id` itself or any transitive spawner of it.
/// Used to decide whether a requester may manage (stop/control) a target: an
/// agent owns its entire spawned subtree.
pub(super) fn swarm_is_self_or_ancestor(
    members: &HashMap<String, SwarmMember>,
    ancestor: &str,
    session_id: &str,
) -> bool {
    ancestor == session_id
        || swarm_ancestors(members, session_id)
            .iter()
            .any(|candidate| candidate == ancestor)
}

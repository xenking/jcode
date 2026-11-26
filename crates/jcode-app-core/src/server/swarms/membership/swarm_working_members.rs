use super::SwarmMember;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, OnceLock},
};
use tokio::sync::RwLock;

/// The live swarm member map of the most recently started server. Unlike the
/// once-only dispatch context it is replaced on every start, so a process that
/// hosts several servers in turn (tests, embedders) never reads a dead one.
type LiveMembers = StdMutex<Option<Arc<RwLock<HashMap<String, SwarmMember>>>>>;

fn live_members() -> &'static LiveMembers {
    static MEMBERS: OnceLock<LiveMembers> = OnceLock::new();
    MEMBERS.get_or_init(|| StdMutex::new(None))
}

pub(super) fn set_live_members(members: Arc<RwLock<HashMap<String, SwarmMember>>>) {
    *live_members()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(members);
}

/// Swarm members spawned by `session_id` that are still working. Their
/// completion wakes the spawner, so the spawner must not be poked meanwhile.
pub(super) fn working_members_spawned_by(
    members: &HashMap<String, SwarmMember>,
    session_id: &str,
) -> Vec<String> {
    let mut working: Vec<String> = members
        .values()
        .filter(|member| {
            member.session_id != session_id
                && member.report_back_to_session_id.as_deref() == Some(session_id)
                && crate::todo::swarm_member_is_working(&member.status)
        })
        .map(|member| member.session_id.clone())
        .collect();
    working.sort();
    working
}

/// Live working members spawned by `session_id`. Empty without a registered
/// server context (tests, embedders), which preserves the previous behavior.
pub(super) async fn running_members_for(session_id: &str) -> Vec<String> {
    let Some(members) = live_members()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    else {
        return Vec::new();
    };
    let members = members.read().await;
    working_members_spawned_by(&members, session_id)
}

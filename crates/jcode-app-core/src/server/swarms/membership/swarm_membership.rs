use crate::server::{await_members_state::remember_terminal_swarm_departure, run_blocking_io};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) async fn remove_member_and_record_departure(
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_id: &str,
    session_id: &str,
) {
    let was_member = {
        let mut swarms = swarms_by_id.write().await;
        if let Some(swarm) = swarms.get_mut(swarm_id) {
            let was_member = swarm.remove(session_id);
            if swarm.is_empty() {
                swarms.remove(swarm_id);
            }
            was_member
        } else {
            false
        }
    };
    if was_member {
        run_blocking_io(|| {
            remember_terminal_swarm_departure(swarm_id, session_id);
        });
    }
}

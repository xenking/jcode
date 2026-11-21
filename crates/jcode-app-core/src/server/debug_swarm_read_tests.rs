#[cfg(test)]
mod tests {
    use super::super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn coordinators_debug_does_not_hold_coordinator_while_waiting_for_members() {
        let sessions = Arc::new(
            super::super::super::session_manager::SessionManager::from_agents(HashMap::new()),
        );
        let members = Arc::new(RwLock::new(HashMap::new()));
        let swarms = Arc::new(RwLock::new(HashMap::new()));
        let context = Arc::new(RwLock::new(HashMap::new()));
        let plans = Arc::new(RwLock::new(HashMap::new()));
        let coordinators = Arc::new(RwLock::new(HashMap::new()));
        let touches = FileTouchService::new();
        let channels = Arc::new(RwLock::new(HashMap::new()));
        let identity = ServerIdentity {
            id: String::new(),
            name: String::new(),
            icon: String::new(),
            git_hash: String::new(),
            version: String::new(),
        };

        let members_gate = members.write().await;
        let command = maybe_handle_swarm_read_command(
            "swarm:coordinators",
            &sessions,
            &members,
            &swarms,
            &context,
            &plans,
            &coordinators,
            &touches,
            &channels,
            &identity,
        );
        tokio::pin!(command);
        tokio::select! {
            result = &mut command => panic!("command unexpectedly completed: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        let coordinator_gate = tokio::time::timeout(Duration::from_secs(1), coordinators.write())
            .await
            .expect("debug command held coordinators while waiting for members");
        drop(coordinator_gate);
        drop(members_gate);
        tokio::time::timeout(Duration::from_secs(1), command)
            .await
            .expect("debug command stalled")
            .expect("debug command failed");
    }
}

use crate::{agent::Agent, protocol::ServerEvent};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(in crate::server) async fn poll_agent_compaction_completion(
    agent: Arc<Mutex<Agent>>,
) -> Option<ServerEvent> {
    // CompactionFinished is published from inside the worker, just before its
    // JoinHandle becomes finished. The first poll can therefore be too early.
    loop {
        let wait = std::time::Instant::now();
        let mut owner = agent.lock().await;
        let waited = wait.elapsed();
        if waited >= std::time::Duration::from_millis(10) {
            crate::logging::info(&format!(
                "[TIMING] compaction_agent_lock_wait: elapsed_ms={}, reason=active_turn_owner",
                waited.as_millis()
            ));
        }
        let (event, error, pending) = owner.poll_compaction_status();
        drop(owner);
        if let Some(event) = event {
            return Some(crate::compaction::compaction_server_event(&event));
        }
        if let Some(error) = error {
            return Some(ServerEvent::CompactResult {
                id: 0,
                success: false,
                message: format!("Compaction failed: {error}"),
            });
        }
        if !pending {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

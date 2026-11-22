use super::*;

/// Apply to the bound agent, not the connection's temporary registry. Like
/// working-dir updates, a busy live target must not block client attachment.
pub(in crate::server) async fn apply_or_defer_desktop_response_capability(
    agent: &Arc<Mutex<Agent>>,
    supports_user_response: bool,
    supports_applets: bool,
) {
    if let Ok(mut guard) = agent.try_lock() {
        guard
            .set_desktop_response_capability(supports_user_response, supports_applets)
            .await;
        return;
    }
    let agent = Arc::clone(agent);
    crate::runtime_activity::spawn(async move {
        agent
            .lock()
            .await
            .set_desktop_response_capability(supports_user_response, supports_applets)
            .await;
    });
}

//! Wire handler for `routing_view`: the routing configuration with live usage.

use crate::protocol::ServerEvent;

pub(super) async fn reply(id: u64) -> ServerEvent {
    let now = chrono::Utc::now().timestamp();
    // Reading credential metadata touches the config and the key store.
    match tokio::task::spawn_blocking(move || jcode_base::routing_view::current(now)).await {
        Ok(view) => ServerEvent::RoutingView { id, view },
        Err(error) => ServerEvent::Error {
            id,
            message: format!("Routing view failed: {error}"),
            retry_after_secs: None,
        },
    }
}

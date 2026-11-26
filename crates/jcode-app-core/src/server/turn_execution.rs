//! Execute reserved turns with server routing and completion accounting.

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use anyhow::Result;
use tokio::sync::mpsc;

/// Execute only with this runtime's non-cloneable turn reservation.
#[expect(
    clippy::too_many_arguments,
    reason = "Carry message payload and turn lease through the streaming boundary"
)]
pub(super) async fn process_reserved_message_streaming_mpsc(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    queued_client_id: Option<Vec<String>>,
    attachment_data: Option<jcode_session_types::MessageAttachments>,
) -> Result<()> {
    process_reserved_message_with_role(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx,
        None,
        queued_client_id,
        attachment_data,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Carry turn lease and input provenance through the streaming boundary"
)]
pub(super) async fn process_reserved_message_with_role(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    display_role: Option<crate::session::StoredDisplayRole>,
    queued_client_id: Option<Vec<String>>,
    attachment_data: Option<jcode_session_types::MessageAttachments>,
) -> Result<()> {
    agent.validate_server_turn(lease)?;
    let session_id = agent.session_id().to_string();
    super::routing_turn::before_turn(agent, &event_tx).await?;
    let result = agent
        .run_reserved_streaming_mpsc(
            lease,
            content,
            images,
            system_reminder,
            event_tx,
            display_role,
            queued_client_id,
            attachment_data,
        )
        .await;
    if result.is_ok() {
        super::routing_turn::after_turn(agent);
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "turn_completed",
                "message_turn_finished",
            )
            .with_session_id(session_id)
            .force_attribution(),
        );
        crate::process_memory::release_retained_heap_debounced(
            "server_turn_completed",
            std::time::Duration::from_secs(30),
        );
    }
    result
}

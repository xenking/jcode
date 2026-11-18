use super::*;

pub(in crate::tui::app) fn handle_remote_char_input(app: &mut App, c: char) {
    input::handle_text_input(app, &c.to_string());
}

pub(in crate::tui::app) async fn send_interleave_now(
    app: &mut App,
    content: String,
    images: Vec<(String, String)>,
    remote: &mut RemoteConnection,
) {
    send_queued_prompt(
        app,
        content,
        images,
        crate::protocol::QueuedPromptMode::Steer,
        remote,
    )
    .await;
}

pub(in crate::tui::app) async fn send_queued_prompt(
    app: &mut App,
    content: String,
    images: Vec<(String, String)>,
    mode: crate::protocol::QueuedPromptMode,
    remote: &mut RemoteConnection,
) {
    send_queued_prompt_with_attachments(
        app,
        content.clone(),
        images,
        Vec::new(),
        content,
        mode,
        remote,
    )
    .await;
}

pub(in crate::tui::app) async fn send_prepared_queued_prompt(
    app: &mut App,
    prepared: input::PreparedInput,
    mode: crate::protocol::QueuedPromptMode,
    remote: &mut RemoteConnection,
) {
    let (content, attachments) = input::paste_attachment_payload(
        &prepared.raw_input,
        &prepared.expanded,
        &prepared.pasted_contents,
    );
    send_queued_prompt_with_attachments(
        app,
        content,
        prepared.images,
        attachments,
        prepared.expanded,
        mode,
        remote,
    )
    .await;
}

async fn send_queued_prompt_with_attachments(
    app: &mut App,
    content: String,
    images: Vec<(String, String)>,
    attachments: Vec<jcode_session_types::Attachment>,
    fallback_content: String,
    mode: crate::protocol::QueuedPromptMode,
    remote: &mut RemoteConnection,
) {
    if content.trim().is_empty() && images.is_empty() && attachments.is_empty() {
        return;
    }
    let prompt = crate::protocol::QueuedPrompt {
        client_id: crate::id::new_id("prompt"),
        content: content.clone(),
        images: images.clone(),
        attachments,
        active_skill: None,
        mode,
        editing: false,
    };
    match remote.queue_prompt(prompt.clone()).await {
        Err(e) => {
            app.restore_failover_payload(app_mod::FallbackResendPayload {
                content: fallback_content,
                images,
                is_system: false,
                system_reminder: None,
                auto_retry: false,
                retry_attempts: 0,
                raw_input: None,
            });
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to queue prompt: {}",
                e
            )));
        }
        Ok(id) => {
            app.pending_queue_requests.push((id, Some(prompt)));
            app.set_status_notice(match mode {
                crate::protocol::QueuedPromptMode::Steer => "Queued: steer",
                crate::protocol::QueuedPromptMode::AfterYield => "Queued: after yield",
            });
            app.restore_stash_after_send();
        }
    }
}

pub(in crate::tui::app) async fn handle_remote_update_command(
    app: &mut App,
    remote: &mut RemoteConnection,
) -> Result<()> {
    if crate::tui::is_ssh_remote() {
        app.set_status_notice("Update the client and SSH server separately, then reconnect");
        return Ok(());
    }
    reload_stale_remote_server_before_update(app, remote).await?;

    let session_id = app
        .remote_session_id
        .clone()
        .unwrap_or_else(|| crate::id::new_id("ses"));
    app.start_background_client_update(session_id);
    Ok(())
}

pub(in crate::tui::app) async fn reload_stale_remote_server_before_update(
    app: &mut App,
    remote: &mut RemoteConnection,
) -> Result<bool> {
    if app.remote_server_has_update != Some(true) {
        return Ok(false);
    }

    app.append_reload_message("Reloading stale server before checking for client updates...");
    remote.reload().await?;
    Ok(true)
}

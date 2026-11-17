use crate::protocol::{ServerEvent, encode_event};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub(super) fn event_for_transport(
    mut event: ServerEvent,
    supports_turn_identity: bool,
) -> Option<ServerEvent> {
    if supports_turn_identity {
        return Some(event);
    }
    loop {
        event = match event {
            ServerEvent::TurnEvent { event, .. } => *event,
            ServerEvent::TurnStarted { .. } | ServerEvent::TurnState { .. } => return None,
            event => return Some(event),
        };
    }
}

/// Client disconnects must not cancel daemon-owned work, but lost replies need
/// a source location and operation without dumping private event payloads.
#[track_caller]
pub(super) fn send_client_event_best_effort(
    sender: &tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    event: ServerEvent,
    operation: &str,
) {
    if sender.send(event).is_err() {
        crate::logging::debug(&format!(
            "Client receiver disconnected during {operation} at {}",
            std::panic::Location::caller()
        ));
    }
}

/// Project wire copies only. Persisted state and opted-in API bridges retain
/// PDF bytes, while old native clients can decode the complete History event.
pub(super) fn side_panel_for_client(
    mut snapshot: crate::side_panel::SidePanelSnapshot,
    supports_pdf_panels: bool,
) -> crate::side_panel::SidePanelSnapshot {
    if !supports_pdf_panels {
        for page in &mut snapshot.pages {
            if page.format == crate::side_panel::SidePanelPageFormat::Pdf {
                page.format = crate::side_panel::SidePanelPageFormat::Markdown;
                // This is a generated fallback, not a linked Markdown file.
                // Otherwise old TUIs re-read binary PDF bytes as text on change.
                page.source = crate::side_panel::SidePanelPageSource::Ephemeral;
            }
            page.pdf_data = None;
        }
    }
    snapshot
}

/// The session's stored todo card state (todos, plan, goals) as a wire event.
/// `pushes` tells the client this daemon sends the event on every change.
pub(super) fn todos_snapshot_event(session_id: &str) -> ServerEvent {
    let payload = serde_json::json!({
        "todos": crate::todo::load_todos(session_id).unwrap_or_else(|error| {
            crate::logging::warn(&format!("Failed to load todos for {session_id}: {error}"));
            Vec::new()
        }),
        "plan": crate::todo::load_plan(session_id).unwrap_or_else(|error| {
            crate::logging::warn(&format!("Failed to load todo plan for {session_id}: {error}"));
            Default::default()
        }),
        "goals": crate::todo::load_goals(session_id).unwrap_or_else(|error| {
            crate::logging::warn(&format!("Failed to load todo goals for {session_id}: {error}"));
            Default::default()
        }),
    });
    ServerEvent::Todos {
        session_id: session_id.to_owned(),
        payload: payload.to_string(),
        pushes: true,
    }
}

pub(super) async fn write_direct_event(
    writer: &Arc<Mutex<crate::transport::WriteHalf>>,
    event: &ServerEvent,
) -> Result<()> {
    let json = encode_event(event);
    let mut w = writer.lock().await;
    w.write_all(json.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_panel::{
        SidePanelPage, SidePanelPageFormat, SidePanelPageSource, SidePanelSnapshot,
    };

    #[test]
    fn best_effort_delivery_preserves_replies_and_tolerates_detached_clients() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        send_client_event_best_effort(&sender, ServerEvent::Done { id: 42 }, "test reply");
        assert!(matches!(
            receiver.try_recv(),
            Ok(ServerEvent::Done { id: 42 })
        ));
        drop(receiver);
        send_client_event_best_effort(&sender, ServerEvent::Done { id: 43 }, "detached reply");
    }

    #[test]
    fn pdf_panels_live_projection_preserves_fallback_and_opted_in_payload() {
        let snapshot = SidePanelSnapshot {
            focus_revision: 123,
            focused_page_id: Some("report".into()),
            pages: vec![SidePanelPage {
                id: "report".into(),
                title: "Report".into(),
                format: SidePanelPageFormat::Pdf,
                source: SidePanelPageSource::LinkedFile,
                content: "PDF document fallback".into(),
                pdf_data: Some("JVBERi0xLjQKJSVFT0Y=".into()),
                ..Default::default()
            }],
        };
        let opted = side_panel_for_client(snapshot.clone(), true);
        assert_eq!(opted, snapshot);
        let mut projected = side_panel_for_client(snapshot.clone(), false);
        assert_eq!(projected.focus_revision, 123);
        assert_eq!(projected.focused_page_id, snapshot.focused_page_id);
        assert_eq!(projected.pages[0].content, snapshot.pages[0].content);
        assert_eq!(projected.pages[0].format, SidePanelPageFormat::Markdown);
        assert!(projected.pages[0].pdf_data.is_none());
        assert_eq!(projected.pages[0].source, SidePanelPageSource::Ephemeral);
        assert!(!crate::side_panel::refresh_linked_page_content(
            &mut projected,
            None
        ));

        // The shipped native reader has this closed enum. Unknown fields are
        // tolerated, but a new value in this existing field is not.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyFormat {
            Markdown,
        }
        #[derive(serde::Deserialize)]
        struct LegacyPage {
            format: LegacyFormat,
        }
        #[derive(serde::Deserialize)]
        struct LegacySnapshot {
            pages: Vec<LegacyPage>,
        }
        #[derive(serde::Deserialize)]
        struct LegacyEvent {
            snapshot: LegacySnapshot,
        }
        let wire = encode_event(&ServerEvent::SidePanelState {
            snapshot: projected,
        });
        let legacy: LegacyEvent = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            legacy.snapshot.pages[0].format,
            LegacyFormat::Markdown
        ));
        assert!(!wire.contains("pdf_data"));
        let wire = encode_event(&ServerEvent::SidePanelState { snapshot: opted });
        assert!(serde_json::from_str::<LegacyEvent>(&wire).is_err());
        assert!(wire.contains("JVBERi0xLjQKJSVFT0Y="));
    }
}

use super::*;

#[path = "history_paging_bounded.rs"]
mod bounded;

#[cfg(test)]
#[path = "history_paging_bounded_tests.rs"]
mod bounded_tests;

pub(in crate::server) async fn handle_history_page(
    id: u64,
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    writer: &Arc<Mutex<WriteHalf>>,
    before: Option<String>,
    limit: usize,
) -> Result<()> {
    let rows = if let Ok(guard) = agent.try_lock() {
        bounded::records(guard.session_for_history())
    } else {
        crate::session::Session::load_for_remote_startup(session_id)
            .or_else(|_| crate::session::Session::load_startup_stub(session_id))
            .map_err(|e| e.to_string())
            .and_then(|session| bounded::records(&session))
    };
    let page = rows.and_then(|rows| bounded::page_records(&rows, before.as_deref(), limit));
    let event = match page {
        Ok(page) => ServerEvent::HistoryPage {
            id,
            session_id: session_id.to_owned(),
            fragments: page
                .fragments
                .into_iter()
                .map(|fragment| serde_json::to_value(fragment).expect("fragment serializes"))
                .collect(),
            before: page.before,
        },
        Err(message) => ServerEvent::Error {
            id,
            message,
            retry_after_secs: None,
        },
    };
    super::super::client_writer::write_direct_event(writer, &event).await
}

pub(in crate::server) async fn handle_session_images(
    id: u64,
    requested_session_id: &str,
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    writer: &Arc<Mutex<WriteHalf>>,
    image_request: Option<(String, usize)>,
) -> Result<()> {
    let event = if requested_session_id != session_id {
        ServerEvent::Error {
            id,
            message: "session is not attached".into(),
            retry_after_secs: None,
        }
    } else {
        let build =
            |session: &crate::session::Session| -> std::result::Result<ServerEvent, String> {
                if let Some((image_id, offset)) = image_request.as_ref() {
                    let image = session
                        .session_image(image_id)
                        .ok_or("session image not found")?;
                    if *offset > image.data.len() || !image.data.is_char_boundary(*offset) {
                        return Err("invalid image offset".into());
                    }
                    let mut end = offset.saturating_add(512 * 1024).min(image.data.len());
                    while !image.data.is_char_boundary(end) {
                        end -= 1;
                    }
                    Ok(ServerEvent::SessionImage {
                        id,
                        session_id: session_id.into(),
                        image_id: image_id.clone(),
                        media_type: image.info.media_type,
                        offset: *offset,
                        total: image.data.len(),
                        data: image.data[*offset..end].to_owned(),
                    })
                } else {
                    Ok(ServerEvent::SessionImages {
                        id,
                        session_id: session_id.into(),
                        images: session
                            .session_images()
                            .into_iter()
                            .map(|image| {
                                serde_json::to_value(image).expect("image metadata serializes")
                            })
                            .collect(),
                    })
                }
            };
        let result = if let Ok(guard) = agent.try_lock() {
            build(guard.session_for_history())
        } else {
            crate::session::Session::load_for_remote_startup(session_id)
                .map_err(|e| e.to_string())
                .and_then(|session| build(&session))
        };
        result.unwrap_or_else(|message| ServerEvent::Error {
            id,
            message,
            retry_after_secs: None,
        })
    };
    super::super::client_writer::write_direct_event(writer, &event).await
}

fn image_chunk(
    session: &crate::session::Session,
    key: &str,
    offset: usize,
) -> std::result::Result<(usize, String), String> {
    let data = if key.starts_with("img_") {
        session
            .session_image(key)
            .ok_or("history image no longer exists")?
            .data
    } else {
        let (_, images, _) =
            crate::session::render_messages_and_images_with_compacted_history(session, usize::MAX);
        images
            .into_iter()
            .find(|image| bounded::image_key(&image.data) == key)
            .ok_or("history image no longer exists")?
            .data
    };
    if offset > data.len() || !data.is_char_boundary(offset) {
        return Err("invalid image offset".into());
    }
    let mut end = offset.saturating_add(512 * 1024).min(data.len());
    while !data.is_char_boundary(end) {
        end -= 1;
    }
    Ok((data.len(), data[offset..end].to_owned()))
}

pub(in crate::server) async fn handle_history_image_chunk(
    id: u64,
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    writer: &Arc<Mutex<WriteHalf>>,
    key: String,
    offset: usize,
) -> Result<()> {
    let result = if let Ok(guard) = agent.try_lock() {
        image_chunk(guard.session_for_history(), &key, offset)
    } else {
        crate::session::Session::load_for_remote_startup(session_id)
            .or_else(|_| crate::session::Session::load_startup_stub(session_id))
            .map_err(|e| e.to_string())
            .and_then(|session| image_chunk(&session, &key, offset))
    };
    let event = match result {
        Ok((total, data)) => ServerEvent::HistoryImageChunk {
            id,
            session_id: session_id.into(),
            key,
            offset,
            total,
            data,
        },
        Err(message) => ServerEvent::Error {
            id,
            message,
            retry_after_secs: None,
        },
    };
    super::super::client_writer::write_direct_event(writer, &event).await
}

pub(in crate::server) async fn handle_get_compacted_history(
    id: u64,
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    writer: &Arc<Mutex<WriteHalf>>,
    visible_messages: usize,
    recent_history: bool,
) -> Result<()> {
    let mut timing = Timing::new(
        if recent_history {
            "recent_history_page"
        } else {
            "compacted_history_page"
        },
        Some(id),
        session_id,
    );
    let started = Instant::now();
    let (messages, images, compacted_info, source) = match agent.try_lock() {
        Ok(agent_guard) => {
            let (messages, images, info) = if recent_history {
                let (messages, images) = agent_guard.get_history_and_rendered_images();
                let (messages, images, info) =
                    recent_history_window(messages, images, visible_messages);
                (messages, images, Some(info))
            } else {
                agent_guard.get_history_and_rendered_images_with_compacted_history(visible_messages)
            };
            (messages, images, info, "live")
        }
        Err(_) => {
            timing.stage("persisted_load");
            let loaded = crate::session::Session::load_for_remote_startup(session_id)
                .or_else(|_| crate::session::Session::load_startup_stub(session_id));
            if loaded.is_err() {
                timing.result(&loaded);
            }
            let session = loaded?;
            timing.stage("render");
            let (rendered_messages, images, info) = if recent_history {
                let (messages, images) = crate::session::render_messages_and_images(&session);
                (messages, images, None)
            } else {
                crate::session::render_messages_and_images_with_compacted_history(
                    &session,
                    visible_messages,
                )
            };
            let messages = rendered_messages
                .into_iter()
                .map(rendered_to_history_message)
                .collect();
            let (messages, images, info) = if recent_history {
                let (messages, images, info) =
                    recent_history_window(messages, images, visible_messages);
                (messages, images, Some(info))
            } else {
                (messages, images, info)
            };
            (messages, images, info, "persisted")
        }
    };

    let compacted_info = compacted_info.unwrap_or(crate::session::RenderedCompactedHistoryInfo {
        total_messages: 0,
        visible_messages: 0,
        remaining_messages: 0,
        hidden_user_prompts: 0,
    });
    crate::logging::info(&format!(
        "[TIMING] get_compacted_history: session={}, source={}, requested={}, visible={}, remaining={}, messages={}, total={}ms",
        session_id,
        source,
        visible_messages,
        compacted_info.visible_messages,
        compacted_info.remaining_messages,
        messages.len(),
        started.elapsed().as_millis(),
    ));

    timing.stage("encode");
    let event = ServerEvent::CompactedHistory {
        id,
        session_id: session_id.to_string(),
        messages,
        images,
        compacted_total: compacted_info.total_messages,
        compacted_visible: compacted_info.visible_messages,
        compacted_remaining: compacted_info.remaining_messages,
        compacted_hidden_prompts: compacted_info.hidden_user_prompts,
    };
    let json = encode_event(&event);
    timing.stage("writer_lock");
    let mut writer = writer.lock().await;
    timing.stage("write");
    let result = writer.write_all(json.as_bytes()).await;
    timing.result(&result);
    result.map_err(Into::into)
}

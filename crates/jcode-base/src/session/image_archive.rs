use super::{
    ContentBlock, Session,
    images::{archive_marker, image_id, is_archive_marker, write_archived_image},
};

/// Archive oldest inline images before request-size recovery removes them from
/// the model context. A failed write leaves the complete transcript untouched.
pub(super) fn archive_to_budget(session: &mut Session, target: usize) -> usize {
    let mut total = 0usize;
    let mut candidates = Vec::new();
    for (message_index, message) in session.messages.iter().enumerate() {
        let mut ordinal = 0usize;
        for (block_index, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::Image { data, .. } => {
                    total = total.saturating_add(data.len());
                    candidates.push((message_index, block_index, ordinal, data.len()));
                    ordinal += 1;
                }
                ContentBlock::Text { text, .. } if is_archive_marker(text) => ordinal += 1,
                _ => {}
            }
        }
    }
    let mut archived = Vec::new();
    for (message_index, block_index, ordinal, length) in candidates {
        if total <= target {
            break;
        }
        let message = &session.messages[message_index];
        let ContentBlock::Image { media_type, data } = &message.content[block_index] else {
            unreachable!()
        };
        let id = image_id(&session.id, &message.id, ordinal);
        if let Err(error) = write_archived_image(&id, data) {
            crate::logging::info(&format!(
                "Cannot archive image for session {}: {error}",
                session.id
            ));
            return 0;
        }
        archived.push((message_index, block_index, archive_marker(&id, media_type)));
        total = total.saturating_sub(length);
    }
    let count = archived.len();
    for (message_index, block_index, text) in archived {
        session.messages[message_index].content[block_index] = ContentBlock::Text {
            text,
            cache_control: None,
        };
    }
    count
}

/// Move only screenshots selected by the ordinary decay budget to private,
/// durable blobs before replacing their inline payload. If writing any blob
/// fails, keep the transcript untouched rather than lose an image.
pub(super) fn archive_and_decay(session: &mut Session) -> usize {
    let mut candidates = Vec::new();
    let mut total = 0usize;
    for (message_index, message) in session.messages.iter().enumerate() {
        if !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        {
            continue;
        }
        let mut ordinal = 0usize;
        for (block_index, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::Image { data, .. } => {
                    total = total.saturating_add(data.len());
                    candidates.push((message_index, block_index, ordinal, data.len()));
                    ordinal += 1;
                }
                ContentBlock::Text { text, .. } if is_archive_marker(text) => ordinal += 1,
                _ => {}
            }
        }
    }
    if total <= jcode_compaction_core::TOOL_IMAGE_DECAY_TRIGGER_CHARS {
        return 0;
    }
    let mut kept_any = false;
    let mut kept_chars = 0usize;
    let mut archived = Vec::new();
    for (message_index, block_index, ordinal, length) in candidates.into_iter().rev() {
        if !kept_any
            || kept_chars.saturating_add(length) <= jcode_compaction_core::TOOL_IMAGE_DECAY_KEEP_CHARS
        {
            kept_any = true;
            kept_chars = kept_chars.saturating_add(length);
            continue;
        }
        let message = &session.messages[message_index];
        let ContentBlock::Image { media_type, data } = &message.content[block_index] else {
            unreachable!()
        };
        let id = image_id(&session.id, &message.id, ordinal);
        if let Err(error) = write_archived_image(&id, data) {
            crate::logging::info(&format!(
                "Cannot archive tool image for session {}: {error}",
                session.id
            ));
            return 0;
        }
        archived.push((message_index, block_index, archive_marker(&id, media_type)));
    }
    let count = archived.len();
    for (message_index, block_index, text) in archived {
        session.messages[message_index].content[block_index] = ContentBlock::Text {
            text,
            cache_control: None,
        };
    }
    count
}

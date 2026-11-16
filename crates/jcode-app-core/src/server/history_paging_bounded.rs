//! Byte-bounded backward transcript fragments. Each fragment is a slice of one
//! JSON record. The opaque cursor validates the boundary against rewind/edits.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::hash::{Hash, Hasher};

pub(super) const LAZY_IMAGE_PREFIX: &str = "jcode-lazy:";

pub(super) fn image_key(data: &str) -> String {
    format!("{:x}", Sha256::digest(data.as_bytes()))
}

/// Every record contains one rendered row and only metadata for its images.
/// Image bytes never enter a history response, even for the initial tail.
pub(super) fn records(session: &crate::session::Session) -> Result<Vec<String>, String> {
    let (messages, images, _) =
        crate::session::render_messages_and_images_with_compacted_history(session, usize::MAX);
    let mut image_rows = vec![Vec::new(); messages.len().max(1)];
    let user_indices: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.role == "user").then_some(i))
        .collect();
    for mut image in images {
        let index = if let Some(boundary) = image.history_message_index {
            let index = boundary.min(image_rows.len() - 1);
            image.history_message_index = Some(boundary.saturating_sub(index));
            index
        } else {
            match &mut image.anchor {
                Some(crate::session::RenderedImageAnchor::UserPrompt { ordinal }) => {
                    let index = user_indices
                        .get(*ordinal)
                        .copied()
                        .unwrap_or(image_rows.len() - 1);
                    index
                }
                Some(crate::session::RenderedImageAnchor::ToolCall { id }) => messages
                    .iter()
                    .position(|m| m.tool_data.as_ref().is_some_and(|tool| &tool.id == id))
                    .unwrap_or(image_rows.len() - 1),
                None => image_rows.len() - 1,
            }
        };
        if let Some(crate::session::RenderedImageAnchor::UserPrompt { ordinal }) = &mut image.anchor
        {
            *ordinal = ordinal
                .saturating_sub(user_indices.partition_point(|user_index| *user_index < index));
        }
        image.data = format!("{LAZY_IMAGE_PREFIX}{}", image_key(&image.data));
        image_rows[index].push(image);
    }
    let mut records = Vec::with_capacity(image_rows.len());
    for (index, message) in messages.into_iter().enumerate() {
        let tool = if message.role == "tool" {
            message.tool_data
        } else {
            None
        };
        records.push(serde_json::json!({
            "message": { "role": message.role, "content": message.content,
                "timestamp_ms": message.timestamp_ms, "response_stats": message.response_stats, "tool": tool },
            "images": std::mem::take(&mut image_rows[index]),
        }).to_string());
    }
    if records.is_empty() && !image_rows[0].is_empty() {
        records.push(serde_json::json!({"message":{"role":"system","content":""}, "images":image_rows.remove(0)}).to_string());
    }
    Ok(records)
}

// JSON can expand each byte to six bytes when escaped. This keeps the entire
// NDJSON frame under 4 MiB even for adversarial control characters.
const MAX_RAW_BYTES: usize = 512 * 1024;
const MAX_ROWS: usize = 128;

#[derive(Debug, Serialize)]
pub(super) struct Fragment {
    pub index: usize,
    pub start: usize,
    pub total: usize,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Page {
    pub fragments: Vec<Fragment>,
    pub before: Option<String>,
}

fn fingerprint(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn cursor(index: usize, end: usize, row: &str) -> String {
    format!("{index}:{end}:{:x}", fingerprint(row))
}

pub(super) fn page_records(
    rows: &[String],
    before: Option<&str>,
    limit: usize,
) -> Result<Page, String> {
    if rows.is_empty() {
        return Ok(Page {
            fragments: vec![],
            before: None,
        });
    }
    let (mut index, mut end) = if let Some(before) = before {
        let mut parts = before.split(':');
        let index: usize = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or("invalid cursor")?;
        let end: usize = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or("invalid cursor")?;
        let hash = parts
            .next()
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .ok_or("invalid cursor")?;
        let row = rows.get(index).ok_or("stale cursor")?;
        if parts.next().is_some()
            || end == 0
            || end > row.len()
            || !row.is_char_boundary(end)
            || fingerprint(row) != hash
        {
            return Err("stale cursor".into());
        }
        (index, end)
    } else {
        let index = rows.len() - 1;
        (index, rows[index].len())
    };
    let mut fragments = Vec::new();
    let mut budget = MAX_RAW_BYTES;
    let limit = limit.clamp(1, MAX_ROWS);
    let before = loop {
        let row = &rows[index];
        if !fragments.is_empty() && end > budget {
            break Some(cursor(index, end, row));
        }
        let mut start = end.saturating_sub(budget);
        while start < end && !row.is_char_boundary(start) {
            start += 1;
        }
        if start == end {
            break Some(cursor(index, end, row));
        }
        fragments.push(Fragment {
            index,
            start,
            total: row.len(),
            text: row[start..end].to_owned(),
        });
        budget -= end - start;
        if start > 0 {
            break Some(cursor(index, start, row));
        }
        if index == 0 {
            break None;
        }
        index -= 1;
        end = rows[index].len();
        if budget < 4 || fragments.len() >= limit {
            break Some(cursor(index, end, &rows[index]));
        }
    };
    Ok(Page { fragments, before })
}

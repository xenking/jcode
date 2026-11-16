//! Stable image identities derived from durable message ids and image ordinals.
//!
//! Old transcripts already contain message ids, so they need no rewrite. The
//! ordinal counts only image blocks, making identities independent of text
//! inserted alongside them. A session namespace prevents cross-session aliases.
use super::{ContentBlock, Session};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImageInfo {
    pub image_id: String,
    pub message_index: usize,
    pub timestamp: Option<DateTime<Utc>>,
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImage {
    #[serde(flatten)]
    pub info: SessionImageInfo,
    pub data: String,
}

pub fn image_id(session_id: &str, message_id: &str, image_ordinal: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(b"jcode-session-image-v1\0");
    hash.update((session_id.len() as u64).to_be_bytes());
    hash.update(session_id.as_bytes());
    hash.update((message_id.len() as u64).to_be_bytes());
    hash.update(message_id.as_bytes());
    hash.update((image_ordinal as u64).to_be_bytes());
    let digest = hash.finalize();
    format!(
        "img_{}",
        digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

const ARCHIVE_PREFIX: &str = "[Older tool screenshot omitted from history; jcode-archive:";

#[derive(Serialize, Deserialize)]
struct ArchiveRef {
    image_id: String,
    media_type: String,
}

pub(super) fn archive_marker(id: &str, media_type: &str) -> String {
    let reference = ArchiveRef {
        image_id: id.into(),
        media_type: media_type.into(),
    };
    format!(
        "{ARCHIVE_PREFIX}{}]",
        serde_json::to_string(&reference).expect("archive reference serializes")
    )
}

fn parse_archive_marker(text: &str) -> Option<ArchiveRef> {
    let json = text.strip_prefix(ARCHIVE_PREFIX)?.strip_suffix(']')?;
    serde_json::from_str(json).ok()
}

pub(super) fn is_archive_marker(text: &str) -> bool {
    parse_archive_marker(text).is_some()
}

pub(super) fn archived_image_media_type(text: &str, id: &str) -> Option<String> {
    let reference = parse_archive_marker(text)?;
    (reference.image_id == id).then_some(reference.media_type)
}

fn archive_path(id: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(
        id.len() == 36 && id.starts_with("img_") && id[4..].bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid image ID"
    );
    Ok(crate::storage::jcode_dir()?
        .join("session-image-blobs")
        .join(id))
}

pub(super) fn write_archived_image(id: &str, data: &str) -> anyhow::Result<()> {
    let path = archive_path(id)?;
    if std::fs::read_to_string(&path).ok().as_deref() != Some(data) {
        crate::storage::write_text_secret(&path, data)?;
    }
    Ok(())
}

fn read_archived_image(id: &str) -> Option<String> {
    std::fs::read_to_string(archive_path(id).ok()?).ok()
}

fn remap_ask_images(content: &str, labels: &[(String, String)]) -> String {
    if !content.starts_with("User answers:\n") {
        return content.to_owned();
    }
    let mut result = content.to_owned();
    for (label, id) in labels {
        let Some(label) = label
            .strip_prefix("[Attached image associated with the preceding tool result: Question ")
            .and_then(|s| s.strip_suffix(']'))
        else {
            continue;
        };
        let Some((question, detail)) = label.split_once(' ') else {
            continue;
        };
        let Some((kind, number)) = detail.rsplit_once(", image ") else {
            continue;
        };
        if !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let field = if kind == "free answer" {
            "Other: ".to_owned()
        } else if let Some(option) = kind
            .strip_prefix("option ")
            .and_then(|s| s.strip_suffix(" comment"))
        {
            let Ok(option) = serde_json::from_str::<String>(option) else {
                continue;
            };
            format!("{option} (comment: ")
        } else {
            continue;
        };
        let prefix = format!("{question}. ");
        let mut offset = 0;
        for line in result.split_inclusive('\n') {
            if line.starts_with(&prefix) {
                if let Some(field_start) = line.find(&field) {
                    let start = field_start + field.len();
                    let end = line[start..].find("; ").map_or(line.len(), |i| start + i);
                    let old = format!("[#image {number}]");
                    if let Some(pos) = line[start..end].find(&old) {
                        let pos = offset + start + pos;
                        result.replace_range(pos..pos + old.len(), &format!("[#{id}]"));
                    }
                }
                break;
            }
            offset += line.len();
        }
    }
    result
}

/// Ensure model-visible IDs are persisted alongside the image blocks. Mutating
/// a uniquely owned content buffer does not clone the image payload.
pub(super) fn annotate_message(session_id: &str, stored: &mut super::StoredMessage) -> bool {
    let mut ordinal = 0usize;
    let mut ids = Vec::new();
    let mut labels = Vec::new();
    for block in &stored.content {
        match block {
            ContentBlock::Image { .. } => {
                ids.push(image_id(session_id, &stored.id, ordinal));
                ordinal += 1;
            }
            ContentBlock::Text { text, .. } if is_archive_marker(text) => ordinal += 1,
            ContentBlock::Text { text, .. }
                if text.starts_with(
                    "[Attached image associated with the preceding tool result: Question ",
                ) =>
            {
                if let Some(id) = ids.last() {
                    labels.push((text.clone(), id.clone()));
                }
            }
            _ => {}
        }
    }
    if ids.is_empty() {
        return false;
    }
    let already_marked = ids.iter().all(|id| {
        let marker = format!("[#{id}]");
        stored.content.iter().any(|block| match block {
            ContentBlock::Text { text, .. } => text.contains(&marker),
            ContentBlock::ToolResult { content, .. } => content.contains(&marker),
            _ => false,
        })
    });
    let needs_ask_remap = !labels.is_empty()
        && stored.content.iter().any(|block| match block {
            ContentBlock::ToolResult { content, .. } if content.starts_with("User answers:\n") => {
                remap_ask_images(content, &labels) != *content
            }
            _ => false,
        });
    if already_marked && !needs_ask_remap {
        return false;
    }
    let has_tool_result = stored
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    let mut mapped = vec![false; ids.len()];
    for block in &mut stored.content {
        if !has_tool_result {
            if let ContentBlock::Text { text, .. } = block {
                for (index, id) in ids.iter().enumerate() {
                    for old in [
                        format!("[#image {}]", index + 1),
                        format!("[image {}]", index + 1),
                    ] {
                        if text.contains(&old) {
                            *text = text.replace(&old, &format!("[#{id}]"));
                            mapped[index] = true;
                        }
                    }
                }
            }
        } else if let ContentBlock::ToolResult { content, .. } = block {
            if !labels.is_empty() && content.starts_with("User answers:\n") {
                *content = remap_ask_images(content, &labels);
            }
            let missing = ids
                .iter()
                .filter(|id| !content.contains(&format!("[#{id}]")))
                .map(|id| format!("[#{id}]"))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                content.push_str("\n\nAttached session images: ");
                content.push_str(&missing.join(", "));
            }
            mapped.fill(true);
        }
    }
    let remaining = ids
        .iter()
        .zip(mapped)
        .filter_map(|(id, mapped)| (!mapped).then(|| format!("[#{id}]")))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        stored.content.push(ContentBlock::Text {
            text: format!("Attached session images: {}", remaining.join(", ")),
            cache_control: None,
        });
    }
    true
}

/// Legacy messages not yet migrated can still produce a marked provider view.
pub(super) fn provider_message(session_id: &str, stored: &super::StoredMessage) -> super::Message {
    let mut message = stored.clone();
    annotate_message(session_id, &mut message);
    message.to_message()
}

impl Session {
    pub fn session_images(&self) -> Vec<SessionImageInfo> {
        let mut result = Vec::new();
        for (message_index, message) in self.messages.iter().enumerate() {
            let mut ordinal = 0;
            for block in &message.content {
                let media_type = match block {
                    ContentBlock::Image { media_type, .. } => Some(media_type.clone()),
                    ContentBlock::Text { text, .. } => {
                        parse_archive_marker(text).and_then(|reference| {
                            (reference.image_id == image_id(&self.id, &message.id, ordinal))
                                .then_some(reference.media_type)
                        })
                    }
                    _ => None,
                };
                if let Some(media_type) = media_type {
                    result.push(SessionImageInfo {
                        image_id: image_id(&self.id, &message.id, ordinal),
                        message_index,
                        timestamp: message.timestamp,
                        media_type,
                    });
                    ordinal += 1;
                }
            }
        }
        result
    }

    pub fn session_image(&self, id: &str) -> Option<SessionImage> {
        for (message_index, message) in self.messages.iter().enumerate() {
            let mut ordinal = 0;
            for block in &message.content {
                let candidate = image_id(&self.id, &message.id, ordinal);
                let found = match block {
                    ContentBlock::Image { media_type, data } => {
                        Some((media_type.clone(), (candidate == id).then(|| data.clone())))
                    }
                    ContentBlock::Text { text, .. } => {
                        parse_archive_marker(text).and_then(|reference| {
                            (reference.image_id == candidate).then(|| {
                                (
                                    reference.media_type,
                                    (candidate == id)
                                        .then(|| read_archived_image(&candidate))
                                        .flatten(),
                                )
                            })
                        })
                    }
                    _ => None,
                };
                if let Some((media_type, data)) = found {
                    if candidate == id {
                        return Some(SessionImage {
                            info: SessionImageInfo {
                                image_id: candidate,
                                message_index,
                                timestamp: message.timestamp,
                                media_type,
                            },
                            data: data?,
                        });
                    }
                    ordinal += 1;
                }
            }
        }
        None
    }
}

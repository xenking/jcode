//! Stable image identities derived from durable message ids and image ordinals.
//!
//! Old transcripts already contain message ids, so they need no rewrite. The
//! ordinal counts only image blocks, making identities independent of text
//! inserted alongside them. A session namespace prevents cross-session aliases.
use super::{ContentBlock, Session};
use chrono::{DateTime, Utc};
use jcode_session_types::SessionImageRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImageInfo {
    pub image_id: String,
    /// One-based order in the complete session, independent of the visible history window.
    #[serde(default)]
    pub ordinal: usize,
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
const IMAGE_ANNOTATION_PREFIX: &str = "Attached session images: ";

pub(crate) fn is_image_annotation(text: &str) -> bool {
    text.strip_prefix(IMAGE_ANNOTATION_PREFIX)
        .is_some_and(|ids| {
            ids.split(", ").all(|marker| {
                marker
                    .strip_prefix("[#img_")
                    .and_then(|id| id.strip_suffix(']'))
                    .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            })
        })
}

#[derive(Serialize, Deserialize)]
struct ArchiveRef {
    image_id: String,
    media_type: String,
}

pub(super) fn archive_marker(id: &str, media_type: &str) -> String {
    let reference = serde_json::Value::Object(serde_json::Map::from_iter([
        ("image_id".into(), serde_json::Value::String(id.into())),
        (
            "media_type".into(),
            serde_json::Value::String(media_type.into()),
        ),
    ]));
    format!("{ARCHIVE_PREFIX}{reference}]")
}

fn parse_archive_marker(text: &str) -> Option<ArchiveRef> {
    let json = text.strip_prefix(ARCHIVE_PREFIX)?.strip_suffix(']')?;
    // A prefix-like prose fragment is not an archive reference unless the
    // complete typed payload validates. Leave such text untouched.
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
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == data => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    crate::storage::write_text_secret(&path, data)?;
    Ok(())
}

fn read_archived_image(id: &str) -> Option<String> {
    let result = archive_path(id).and_then(|path| Ok(std::fs::read_to_string(path)?));
    match result {
        Ok(data) => Some(data),
        Err(error) => {
            crate::logging::warn(&format!("Cannot read archived session image {id}: {error}"));
            None
        }
    }
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
                    let mut depth = 0usize;
                    for ch in line[..start].chars() {
                        match ch {
                            '(' => depth += 1,
                            ')' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                    let mut end = line.len();
                    for (relative, ch) in line[start..].char_indices() {
                        let index = start + relative;
                        match ch {
                            '(' => depth += 1,
                            ')' => depth = depth.saturating_sub(1),
                            ';' if depth == 0 && line[index..].starts_with("; ") => {
                                end = index;
                                break;
                            }
                            _ => {}
                        }
                    }
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
                content.push_str("\n\n");
                content.push_str(IMAGE_ANNOTATION_PREFIX);
                content.push_str(&missing.join(", "));
            }
            mapped.fill(true);
        }
    }
    let remaining = ids
        .iter()
        .zip(mapped)
        .filter(|(_, mapped)| !mapped)
        .map(|(id, _)| format!("[#{id}]"))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        stored.content.push(ContentBlock::Text {
            text: format!("{IMAGE_ANNOTATION_PREFIX}{}", remaining.join(", ")),
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

struct IndexedImage {
    info: SessionImageInfo,
    block_index: usize,
}

/// Derived from the durable transcript, never from a provider projection or UI page.
/// Aliases preserve lookups for IDs recorded before re-reads shared an identity.
#[derive(Default)]
pub(super) struct SessionImageIndex {
    entries: Vec<IndexedImage>,
    by_id: HashMap<String, usize>,
}

impl SessionImageIndex {
    fn get(&self, selector: &str) -> Option<&IndexedImage> {
        let index = if let Some(index) = self.by_id.get(selector) {
            *index
        } else {
            selector.parse::<usize>().ok()?.checked_sub(1)?
        };
        self.entries.get(index)
    }

    pub(super) fn reference(&self, id: &str) -> Option<SessionImageRef> {
        let entry = self.get(id)?;
        Some(SessionImageRef {
            image_id: entry.info.image_id.clone(),
            ordinal: entry.info.ordinal,
        })
    }
}

impl Session {
    pub(super) fn image_index(&self) -> SessionImageIndex {
        let mut index = SessionImageIndex::default();
        let mut reads = HashMap::new();
        for (message_index, message) in self.messages.iter().enumerate() {
            let mut local_ordinal = 0;
            let mut read = None;
            for (block_index, block) in message.content.iter().enumerate() {
                let media_type = match block {
                    ContentBlock::Image { media_type, .. } => media_type.clone(),
                    ContentBlock::Text { text, .. } => {
                        let Some(reference) = parse_archive_marker(text) else {
                            continue;
                        };
                        if reference.image_id != image_id(&self.id, &message.id, local_ordinal) {
                            continue;
                        }
                        reference.media_type
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } if name == "read" => {
                        if let Some(selector) = input
                            .get("file_path")
                            .and_then(|value| value.as_str())
                            .and_then(|path| path.strip_prefix("session-image://"))
                        {
                            reads.insert(id.as_str(), selector);
                        }
                        continue;
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let returned_id = content
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("Session image [#"))
                            .and_then(|line| line.strip_suffix(']'));
                        read = reads
                            .remove(tool_use_id.as_str())
                            .filter(|_| !is_error.unwrap_or(false))
                            .and_then(|selector| index.get(selector))
                            .filter(|original| {
                                returned_id.and_then(|id| index.by_id.get(id)).copied()
                                    == Some(original.info.ordinal - 1)
                            })
                            .map(|original| original.info.ordinal - 1);
                        continue;
                    }
                    _ => continue,
                };
                let id = image_id(&self.id, &message.id, local_ordinal);
                local_ordinal += 1;
                if let Some(entry_index) = read.take() {
                    index.by_id.insert(id, entry_index);
                    continue;
                }
                let entry_index = index.entries.len();
                index.by_id.insert(id.clone(), entry_index);
                index.entries.push(IndexedImage {
                    info: SessionImageInfo {
                        image_id: id,
                        ordinal: entry_index + 1,
                        message_index,
                        timestamp: message.timestamp,
                        media_type,
                    },
                    block_index,
                });
            }
        }
        index
    }

    pub fn session_images(&self) -> Vec<SessionImageInfo> {
        self.image_index()
            .entries
            .into_iter()
            .map(|entry| entry.info)
            .collect()
    }

    pub fn session_image_references_for_message(
        &self,
        message_id: &str,
        ordinals: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (String, Option<SessionImageRef>)> + use<> {
        let index = self.image_index();
        let session_id = self.id.clone();
        let message_id = message_id.to_owned();
        ordinals.map(move |ordinal| {
            let id = image_id(&session_id, &message_id, ordinal);
            let reference = index.reference(&id);
            (id, reference)
        })
    }

    /// Accept an immutable image ID or a one-based session ordinal.
    pub fn session_image(&self, selector: &str) -> Option<SessionImage> {
        let index = self.image_index();
        let entry = index.get(selector)?;
        let block = self
            .messages
            .get(entry.info.message_index)?
            .content
            .get(entry.block_index)?;
        let data = match block {
            ContentBlock::Image { data, .. } => data.clone(),
            ContentBlock::Text { .. } => read_archived_image(&entry.info.image_id)?,
            _ => return None,
        };
        Some(SessionImage {
            info: entry.info.clone(),
            data,
        })
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    #[test]
    fn unreadable_archive_is_not_overwritten() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        struct RestoreHome(Option<std::ffi::OsString>);
        impl Drop for RestoreHome {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(home) => crate::env::set_var("JCODE_HOME", home),
                    None => crate::env::remove_var("JCODE_HOME"),
                }
            }
        }
        let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
        crate::env::set_var("JCODE_HOME", temp.path());
        let id = image_id("archive-read-error", "message", 0);
        let path = archive_path(&id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xff]).unwrap();
        assert!(write_archived_image(&id, "replacement").is_err());
        assert_eq!(std::fs::read(path).unwrap(), [0xff]);
    }

    #[test]
    fn archive_marker_escapes_reference_fields() {
        let marker = archive_marker("quote\"\\ёж", "image/\"test\n");
        let reference = parse_archive_marker(&marker).unwrap();
        assert_eq!(reference.image_id, "quote\"\\ёж");
        assert_eq!(reference.media_type, "image/\"test\n");
    }
}

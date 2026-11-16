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

impl Session {
    pub fn session_images(&self) -> Vec<SessionImageInfo> {
        self.messages
            .iter()
            .enumerate()
            .flat_map(|(message_index, message)| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { media_type, .. } => Some(media_type),
                        _ => None,
                    })
                    .enumerate()
                    .map(move |(ordinal, media_type)| SessionImageInfo {
                        image_id: image_id(&self.id, &message.id, ordinal),
                        message_index,
                        timestamp: message.timestamp,
                        media_type: media_type.clone(),
                    })
            })
            .collect()
    }

    pub fn session_image(&self, id: &str) -> Option<SessionImage> {
        for (message_index, message) in self.messages.iter().enumerate() {
            let mut ordinal = 0;
            for block in &message.content {
                if let ContentBlock::Image { media_type, data } = block {
                    let candidate = image_id(&self.id, &message.id, ordinal);
                    if candidate == id {
                        return Some(SessionImage {
                            info: SessionImageInfo {
                                image_id: candidate,
                                message_index,
                                timestamp: message.timestamp,
                                media_type: media_type.clone(),
                            },
                            data: data.clone(),
                        });
                    }
                    ordinal += 1;
                }
            }
        }
        None
    }
}

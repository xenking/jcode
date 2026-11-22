use serde::{Deserialize, Serialize};

/// Attachment-independent identity and position of text in a daemon turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantTextFrame {
    /// The bytes come from the accompanying legacy TextDelta.
    Append {
        message_id: String,
        offset: usize,
    },
    Replace {
        message_id: String,
        text: String,
    },
    /// Late attachments recover missed bytes from the absolute final text.
    Complete {
        message_id: String,
        text: String,
    },
}

//! Cross-session full-text search results.
//!
//! One query spans every stored session. The reply is split into the two
//! sections a client renders separately: session titles that match, and
//! transcript messages that match. Splitting them here (rather than as a flat
//! list with a tag) means a client that only wants titles never has to filter
//! a thousand message hits, and the ranking of each section is decided once,
//! on the side that has the whole corpus.

use serde::{Deserialize, Serialize};

/// A byte-free span of highlighted characters in [`SessionSearchHit::snippet`].
///
/// Offsets are Unicode scalar (char) indices, not bytes, so a client can slice
/// the string without re-deriving grapheme boundaries from a byte count that
/// would be wrong for Cyrillic or CJK text. Ranges never overlap and are
/// sorted by `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchHighlight {
    pub start: u32,
    pub end: u32,
}

/// Which half of the corpus a hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchRole {
    /// The session title matched the query.
    Title,
    /// A user prompt matched the query.
    User,
    /// Assistant final text matched the query.
    Assistant,
}

impl SessionSearchRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One matching session title or transcript message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    /// The session's current title, so a client can label every row without a
    /// second lookup. Empty when the session has never been titled.
    pub title: String,
    pub role: SessionSearchRole,
    /// The model that produced an assistant hit, as the session recorded it
    /// (`claude-opus-5-5`, `deepseek-v4.1-flash`, ...). Absent for user and
    /// title hits, and for sessions stored before models were indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// One-line excerpt around the best match, with [`Self::highlights`]
    /// marking the matched terms inside it.
    pub snippet: String,
    pub highlights: Vec<SessionSearchHighlight>,
    /// When this hit happened: the message's timestamp for transcript hits,
    /// the session's last activity for title hits. Unix epoch milliseconds.
    pub timestamp_ms: i64,
    /// The transcript message id, for scrolling the opened session to it.
    /// Absent for title hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// Reply payload for `SearchSessions`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchResults {
    /// The query these results answer, echoed so a client can drop results for
    /// a query the user has already replaced.
    pub query: String,
    /// Title matches, best first.
    pub titles: Vec<SessionSearchHit>,
    /// Transcript matches, best first.
    pub messages: Vec<SessionSearchHit>,
    /// Sessions in the index when the query ran. Lets a client distinguish
    /// "no matches" from "nothing indexed yet".
    pub indexed_sessions: u64,
    /// At least one section was cut short by the limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

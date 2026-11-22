use std::{fs, io::BufReader};

use anyhow::{Result, anyhow, ensure};
use jcode_session_types::{RenderedMessage, SIDE_QUESTION_VISIBILITY_DIR, SideQuestionTopic};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};

use super::Session;

impl Session {
    /// Mark a new child after copying its inherited messages, before its first turn.
    pub fn mark_side_question(&mut self) {
        debug_assert!(self.parent_id.is_some());
        self.hidden = true;
        self.side_question_message_start = Some(self.messages.len());
        self.autoreview_enabled = Some(false);
        self.autojudge_enabled = Some(false);
    }

    pub fn save_new_side_question(&mut self) -> Result<()> {
        self.mark_side_question();
        let visibility = visibility_path(&self.id)?;
        crate::storage::write_json(&visibility, &true)?;
        if let Err(error) = self.save_prepared() {
            let _ = fs::remove_file(visibility);
            return Err(error);
        }
        Ok(())
    }

    /// Promotion only changes visibility. A live turn may still own the transcript.
    pub fn promote_side_question(session_id: &str) -> Result<()> {
        let path = visibility_path(session_id)?;
        ensure!(
            super::session_path(session_id)?.is_file(),
            "Unknown side-question session"
        );
        if !path.is_file() {
            let session = Self::load_startup_stub(session_id)?;
            ensure!(
                session.side_question_topic().is_some(),
                "Not a side-question session"
            );
        }
        crate::storage::write_json(&path, &false)
    }

    pub fn side_question_was_promoted(session_id: &str) -> bool {
        visibility_path(session_id)
            .ok()
            .and_then(|path| fs::read(path).ok())
            .is_some_and(|bytes| bytes == b"false")
    }

    pub fn is_hidden_side_question(&self) -> bool {
        self.hidden && self.parent_id.is_some() && !Self::side_question_was_promoted(&self.id)
    }

    pub fn side_question_topic(&self) -> Option<SideQuestionTopic> {
        if !self.hidden && self.side_question_message_start.is_none() {
            return None;
        }
        Some(SideQuestionTopic {
            session_id: self.id.clone(),
            parent_session_id: self.parent_id.clone()?,
            created_at_ms: self.created_at.timestamp_millis(),
            message_start: self.side_question_message_start,
        })
    }

    pub fn list_side_questions(parent_session_id: &str) -> Result<Vec<SideQuestionTopic>> {
        let dir = crate::storage::jcode_dir()?.join("sessions");
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut topics = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json")
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_snapshot_child(&path, parent_session_id) {
                continue;
            }
            if let Ok(session) = Self::load_startup_stub(id)
                && session.id == id
                && let Some(topic) = session.side_question_topic()
                && topic.parent_session_id == parent_session_id
            {
                topics.push(topic);
            }
        }
        topics.sort_unstable_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(topics)
    }

    /// Project only the child's own conversation. An old fork with no stored
    /// boundary cannot distinguish its conversation from the inherited context.
    pub fn side_question_history(mut self) -> Result<Vec<RenderedMessage>> {
        let start = self.side_question_message_start.ok_or_else(|| {
            anyhow!("This session has no recorded side-question history boundary")
        })?;
        ensure!(
            start <= self.messages.len(),
            "Side-question history boundary is out of range"
        );
        self.messages.drain(..start);
        self.compaction = None;
        Ok(super::render::render_messages(&self)
            .into_iter()
            .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
            .collect())
    }
}

fn visibility_path(id: &str) -> Result<std::path::PathBuf> {
    validate_side_question_session_id(id)?;
    Ok(crate::storage::jcode_dir()?
        .join("state")
        .join(SIDE_QUESTION_VISIBILITY_DIR)
        .join(format!("{id}.json")))
}

pub(super) fn validate_side_question_session_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
            && !matches!(id, "." | ".."),
        "Invalid side-question session ID",
    );
    Ok(())
}

pub fn load_side_question_history(id: &str) -> Result<Vec<RenderedMessage>> {
    validate_side_question_session_id(id)?;
    Session::load(id)?.side_question_history()
}

/// The native snapshot writes the top-level parent before the transcript.
/// Stop there: a history lookup must not read unrelated conversation bodies.
fn is_snapshot_child(path: &std::path::Path, parent_session_id: &str) -> bool {
    struct ParentMatch<'a> {
        parent: &'a str,
        matched: &'a mut bool,
    }
    impl<'de> DeserializeSeed<'de> for ParentMatch<'_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for ParentMatch<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a session snapshot")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == "parent_id" {
                    *self.matched =
                        map.next_value::<Option<String>>()?.as_deref() == Some(self.parent);
                    break;
                }
                if key == "messages" {
                    break;
                }
                map.next_value::<IgnoredAny>()?;
            }
            Err(serde::de::Error::custom("snapshot header read"))
        }
    }
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut matched = false;
    let _ = ParentMatch {
        parent: parent_session_id,
        matched: &mut matched,
    }
    .deserialize(&mut serde_json::Deserializer::from_reader(BufReader::new(
        file,
    )));
    matched
}

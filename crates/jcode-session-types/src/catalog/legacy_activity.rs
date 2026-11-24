use chrono::{DateTime, Utc};
use serde::{
    Deserialize, Deserializer,
    de::{SeqAccess, Visitor},
};
use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::Path,
};

use super::{SessionCatalogActivity, SessionOrigin, counts_as_catalog_text, legacy_agent_session};

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    display_role: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    content: Vec<Block>,
}

#[derive(Default)]
struct MessageFacts {
    reply: Option<DateTime<Utc>>,
    last_message: Option<DateTime<Utc>>,
    scheduled: Option<DateTime<Utc>>,
}

impl<'de> Deserialize<'de> for MessageFacts {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Scan;
        impl<'de> Visitor<'de> for Scan {
            type Value = MessageFacts;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of stored messages")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut facts = MessageFacts::default();
                while let Some(message) = seq.next_element::<Message>()? {
                    let role = message.role.as_str();
                    if matches!(role, "user" | "assistant")
                        && matches!(message.display_role.as_deref(), None | Some("jarvis_user"))
                        && message
                            .content
                            .iter()
                            .any(|block| match block.kind.as_deref() {
                                Some("image") => true,
                                Some("text") | None => block.text.as_deref().is_some_and(|text| {
                                    counts_as_catalog_text(role == "user", text)
                                }),
                                _ => false,
                            })
                    {
                        facts.last_message = facts.last_message.max(message.timestamp);
                    }
                    match role {
                        "assistant" => facts.reply = facts.reply.max(message.timestamp),
                        "user"
                            if message.content.iter().any(|block| {
                                block.text.as_deref().is_some_and(|text| {
                                    text.trim_start().starts_with("[Scheduled task]\n")
                                })
                            }) =>
                        {
                            facts.scheduled = facts.scheduled.max(message.timestamp)
                        }
                        _ => {}
                    }
                }
                Ok(facts)
            }
        }
        de.deserialize_seq(Scan)
    }
}

#[derive(Deserialize)]
struct Snapshot {
    parent_id: Option<String>,
    title: Option<String>,
    created_at: Option<DateTime<Utc>>,
    system_prompt: Option<String>,
    side_question_message_start: Option<usize>,
    #[serde(default)]
    agent_started: bool,
    #[serde(default)]
    is_swarm_worker: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    messages: MessageFacts,
}

#[derive(Deserialize)]
struct Append {
    #[serde(default)]
    append_messages: MessageFacts,
}

/// Read old records once to populate the catalog index. Callers budget the
/// snapshot size before using this path. The journal is size-capped by storage.
pub fn read_legacy_catalog_activity(path: &Path) -> io::Result<SessionCatalogActivity> {
    let file = BufReader::new(File::open(path)?);
    let snapshot: Snapshot = serde_json::from_reader(file).map_err(io::Error::other)?;
    let mut facts = snapshot.messages;
    let journal = path.with_extension("journal.jsonl");
    if let Ok(file) = File::open(journal) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            // Ignore an incomplete final append while a writer is active.
            if let Ok(append) = serde_json::from_str::<Append>(&line) {
                facts.reply = facts.reply.max(append.append_messages.reply);
                facts.last_message = facts.last_message.max(append.append_messages.last_message);
                facts.scheduled = facts.scheduled.max(append.append_messages.scheduled);
            }
        }
    }
    let has_parent = snapshot.parent_id.is_some();
    let belongs_to_session = |time: &DateTime<Utc>| {
        !has_parent || snapshot.created_at.is_none_or(|created| *time >= created)
    };
    let is_agent = snapshot.agent_started
        || snapshot.is_swarm_worker
        || snapshot.hidden
        || snapshot.side_question_message_start.is_some()
        || legacy_agent_session(
            has_parent,
            snapshot.title.as_deref(),
            snapshot.system_prompt.as_deref(),
        )
        || (has_parent && facts.scheduled.as_ref().is_some_and(belongs_to_session));
    Ok(SessionCatalogActivity {
        origin: if is_agent {
            SessionOrigin::Agent
        } else {
            SessionOrigin::User
        },
        last_agent_activity_at_ms: facts
            .reply
            .filter(belongs_to_session)
            .map(|time| time.timestamp_millis()),
        last_message_at_ms: Some(
            facts
                .last_message
                .filter(belongs_to_session)
                .map(|time| time.timestamp_millis()),
        ),
    })
}

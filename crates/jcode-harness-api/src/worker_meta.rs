//! Per-session facts read from a session's own record.
//!
//! The catalog knows a session exists; its record knows what it runs on and
//! what it has spent. A swarm worker is another session on the same machine,
//! so its coordinator's sidebar can only show a model, an effort and a token
//! count if they are read from the worker's record. This module is that read:
//! snapshot plus journal, newest metadata wins, absent fields stay absent.
//!
//! It is deliberately dependency-free and cached by file stamp: a catalog
//! refresh touches every listed session, and a transcript with inline images
//! is megabytes, so an unchanged worker must cost two `stat` calls.

use crate::{SessionInfo, WorkerMeta, WorkerPhase};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Jcode home for this process: `$JCODE_HOME`, else `$HOME/.jcode`.
pub(crate) fn local_home() -> Option<PathBuf> {
    std::env::var_os("JCODE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".jcode"))
        })
}

/// Read the records of every session whose id is a plain session id, and
/// return what each one says. Sessions without a readable record are simply
/// absent, never zero-filled.
pub fn worker_meta_for(home: &Path, session_ids: &[String]) -> HashMap<String, WorkerMeta> {
    let mut out = HashMap::new();
    for session_id in session_ids {
        if !is_plain_session_id(session_id) {
            continue;
        }
        if let Some(meta) = worker_meta(home, session_id) {
            out.insert(session_id.clone(), meta);
        }
    }
    out
}

/// Like [`worker_meta_for`], but only for sessions that are swarm workers
/// (they name a different parent) and using the local jcode home.
pub fn local_worker_meta(sessions: &[SessionInfo]) -> HashMap<String, WorkerMeta> {
    let Some(home) = local_home() else {
        return HashMap::new();
    };
    let ids = worker_ids(sessions);
    if ids.is_empty() {
        return HashMap::new();
    }
    worker_meta_for(&home, &ids)
}

/// Ids of sessions that belong to a swarm coordinator.
pub fn worker_ids(sessions: &[SessionInfo]) -> Vec<String> {
    let mut seen = HashSet::new();
    sessions
        .iter()
        .filter(|session| {
            session
                .parent_session_id
                .as_deref()
                .is_some_and(|parent| parent != session.session_id)
        })
        .map(|session| session.session_id.clone())
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// A session id is interpolated into a path, so anything else is refused.
fn is_plain_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// What `home/sessions/<id>.json` plus its journal say about the session.
/// `None` when neither file exists.
pub fn worker_meta(home: &Path, session_id: &str) -> Option<WorkerMeta> {
    if !is_plain_session_id(session_id) {
        return None;
    }
    let stamp = stamp(home, session_id);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    let key = home.join(session_id);
    if let Some(stamp) = stamp
        && let Some((seen, meta)) = cache.get(&key)
        && *seen == stamp
    {
        return Some(meta.clone());
    }
    let meta = read(home, session_id);
    if let (Some(stamp), Some(meta)) = (stamp, &meta) {
        cache.insert(key, (stamp, meta.clone()));
    }
    meta
}

type Stamp = (u64, Option<SystemTime>, u64, Option<SystemTime>);

static CACHE: Mutex<Option<HashMap<PathBuf, (Stamp, WorkerMeta)>>> = Mutex::new(None);

fn stamp(home: &Path, session_id: &str) -> Option<Stamp> {
    let meta = |name: String| match std::fs::metadata(home.join("sessions").join(name)) {
        Ok(metadata) => metadata.modified().map(|time| (metadata.len(), Some(time))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((0, None)),
        Err(error) => Err(error),
    };
    match (
        meta(format!("{session_id}.json")),
        meta(format!("{session_id}.journal.jsonl")),
    ) {
        (Ok((a, am)), Ok((b, bm))) => Some((a, am, b, bm)),
        _ => None, // An unreadable stamp never validates a cached record.
    }
}

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    /// Prompt tokens the provider reported. Providers that report
    /// `prompt_tokens` already folded the cache in; the others split it, and
    /// the cache read/write counts belong to the prompt side.
    fn prompt(&self) -> u64 {
        self.prompt_tokens.unwrap_or_else(|| {
            self.input_tokens
                .unwrap_or(0)
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
        })
    }
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    token_usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
struct Snapshot {
    #[serde(flatten)]
    meta: Meta,
    #[serde(default)]
    messages: Vec<Message>,
}

#[derive(Deserialize, Default)]
struct JournalLine {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    append_messages: Vec<Message>,
}

/// Running totals plus the newest message, so one pass serves both.
#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    /// True once any message reported a prompt or output count, so a session
    /// with no usage telemetry never claims to have spent nothing.
    seen: bool,
    last: Option<Phase>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Phase {
    phase: WorkerPhase,
    tool: Option<String>,
}

impl Totals {
    fn add(&mut self, message: &Message) {
        if let Some(usage) = message.token_usage.as_ref() {
            let prompt = usage.prompt();
            let output = usage.output_tokens.unwrap_or(0);
            self.input = self.input.saturating_add(prompt);
            self.output = self.output.saturating_add(output);
            self.cache_read = self
                .cache_read
                .saturating_add(usage.cache_read_input_tokens.unwrap_or(0));
            self.cache_creation = self
                .cache_creation
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
            self.seen |= prompt > 0 || output > 0;
        }
        if let Some(phase) = message_phase(message) {
            self.last = Some(phase);
        }
    }
}

fn message_phase(message: &Message) -> Option<Phase> {
    let role = message.role.as_deref().unwrap_or_default();
    let parts = match message.content.as_ref()? {
        serde_json::Value::Array(parts) => parts.as_slice(),
        // Older records stored the message as one string.
        serde_json::Value::String(_) => &[],
        _ => return None,
    };
    let mut kinds = Vec::with_capacity(parts.len());
    let mut tool = None;
    for part in parts {
        let Some(kind) = part.get("type").and_then(|kind| kind.as_str()) else {
            continue;
        };
        if kind == "tool_use" && tool.is_none() {
            tool = part
                .get("name")
                .and_then(|name| name.as_str())
                .map(|name| name.trim_start_matches("functions.").to_owned());
        }
        kinds.push(kind);
    }
    // An unanswered tool call is the worker's tool running right now. A tool
    // result in the newest message means the model is about to see it again.
    if kinds.contains(&"tool_use") {
        return Some(Phase {
            phase: WorkerPhase::Tool,
            tool,
        });
    }
    if kinds.contains(&"tool_result") {
        return Some(Phase {
            phase: WorkerPhase::Thinking,
            tool: None,
        });
    }
    if role == "assistant" {
        return Some(Phase {
            phase: WorkerPhase::Thinking,
            tool: None,
        });
    }
    if role == "user" {
        // A prompt just landed; the turn has not produced anything yet.
        return Some(Phase {
            phase: WorkerPhase::Thinking,
            tool: None,
        });
    }
    None
}

fn read(home: &Path, session_id: &str) -> Option<WorkerMeta> {
    match read_checked(home, session_id) {
        Ok(meta) => meta,
        Err(error) => {
            eprintln!("harness worker metadata unavailable for {session_id}: {error}");
            None
        }
    }
}

fn read_checked(home: &Path, session_id: &str) -> std::io::Result<Option<WorkerMeta>> {
    let dir = home.join("sessions");
    let snapshot: Option<Snapshot> = match std::fs::read(dir.join(format!("{session_id}.json"))) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(std::io::Error::other)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let journal = match std::fs::read_to_string(dir.join(format!("{session_id}.journal.jsonl"))) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for (index, line) in journal.split_inclusive('\n').enumerate() {
        match serde_json::from_str::<JournalLine>(line) {
            Ok(entry) => entries.push(entry),
            // A writer may be appending the final line. Do not cache or publish
            // partial usage totals until the record is complete.
            Err(error) => {
                return Err(std::io::Error::other(format!(
                    "journal line {}: {error}",
                    index + 1
                )));
            }
        }
    }
    let journal = entries;
    if snapshot.is_none() && journal.is_empty() {
        return Ok(None);
    }
    // Journal-only records are supported; a missing snapshot is not a read error.
    let snapshot = snapshot.unwrap_or_default();
    let mut meta = WorkerMeta {
        model: clean(snapshot.meta.model),
        provider: clean(snapshot.meta.provider_key),
        effort: clean(snapshot.meta.reasoning_effort),
        ..WorkerMeta::default()
    };
    let mut totals = Totals::default();
    for message in &snapshot.messages {
        totals.add(message);
    }
    // The journal is newer than the snapshot: its metadata wins, and its
    // appended messages extend the totals.
    let mut closed = snapshot
        .meta
        .status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("closed"));
    for line in &journal {
        meta.model = clean(line.meta.model.clone()).or(meta.model);
        meta.provider = clean(line.meta.provider_key.clone()).or(meta.provider);
        meta.effort = clean(line.meta.reasoning_effort.clone()).or(meta.effort);
        if let Some(status) = line.meta.status.as_deref() {
            closed = status.eq_ignore_ascii_case("closed");
        }
        for message in &line.append_messages {
            totals.add(message);
        }
    }
    if totals.seen {
        meta.input_tokens = Some(totals.input);
        meta.output_tokens = Some(totals.output);
        if totals.cache_read > 0 {
            meta.cache_read_tokens = Some(totals.cache_read);
        }
        if totals.cache_creation > 0 {
            meta.cache_creation_tokens = Some(totals.cache_creation);
        }
    }
    // A closed session is not doing anything, whatever its last message was.
    let (phase, tool) = if closed {
        (WorkerPhase::Idle, None)
    } else {
        match totals.last {
            Some(phase) => (phase.phase, phase.tool),
            None => (WorkerPhase::Idle, None),
        }
    };
    meta.phase = Some(phase);
    meta.tool = tool;
    // The record's own write time is the newest activity: the journal is
    // appended as messages arrive, so its mtime moves with the turn.
    meta.last_activity_ms = stamp_millis(home, session_id);
    Ok(Some(meta))
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn stamp_millis(home: &Path, session_id: &str) -> Option<i64> {
    // Advisory activity time only. Unknown filesystem time stays absent, never
    // a fresh heartbeat or authority to recover/cancel a worker.
    let dir = home.join("sessions");
    let newest = [
        format!("{session_id}.journal.jsonl"),
        format!("{session_id}.json"),
    ]
    .into_iter()
    .filter_map(|name| {
        std::fs::metadata(dir.join(name))
            .ok()
            .and_then(|meta| meta.modified().ok())
    })
    .max()?;
    newest
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as i64)
}

#[cfg(test)]
#[path = "harness_api_tests/worker_meta.rs"]
mod tests;

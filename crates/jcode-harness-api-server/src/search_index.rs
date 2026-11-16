//! Cross-session full-text search over stored transcripts.
//!
//! A client offering global recall ("where did I discuss X?") cannot grep
//! session files per keystroke: the corpus here is ~100 MB across hundreds of
//! files, and every miss would cost a full rescan. So this keeps a SQLite FTS5
//! database at `$JCODE_HOME/state/search.db` and answers from that.
//!
//! What is indexed, and why only that:
//! - user prompts and assistant *final* text. Tool calls, tool results and
//!   system/reminder scaffolding are excluded: they dominate the byte count,
//!   they are the least interesting thing to recall, and indexing them would
//!   bury the sentence the user actually remembers under a hundred `bash`
//!   invocations.
//! - only text the *human* wrote. The harness itself appends user-role messages
//!   (auto-pokes, swarm await reports, background-task notices, reload
//!   continuations, session reminders). They are model instructions, not the
//!   user's words, and attributing them to the user makes search lie about who
//!   said what, so [`is_harness_injection`] drops them at index time.
//! - session titles, in their own FTS table so the UI can show a "Titles"
//!   section whose ranking never competes with transcript hits.
//!
//! Freshness: [`SearchIndex::sync`] walks the sessions directory and reindexes
//! only what changed, keyed on a file fingerprint (mtime + length of both the
//! snapshot and its append journal). A restart therefore costs one `stat` per
//! session, not one parse. [`SearchIndex::search`] syncs first, so a query is
//! always answered from the current transcript, and the bridge additionally
//! calls [`SearchIndex::note_session`] on append/title events to keep the
//! index warm without waiting for a query.
//!
//! Tokenization is `unicode61 remove_diacritics 2`: it splits on Unicode word
//! boundaries rather than ASCII, so Cyrillic prompts index and match, and it
//! folds diacritics so `cafe` finds `café` and `еще` finds `ещё`.

use jcode_harness_api::{
    SessionSearchHighlight, SessionSearchHit, SessionSearchResults, SessionSearchRole,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Bumped when a schema change makes existing rows wrong. The index is a cache
/// of the session files, so the fix for a mismatch is to drop and rebuild.
///
/// v2: messages carry the model that produced them, and harness-injected
/// user-role text is no longer indexed as a user prompt.
const SCHEMA_VERSION: i64 = 3;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
/// One pasted tool dump is not worth an unbounded row. FTS stores every token,
/// so a single pathological message can otherwise outweigh a whole session.
const MAX_MESSAGE_CHARS: usize = 32_768;
/// Tokens of context in a snippet: enough to read the sentence, short enough
/// for one list row.
const SNIPPET_TOKENS: usize = 24;
/// Assistant prose is more verbose than prompts, so identical matches favour
/// the prompt that asked for the work over the answer that delivered it.
const ASSISTANT_BOOST: f64 = 0.9;

const SNIPPET_START: char = '\u{2}';
const SNIPPET_END: char = '\u{3}';

/// Result of one incremental sync, used by the perf test and the bridge log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub scanned: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub pruned: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    snapshot_len: u64,
    snapshot_mtime_ms: i64,
    journal_len: u64,
    journal_mtime_ms: i64,
}

impl Fingerprint {
    fn encode(self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.snapshot_len, self.snapshot_mtime_ms, self.journal_len, self.journal_mtime_ms
        )
    }
}

/// One session's on-disk sources. A session always has a snapshot; it has a
/// journal only while (or after) work appended past the snapshot.
#[derive(Debug, Clone)]
struct SessionSource {
    id: String,
    snapshot: PathBuf,
    journal: Option<PathBuf>,
}

impl SessionSource {
    fn fingerprint(&self) -> Option<Fingerprint> {
        let (snapshot_len, snapshot_mtime_ms) = file_fingerprint(&self.snapshot)?;
        let (journal_len, journal_mtime_ms) = self
            .journal
            .as_deref()
            .and_then(file_fingerprint)
            .unwrap_or((0, 0));
        Some(Fingerprint {
            snapshot_len,
            snapshot_mtime_ms,
            journal_len,
            journal_mtime_ms,
        })
    }
}

fn file_fingerprint(path: &Path) -> Option<(u64, i64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_ms = modified
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    Some((metadata.len(), mtime_ms))
}

/// One indexed message ready to be written.
#[derive(Debug, Clone, PartialEq)]
struct MessageRow {
    message_id: String,
    role: SessionSearchRole,
    /// The model behind an assistant row; `None` for prompts and for sessions
    /// stored before the runtime recorded a model.
    model: Option<String>,
    ts_ms: i64,
    text: String,
}

/// Everything one session contributes to the index.
#[derive(Debug, Clone, Default, PartialEq)]
struct IndexedSession {
    title: String,
    activity_ms: i64,
    messages: Vec<MessageRow>,
}

/// Indexed corpus for cross-session search.
pub struct SearchIndex {
    connection: Connection,
    sessions_dir: PathBuf,
}

impl SearchIndex {
    /// Open (or create) the index for this jcode home.
    ///
    /// Failure is not fatal to the caller: a missing or unwritable database
    /// just means global search is unavailable, and the bridge reports an
    /// error for that one request instead of failing to serve the client.
    pub fn open_default() -> rusqlite::Result<Self> {
        let home =
            jcode_home().ok_or(rusqlite::Error::InvalidPath(PathBuf::from("$JCODE_HOME")))?;
        Self::open_at(&home.join("state/search.db"), &home.join("sessions"))
    }

    pub fn open_at(db_path: &Path, sessions_dir: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let connection = Connection::open(db_path)?;
        // WAL keeps a query from blocking the write that follows a turn, and
        // the bridge may serve several clients from one process.
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        let _ = connection.pragma_update(None, "synchronous", "NORMAL");
        let _ = connection.busy_timeout(std::time::Duration::from_secs(5));
        Self::prepare(&connection)?;
        Ok(Self {
            connection,
            sessions_dir: sessions_dir.to_path_buf(),
        })
    }

    fn prepare(connection: &Connection) -> rusqlite::Result<()> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            connection.execute_batch(
                "DROP TABLE IF EXISTS search_messages;
                 DROP TABLE IF EXISTS search_titles;
                 DROP TABLE IF EXISTS search_sessions;",
            )?;
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS search_sessions (
                 session_id TEXT PRIMARY KEY,
                 title TEXT NOT NULL DEFAULT '',
                 activity_ms INTEGER NOT NULL DEFAULT 0,
                 fingerprint TEXT NOT NULL DEFAULT '',
                 indexed_messages INTEGER NOT NULL DEFAULT 0
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS search_messages USING fts5(
                 session_id UNINDEXED,
                 message_id UNINDEXED,
                 role UNINDEXED,
                 ts_ms UNINDEXED,
                 model UNINDEXED,
                 text,
                 tokenize = 'unicode61 remove_diacritics 2'
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS search_titles USING fts5(
                 session_id UNINDEXED,
                 title,
                 tokenize = 'unicode61 remove_diacritics 2'
             );",
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)
    }

    /// Sessions the index currently knows about.
    pub fn indexed_sessions(&self) -> u64 {
        self.connection
            .query_row("SELECT COUNT(*) FROM search_sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or_default()
            .max(0) as u64
    }

    /// Reindex whatever changed on disk. Cheap when nothing did.
    pub fn sync(&mut self) -> SyncStats {
        let mut stats = SyncStats::default();
        let sources = match self.session_sources() {
            Ok(sources) => sources,
            Err(_) => return stats,
        };
        stats.scanned = sources.len();
        let known = self.known_fingerprints();
        let mut pending: Vec<(SessionSource, Fingerprint)> = Vec::new();
        for source in sources {
            let Some(fingerprint) = source.fingerprint() else {
                continue;
            };
            if known
                .get(&source.id)
                .is_some_and(|known| *known == fingerprint.encode())
            {
                stats.unchanged += 1;
                continue;
            }
            pending.push((source, fingerprint));
        }
        if let Some(pruned) = self.prune_missing(&known) {
            stats.pruned = pruned;
        }
        if pending.is_empty() {
            return stats;
        }
        let indexed = index_sources(pending);
        let mut written = 0usize;
        for (source_id, fingerprint, payload) in indexed {
            if self
                .write_session(&source_id, &fingerprint, &payload)
                .is_ok()
            {
                written += 1;
            }
        }
        stats.indexed = written;
        stats
    }

    /// Reindex one session if its files changed. Called on live events so a
    /// freshly finished turn is searchable before anything asks for it.
    pub fn note_session(&mut self, session_id: &str) {
        let Some(source) = self.session_source(session_id) else {
            return;
        };
        let Some(fingerprint) = source.fingerprint() else {
            return;
        };
        let known = self.known_fingerprint(session_id).unwrap_or_default();
        if known == fingerprint.encode() {
            return;
        }
        if let Some((_, _, payload)) = index_sources(vec![(source, fingerprint)]).pop() {
            let _ = self.write_session(session_id, &fingerprint, &payload);
        }
    }

    /// Record a title the runtime just set, without re-reading the snapshot.
    pub fn note_title(&mut self, session_id: &str, title: &str) {
        let _ = self.connection.execute(
            "INSERT INTO search_sessions (session_id, title, activity_ms, fingerprint, indexed_messages)
             VALUES (?1, ?2, ?3, '', 0)
             ON CONFLICT(session_id) DO UPDATE SET title = excluded.title",
            params![session_id, title, now_ms()],
        );
        let _ = self.connection.execute(
            "DELETE FROM search_titles WHERE session_id = ?1",
            params![session_id],
        );
        if !title.trim().is_empty() {
            let _ = self.connection.execute(
                "INSERT INTO search_titles (session_id, title) VALUES (?1, ?2)",
                params![session_id, title],
            );
        }
    }

    /// Answer one query against the current corpus.
    pub fn search(
        &mut self,
        query: &str,
        limit: Option<u32>,
    ) -> rusqlite::Result<SessionSearchResults> {
        let limit = limit
            .map(|limit| limit as usize)
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_LIMIT)
            .min(MAX_LIMIT);
        let mut results = SessionSearchResults {
            query: query.to_string(),
            ..Default::default()
        };
        results.indexed_sessions = self.indexed_sessions();
        let Some(match_expression) = match_expression(query) else {
            return Ok(results);
        };
        results.titles = self.search_titles(&match_expression, limit)?;
        results.messages = self.search_messages(&match_expression, limit)?;
        results.truncated = results.titles.len() >= limit || results.messages.len() >= limit;
        Ok(results)
    }

    fn search_titles(
        &self,
        match_expression: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionSearchHit>> {
        let mut statement = self.connection.prepare(
            "SELECT t.session_id,
                    s.title,
                    s.activity_ms,
                    snippet(search_titles, 1, ?2, ?3, '…', ?4),
                    bm25(search_titles, 0.0, 1.0)
             FROM search_titles t
             JOIN search_sessions s ON s.session_id = t.session_id
             WHERE search_titles MATCH ?1
             ORDER BY bm25(search_titles, 0.0, 1.0), s.activity_ms DESC
             LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                match_expression,
                SNIPPET_START.to_string(),
                SNIPPET_END.to_string(),
                SNIPPET_TOKENS as i64,
                limit as i64
            ],
            |row| {
                let raw: String = row.get(3)?;
                let (snippet, highlights) = split_highlights(&raw);
                Ok(SessionSearchHit {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    role: SessionSearchRole::Title,
                    model: None,
                    snippet,
                    highlights,
                    timestamp_ms: row.get(2)?,
                    message_id: None,
                })
            },
        )?;
        rows.collect()
    }

    fn search_messages(
        &self,
        match_expression: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionSearchHit>> {
        // bm25 returns a negative score (more negative is better) and the role
        // column is UNINDEXED, so the prompt/answer weighting is applied here
        // rather than in the match expression.
        let mut statement = self.connection.prepare(
            "SELECT m.session_id,
                    m.message_id,
                    m.role,
                    m.ts_ms,
                    m.model,
                    s.title,
                    snippet(search_messages, 5, ?2, ?3, '…', ?4),
                    bm25(search_messages, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0)
             FROM search_messages m
             LEFT JOIN search_sessions s ON s.session_id = m.session_id
             WHERE search_messages MATCH ?1
             ORDER BY bm25(search_messages, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0)
             LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                match_expression,
                SNIPPET_START.to_string(),
                SNIPPET_END.to_string(),
                SNIPPET_TOKENS as i64,
                // Over-fetch: the role boost reorders rows across a wider band
                // than the SQL limit, so trim in Rust after scoring.
                (limit * 4) as i64
            ],
            |row| {
                let role: String = row.get(2)?;
                let raw: String = row.get(6)?;
                let rank: f64 = row.get(7)?;
                let (snippet, highlights) = split_highlights(&raw);
                let role = match role.as_str() {
                    "user" => SessionSearchRole::User,
                    _ => SessionSearchRole::Assistant,
                };
                Ok((
                    SessionSearchHit {
                        session_id: row.get(0)?,
                        title: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        role,
                        model: row.get(4)?,
                        snippet,
                        highlights,
                        timestamp_ms: row.get(3)?,
                        message_id: Some(row.get(1)?),
                    },
                    rank,
                ))
            },
        )?;
        let mut scored: Vec<(SessionSearchHit, f64)> = rows.filter_map(Result::ok).collect();
        scored.sort_by(|(left, left_rank), (right, right_rank)| {
            let left_score = boost(left.role) * -left_rank;
            let right_score = boost(right.role) * -right_rank;
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Recent first is the tiebreak: equally relevant hits are more
                // likely to be the conversation the user remembers.
                .then(right.timestamp_ms.cmp(&left.timestamp_ms))
        });
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(hit, _)| hit).collect())
    }

    fn write_session(
        &mut self,
        session_id: &str,
        fingerprint: &Fingerprint,
        payload: &IndexedSession,
    ) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM search_messages WHERE session_id = ?1",
            params![session_id],
        )?;
        transaction.execute(
            "DELETE FROM search_titles WHERE session_id = ?1",
            params![session_id],
        )?;
        transaction.execute(
            "INSERT INTO search_sessions (session_id, title, activity_ms, fingerprint, indexed_messages)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                 title = excluded.title,
                 activity_ms = excluded.activity_ms,
                 fingerprint = excluded.fingerprint,
                 indexed_messages = excluded.indexed_messages",
            params![
                session_id,
                payload.title,
                payload.activity_ms,
                fingerprint.encode(),
                payload.messages.len() as i64
            ],
        )?;
        if !payload.title.trim().is_empty() {
            transaction.execute(
                "INSERT INTO search_titles (session_id, title) VALUES (?1, ?2)",
                params![session_id, payload.title],
            )?;
        }
        {
            let mut insert = transaction.prepare(
                "INSERT INTO search_messages (session_id, message_id, role, ts_ms, model, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for message in &payload.messages {
                insert.execute(params![
                    session_id,
                    message.message_id,
                    message.role.as_str(),
                    message.ts_ms,
                    message.model,
                    message.text
                ])?;
            }
        }
        transaction.commit()
    }

    fn session_sources(&self) -> std::io::Result<Vec<SessionSource>> {
        let mut ids: BTreeMap<String, SessionSource> = BTreeMap::new();
        for entry in std::fs::read_dir(&self.sessions_dir)?.flatten() {
            let path = entry.path();
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(id) = name.strip_suffix(".journal.jsonl").map(str::to_string) {
                ids.entry(id.clone())
                    .or_insert_with(|| SessionSource {
                        snapshot: self.sessions_dir.join(format!("{id}.json")),
                        id,
                        journal: None,
                    })
                    .journal = Some(path);
            } else if let Some(id) = name.strip_suffix(".json").map(str::to_string) {
                // `*.bak` and anything else that is not a session snapshot is
                // already excluded by the suffix, but a stray directory entry
                // such as `edit-stats` is filtered above.
                let snapshot = path.clone();
                let source = ids.entry(id.clone()).or_insert_with(|| SessionSource {
                    id,
                    snapshot: snapshot.clone(),
                    journal: None,
                });
                source.snapshot = path;
            }
        }
        // A journal without its snapshot cannot be read as a transcript.
        ids.retain(|_, source| source.snapshot.is_file());
        Ok(ids.into_values().collect())
    }

    fn session_source(&self, session_id: &str) -> Option<SessionSource> {
        let snapshot = self.sessions_dir.join(format!("{session_id}.json"));
        if !snapshot.is_file() {
            return None;
        }
        let journal = self
            .sessions_dir
            .join(format!("{session_id}.journal.jsonl"));
        Some(SessionSource {
            id: session_id.to_string(),
            snapshot,
            journal: journal.is_file().then_some(journal),
        })
    }

    fn known_fingerprints(&self) -> BTreeMap<String, String> {
        let Ok(mut statement) = self
            .connection
            .prepare("SELECT session_id, fingerprint FROM search_sessions")
        else {
            return BTreeMap::new();
        };
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<_>>())
            .unwrap_or_default()
    }

    fn known_fingerprint(&self, session_id: &str) -> Option<String> {
        self.connection
            .query_row(
                "SELECT fingerprint FROM search_sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .ok()
    }

    /// Drop sessions whose files are gone, so a deleted session cannot keep
    /// answering queries forever.
    fn prune_missing(&mut self, known: &BTreeMap<String, String>) -> Option<usize> {
        let present: Vec<String> = self
            .session_sources()
            .ok()?
            .into_iter()
            .map(|s| s.id)
            .collect();
        let mut pruned = 0;
        for session_id in known.keys() {
            if present.iter().any(|present| present == session_id) {
                continue;
            }
            let _ = self.connection.execute(
                "DELETE FROM search_messages WHERE session_id = ?1",
                params![session_id],
            );
            let _ = self.connection.execute(
                "DELETE FROM search_titles WHERE session_id = ?1",
                params![session_id],
            );
            let _ = self.connection.execute(
                "DELETE FROM search_sessions WHERE session_id = ?1",
                params![session_id],
            );
            pruned += 1;
        }
        Some(pruned)
    }
}

fn boost(role: SessionSearchRole) -> f64 {
    match role {
        SessionSearchRole::Assistant => ASSISTANT_BOOST,
        _ => 1.0,
    }
}

/// Parse the query into an FTS5 MATCH expression.
///
/// The user types prose, not FTS5 syntax: a stray `-`, `"` or the word `OR`
/// must not turn into an operator or a syntax error. So the query is split
/// into alphanumeric runs, each quoted (quoting also neutralises bare
/// keywords like `AND`) and made a prefix match, then ANDed.
fn match_expression(query: &str) -> Option<String> {
    let folded = fold_cyrillic_yo(query);
    let tokens: Vec<String> = folded
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"*", token.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(" AND "))
}

/// Fold `ё` to `е`.
///
/// SQLite's `remove_diacritics` does not decompose Cyrillic `ё`, and Russian
/// typists write either letter for the same word. Folding both the indexed text
/// and the query means `еще` and `ещё` are the same search.
fn fold_cyrillic_yo(text: &str) -> String {
    if !text.contains(['ё', 'Ё']) {
        return text.to_string();
    }
    text.chars()
        .map(|character| match character {
            'ё' => 'е',
            'Ё' => 'Е',
            other => other,
        })
        .collect()
}

/// Turn an FTS5 snippet with marker characters into clean text plus char
/// ranges, so the client can highlight without knowing about the markers.
fn split_highlights(raw: &str) -> (String, Vec<SessionSearchHighlight>) {
    let mut text = String::with_capacity(raw.len());
    let mut highlights = Vec::new();
    let mut chars = 0u32;
    let mut open: Option<u32> = None;
    for character in raw.chars() {
        match character {
            SNIPPET_START => open = Some(chars),
            SNIPPET_END => {
                if let Some(start) = open.take()
                    && chars > start
                {
                    highlights.push(SessionSearchHighlight { start, end: chars });
                }
            }
            _ => {
                text.push(character);
                chars += 1;
            }
        }
    }
    // A snippet cut mid-highlight (truncated by the token budget) still marks
    // what it showed.
    if let Some(start) = open
        && chars > start
    {
        highlights.push(SessionSearchHighlight { start, end: chars });
    }
    (text, highlights)
}

/// Parse pending sessions in parallel, then return them sorted by id so writes
/// (and tests) are deterministic.
fn index_sources(
    pending: Vec<(SessionSource, Fingerprint)>,
) -> Vec<(String, Fingerprint, IndexedSession)> {
    let threads = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let chunk = pending.len().div_ceil(threads).max(1);
    let mut parsed: Vec<(String, Fingerprint, IndexedSession)> = std::thread::scope(|scope| {
        let workers: Vec<_> = pending
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(source, fingerprint)| {
                            let payload = read_session(source).ok()?;
                            Some((source.id.clone(), *fingerprint, payload))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .filter_map(|worker| worker.join().ok())
            .flatten()
            .collect()
    });
    parsed.sort_by(|left, right| left.0.cmp(&right.0));
    parsed
}

#[derive(Debug, Deserialize)]
struct SessionSnapshot {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    /// The model the session is (or was last) running, recorded next to its
    /// messages; a hit can then say which model wrote the answer.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    messages: Vec<StoredMessage>,
}

#[derive(Debug, Deserialize)]
struct StoredMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    role: String,
    /// Present on system reminders and other bookkeeping rows that are shown
    /// differently from what the user typed.
    #[serde(default)]
    display_role: Option<String>,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: Option<bool>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct JournalLine {
    #[serde(default)]
    meta: Option<JournalMeta>,
    #[serde(default)]
    append_messages: Vec<StoredMessage>,
}

#[derive(Debug, Deserialize)]
struct JournalMeta {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    /// The model in force when these appends were written. A session can move
    /// between models mid-conversation, so this is per journal line, not
    /// per session.
    #[serde(default)]
    model: Option<String>,
}

/// Read one session from its snapshot plus any journal appends the snapshot
/// does not contain yet.
fn read_session(source: &SessionSource) -> anyhow::Result<IndexedSession> {
    let snapshot: SessionSnapshot = serde_json::from_slice(&std::fs::read(&source.snapshot)?)?;
    let mut indexed = IndexedSession {
        title: snapshot
            .custom_title
            .clone()
            .or_else(|| snapshot.title.clone())
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut latest_ms = 0;
    let mut model = snapshot.model.clone();
    let mut ask_ids = std::collections::HashSet::new();
    for message in &snapshot.messages {
        push_message(
            &mut indexed.messages,
            &mut seen,
            &mut latest_ms,
            message,
            model.as_deref(),
            &mut ask_ids,
        );
    }
    if let Some(updated_at) = snapshot.updated_at.as_deref() {
        latest_ms = latest_ms.max(parse_timestamp_ms(updated_at).unwrap_or_default());
    }
    if let Some(journal) = source.journal.as_deref()
        && let Ok(bytes) = std::fs::read(journal)
    {
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(line) = serde_json::from_slice::<JournalLine>(line) else {
                continue;
            };
            if let Some(meta) = line.meta {
                if meta.model.is_some() {
                    model = meta.model;
                }
                if let Some(title) = meta.custom_title.or(meta.title)
                    && !title.trim().is_empty()
                {
                    indexed.title = title;
                }
                if let Some(updated_at) = meta.updated_at.as_deref() {
                    latest_ms = latest_ms.max(parse_timestamp_ms(updated_at).unwrap_or_default());
                }
            }
            for message in &line.append_messages {
                push_message(
                    &mut indexed.messages,
                    &mut seen,
                    &mut latest_ms,
                    message,
                    model.as_deref(),
                    &mut ask_ids,
                );
            }
        }
    }
    indexed.activity_ms = latest_ms;
    for message in &mut indexed.messages {
        if message.ts_ms == 0 {
            message.ts_ms = latest_ms;
        }
    }
    Ok(indexed)
}

fn push_message(
    rows: &mut Vec<MessageRow>,
    seen: &mut std::collections::HashSet<String>,
    latest_ms: &mut i64,
    message: &StoredMessage,
    model: Option<&str>,
    ask_ids: &mut std::collections::HashSet<String>,
) {
    for block in &message.content {
        if let ContentBlock::ToolUse { id, name } = block
            && name == "ask"
        {
            ask_ids.insert(id.clone());
        }
    }
    // System prompts, reminders and other display-only rows are scaffolding,
    // not something worth recalling.
    if message.display_role.is_some() {
        return;
    }
    let role = match message.role.as_str() {
        "user" => SessionSearchRole::User,
        "assistant" => SessionSearchRole::Assistant,
        _ => return,
    };
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if role == SessionSearchRole::User
                && ask_ids.contains(tool_use_id)
                && *is_error != Some(true) =>
            {
                jcode_session_types::stored_ask_answer_text(content)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    // A user-role message is not automatically the user's: the harness appends
    // its own instructions through the same channel.
    if role == SessionSearchRole::User && is_harness_injection(text) {
        return;
    }
    let ts_ms = message
        .timestamp
        .as_deref()
        .and_then(parse_timestamp_ms)
        .unwrap_or_default();
    *latest_ms = (*latest_ms).max(ts_ms);
    let message_id = message.id.clone().unwrap_or_default();
    if !message_id.is_empty() && !seen.insert(message_id.clone()) {
        return;
    }
    rows.push(MessageRow {
        message_id,
        role,
        model: (role == SessionSearchRole::Assistant)
            .then(|| model.map(str::to_string))
            .flatten()
            .filter(|model| !model.trim().is_empty()),
        ts_ms,
        text: truncate_chars(&fold_cyrillic_yo(text), MAX_MESSAGE_CHARS),
    });
}

/// True when a user-role message was written by the harness, not typed by the
/// human.
///
/// Every prefix here is a fixed header the runtime itself emits: session
/// reminders, `[auto]` continuations (reload, interruption, todo gates),
/// auto-poke prompts, swarm await/notification reports, and background-task
/// notices. They are addressed to the model, so indexing them under "You"
/// would make the search screen claim the user wrote the harness's own
/// instructions.
fn is_harness_injection(text: &str) -> bool {
    let trimmed = text.trim_start();
    const PREFIXES: [&str; 7] = [
        "<system-reminder>",
        "[auto]",
        "🐝 **Swarm",
        "🐝 Swarm",
        "**Background task",
        "Member statuses:",
        "Await satisfied.",
    ];
    if PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return true;
    }
    jcode_todo_policy::is_auto_poke_message(trimmed)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

/// RFC 3339 to Unix epoch milliseconds, tolerating fractional seconds and a
/// numeric offset. Returns `None` for anything unparseable; callers fall back
/// to the session's activity time rather than dropping the message.
fn parse_timestamp_ms(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let field =
        |range: std::ops::Range<usize>| -> Option<i64> { raw.get(range)?.parse::<i64>().ok() };
    let year = field(0..4)?;
    let month = field(5..7)?;
    let day = field(8..10)?;
    let hour = field(11..13)?;
    let minute = field(14..16)?;
    let second = field(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = raw.get(19..)?;
    let mut millis = 0i64;
    if rest.starts_with('.') {
        let digits: &str = &rest[1..];
        let length = digits
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count()
            .min(digits.len());
        let fraction = &digits[..length];
        if fraction.is_empty() {
            return None;
        }
        let mut value: i64 = fraction.get(..length.min(3))?.parse().ok()?;
        for _ in length.min(3)..3 {
            value *= 10;
        }
        millis = value;
        rest = &digits[length..];
    }
    let offset_seconds = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = match rest.get(3..) {
                Some("") => 0,
                Some(rest) => rest.strip_prefix(':')?.parse().ok()?,
                None => 0,
            };
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    Some(seconds * 1_000 + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn jcode_home() -> Option<PathBuf> {
    std::env::var_os("JCODE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".jcode")))
}

static INDEX: OnceLock<Mutex<Option<SearchIndex>>> = OnceLock::new();

fn global_index() -> MutexGuard<'static, Option<SearchIndex>> {
    let mutex = INDEX.get_or_init(|| Mutex::new(SearchIndex::open_default().ok()));
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `query` against the process-wide index, syncing changed sessions first.
/// An unopenable index yields empty results rather than an error, so a client
/// sees "no matches" instead of a failed search screen.
pub fn search(query: &str, limit: Option<u32>) -> SessionSearchResults {
    let mut guard = global_index();
    let Some(index) = guard.as_mut() else {
        return SessionSearchResults {
            query: query.to_string(),
            ..Default::default()
        };
    };
    let started = Instant::now();
    let stats = index.sync();
    let results = index
        .search(query, limit)
        .unwrap_or_else(|_| SessionSearchResults {
            query: query.to_string(),
            ..Default::default()
        });
    if stats.indexed > 0 {
        eprintln!(
            "harness API bridge: search index synced {} session(s) ({} unchanged) in {:.1}ms",
            stats.indexed,
            stats.unchanged,
            started.elapsed().as_secs_f64() * 1_000.0
        );
    }
    results
}

/// Live update: one session's files changed (a message was appended).
pub fn note_session(session_id: &str) {
    let mut guard = global_index();
    if let Some(index) = guard.as_mut() {
        index.note_session(session_id);
    }
}

/// Live update: a session's title changed.
pub fn note_title(session_id: &str, title: &str) {
    let mut guard = global_index();
    if let Some(index) = guard.as_mut() {
        index.note_title(session_id, title);
    }
}

#[cfg(test)]
#[path = "search_index_tests.rs"]
mod tests;

/// Point the process-wide index at a test corpus.
///
/// The bridge's request path goes through [`search`], so an API round-trip test
/// has to install its own index rather than index whatever the developer has
/// in `~/.jcode`. Callers must hold the translate test lock, since the slot is
/// process-global.
#[cfg(test)]
pub fn install_global_for_tests(index: SearchIndex) {
    let mutex = INDEX.get_or_init(|| Mutex::new(None));
    *mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(index);
}

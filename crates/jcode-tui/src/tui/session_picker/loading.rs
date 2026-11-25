use crate::id::{extract_session_name, session_icon};
#[path = "loading/opencode.rs"]
mod opencode;
use opencode::load_opencode_session_info;

use crate::message::Role;
use crate::registry::{self, ServerInfo};
use crate::session::{self, CrashedSessionsInfo, Session, SessionStatus, StoredDisplayRole};
use crate::storage;
use anyhow::Result;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{
    DEFAULT_SESSION_SCAN_LIMIT, MAX_SESSION_SCAN_LIMIT, MIN_SESSION_SCAN_LIMIT, PreviewMessage,
    ResumeTarget, SEARCH_CONTENT_BUDGET_BYTES, ServerGroup, SessionInfo, SessionSource,
};

mod mobile_inventory;

#[cfg(test)]
const TRANSCRIPT_SEARCH_CHUNK_BYTES: usize = 64 * 1024;

/// Maximum number of trailing transcript messages loaded into the `/resume`
/// preview. Large enough that the preview scrolls back through a long session
/// like the live chat does, while bounding the one-off render on selection.
pub(super) const PREVIEW_MESSAGE_LIMIT: usize = 400;

fn session_scan_limit() -> usize {
    std::env::var("JCODE_SESSION_PICKER_MAX_SESSIONS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .map(|n| n.clamp(MIN_SESSION_SCAN_LIMIT, MAX_SESSION_SCAN_LIMIT))
        .unwrap_or(DEFAULT_SESSION_SCAN_LIMIT)
}

fn session_candidate_window(scan_limit: usize) -> usize {
    scan_limit
        .saturating_mul(20)
        .clamp(scan_limit.max(1), 20_000)
}

/// Whether the picker lists transcripts discovered from other agent CLIs.
///
/// On by default (they can be resumed or imported), but it clutters the picker
/// for users who only ever want jcode's own sessions, so `[display]
/// external_sessions = false` (or `JCODE_EXTERNAL_SESSIONS=0`) opts out
/// (issue #674). Checked before spawning the scan threads so opting out also
/// skips the filesystem work.
fn include_external_sessions() -> bool {
    crate::config::config().display.external_sessions
}

fn include_old_saved_sessions_on_initial_load() -> bool {
    std::env::var("JCODE_SESSION_PICKER_INCLUDE_OLD_SAVED")
        .ok()
        .is_some_and(|raw| matches!(raw.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

const SESSION_LIST_CACHE_TTL: Duration = Duration::from_secs(5);
const SESSION_LIST_DISK_CACHE_VERSION: u32 = 5;
const SESSION_LIST_DISK_CACHE_MAX_AGE_SECONDS: i64 = 7 * 24 * 60 * 60;
const SAVED_METADATA_TAIL_SCAN_BYTES: u64 = 64 * 1024;
const INITIAL_TRANSCRIPT_SEARCH_BUDGET_BYTES: usize = 64 * 1024;
const MESSAGE_SEARCH_EXCERPT_BYTES: usize = 8 * 1024;

/// Upper bound on worker threads used to parse/stat session files in parallel.
/// The session picker load is dominated by per-file IO + JSON parsing across
/// hundreds of snapshots; fanning that work out across cores turns the cold
/// `/resume` load from a serial slog into a roughly core-count-bounded scan.
const SESSION_LOAD_MAX_THREADS: usize = 8;

/// Number of worker threads to use for a parallel pass over `item_count` items.
/// Returns 1 for tiny batches so we never pay thread-spawn overhead when there
/// is barely any work to do.
fn session_load_thread_count(item_count: usize) -> usize {
    if item_count <= 1 {
        return 1;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores.clamp(1, SESSION_LOAD_MAX_THREADS).min(item_count)
}

/// Map `f` over `items` across a bounded scoped thread pool, preserving input
/// order in the returned vector. Falls back to a plain serial map when only one
/// worker is warranted. `f` must be `Sync` because every worker shares it.
fn parallel_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let thread_count = session_load_thread_count(items.len());
    if thread_count <= 1 {
        return items.into_iter().map(f).collect();
    }

    // Partition the work into `thread_count` owned chunks so each worker can
    // take its inputs by value (no clone, no shared mutation). We remember the
    // starting offset of each chunk to stitch results back into input order.
    let chunk_size = items.len().div_ceil(thread_count);
    let mut chunks: Vec<(usize, Vec<T>)> = Vec::with_capacity(thread_count);
    let mut offset = 0usize;
    let mut remaining = items;
    while !remaining.is_empty() {
        let take = chunk_size.min(remaining.len());
        let rest = remaining.split_off(take);
        chunks.push((offset, remaining));
        offset += take;
        remaining = rest;
    }

    let f = &f;
    let mut results: Vec<(usize, Vec<R>)> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(chunks.len());
        for (start, chunk) in chunks {
            handles
                .push(scope.spawn(move || (start, chunk.into_iter().map(f).collect::<Vec<R>>())));
        }
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    results.sort_by_key(|(start, _)| *start);
    let total: usize = results.iter().map(|(_, chunk)| chunk.len()).sum();
    let mut out = Vec::with_capacity(total);
    for (_, chunk) in results {
        out.extend(chunk);
    }
    out
}

#[derive(Clone)]
struct SessionListCacheEntry {
    loaded_at: Instant,
    sessions_dir: PathBuf,
    scan_limit: usize,
    /// Part of the cache key: toggling `display.external_sessions` must not
    /// serve a stale list that still contains (or still omits) other CLIs'
    /// transcripts.
    external_sessions: bool,
    sessions: Vec<SessionInfo>,
}

#[derive(Default)]
struct SessionListCacheState {
    generation: u64,
    entry: Option<SessionListCacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct GroupedSessionListDiskCache<Groups = Vec<ServerGroup>, Orphans = Vec<SessionInfo>> {
    version: u32,
    generated_at: chrono::DateTime<chrono::Utc>,
    sessions_dir: PathBuf,
    scan_limit: usize,
    include_old_saved_sessions: bool,
    /// Part of the cache key for the same reason as
    /// `SessionListCacheEntry::external_sessions`. Defaulted so an older cache
    /// file stays readable (it was written with externals on).
    #[serde(default = "crate::tui::session_picker::loading::default_true")]
    external_sessions: bool,
    server_groups: Groups,
    orphan_sessions: Orphans,
}

pub(crate) fn default_true() -> bool {
    true
}

fn session_list_cache() -> &'static Mutex<SessionListCacheState> {
    static CACHE: OnceLock<Mutex<SessionListCacheState>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SessionListCacheState::default()))
}

pub fn invalidate_session_list_cache() {
    if let Ok(mut state) = session_list_cache().lock() {
        state.generation = state.generation.wrapping_add(1);
        state.entry = None;
    }
}

#[cfg(test)]
type PublishHook = Box<dyn Fn() + Send>;

#[cfg(test)]
fn before_session_list_cache_publish_hook() -> &'static Mutex<Option<PublishHook>> {
    static HOOK: OnceLock<Mutex<Option<PublishHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_before_session_list_cache_publish_hook() {
    if let Ok(hook) = before_session_list_cache_publish_hook().lock()
        && let Some(hook) = hook.as_ref()
    {
        hook();
    }
}

#[cfg(test)]
pub(crate) fn set_before_session_list_cache_publish_hook(hook: Option<Box<dyn Fn() + Send>>) {
    if let Ok(mut slot) = before_session_list_cache_publish_hook().lock() {
        *slot = hook;
    }
}

fn session_list_disk_cache_path() -> Result<PathBuf> {
    Ok(storage::jcode_dir()?.join("cache/session-picker-list-v2.json"))
}

fn session_list_disk_cache_is_usable(
    cache: &GroupedSessionListDiskCache,
    sessions_dir: &Path,
    scan_limit: usize,
    want_external: bool,
) -> bool {
    cache.version == SESSION_LIST_DISK_CACHE_VERSION
        && cache.sessions_dir == sessions_dir
        && cache.scan_limit == scan_limit
        && cache.include_old_saved_sessions == include_old_saved_sessions_on_initial_load()
        && cache.external_sessions == want_external
        && chrono::Utc::now()
            .signed_duration_since(cache.generated_at)
            .num_seconds()
            <= SESSION_LIST_DISK_CACHE_MAX_AGE_SECONDS
}

/// Borrow the inventories while writing. Cloning them here duplicates every
/// search index and loaded transcript solely for the duration of serialization.
fn grouped_session_list_disk_cache<'a>(
    sessions_dir: &Path,
    scan_limit: usize,
    server_groups: &'a [ServerGroup],
    orphan_sessions: &'a [SessionInfo],
) -> GroupedSessionListDiskCache<&'a [ServerGroup], &'a [SessionInfo]> {
    GroupedSessionListDiskCache {
        version: SESSION_LIST_DISK_CACHE_VERSION,
        generated_at: chrono::Utc::now(),
        sessions_dir: sessions_dir.to_path_buf(),
        scan_limit,
        include_old_saved_sessions: include_old_saved_sessions_on_initial_load(),
        external_sessions: include_external_sessions(),
        server_groups,
        orphan_sessions,
    }
}

fn write_grouped_session_list_disk_cache(
    sessions_dir: &Path,
    scan_limit: usize,
    server_groups: &[ServerGroup],
    orphan_sessions: &[SessionInfo],
) {
    let Ok(path) = session_list_disk_cache_path() else {
        return;
    };
    let cache =
        grouped_session_list_disk_cache(sessions_dir, scan_limit, server_groups, orphan_sessions);
    if let Err(err) = storage::write_json_fast(&path, &cache) {
        crate::logging::debug(&format!(
            "failed to write session picker disk cache {}: {}",
            path.display(),
            err
        ));
    }
}

pub fn load_cached_sessions_grouped() -> Option<(Vec<ServerGroup>, Vec<SessionInfo>)> {
    let sessions_dir = storage::jcode_dir().ok()?.join("sessions");
    let scan_limit = session_scan_limit();
    let path = session_list_disk_cache_path().ok()?;
    let cache: GroupedSessionListDiskCache = storage::read_json(&path).ok()?;
    if !session_list_disk_cache_is_usable(
        &cache,
        &sessions_dir,
        scan_limit,
        include_external_sessions(),
    ) {
        return None;
    }
    Some((cache.server_groups, cache.orphan_sessions))
}

/// Recover the exact recently closed sessions without enumerating historical
/// transcripts or waiting for the large search cache to deserialize.
fn load_recently_closed_sessions() -> Vec<SessionInfo> {
    let window = Duration::from_secs(crate::config::config().display.resume_recent_close_seconds);
    let closures = session::recent_window_closures(window);
    if closures.is_empty() {
        return Vec::new();
    }
    let Ok(home) = storage::jcode_dir() else {
        return Vec::new();
    };
    let sessions_dir = home.join("sessions");
    let catchup = crate::catchup::CatchupSeenSnapshot::load();
    let mut ids: Vec<_> = closures.into_iter().collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    parallel_map(ids, |(id, _)| {
        parse_jcode_session_info(&sessions_dir, &id, &catchup)
    })
    .into_iter()
    .flatten()
    .collect()
}

/// Shared staged loader for the standalone and in-session pickers. A just-closed
/// session is actionable before loading either the cache or the historical list.
pub(crate) fn send_session_list_updates(sender: std::sync::mpsc::Sender<super::SessionListUpdate>) {
    send_session_list_updates_with_discovery(
        sender,
        mobile_inventory::available(),
        mobile_inventory::refresh,
    );
}

fn send_session_list_updates_with_discovery(
    sender: std::sync::mpsc::Sender<super::SessionListUpdate>,
    discovery_available: bool,
    refresh: impl FnOnce() -> Result<bool>,
) {
    let recent = load_recently_closed_sessions();
    if !recent.is_empty()
        && sender
            .send(super::SessionListUpdate::Cached((
                Vec::new(),
                recent.clone(),
            )))
            .is_err()
    {
        return;
    }
    if let Some((mut groups, mut orphans)) = load_cached_sessions_grouped() {
        // Do not let an older cached snapshot remove or roll back the recovered
        // session while the full refresh is still running.
        let ids: HashSet<_> = recent.iter().map(|s| s.id.as_str()).collect();
        for group in &mut groups {
            group.sessions.retain(|s| !ids.contains(s.id.as_str()));
        }
        orphans.retain(|s| !ids.contains(s.id.as_str()));
        orphans.extend(recent.iter().cloned());
        if sender
            .send(super::SessionListUpdate::Cached((groups, orphans)))
            .is_err()
        {
            return;
        }
    }
    let result = load_sessions_grouped().map(|(groups, mut orphans)| {
        // A long-idle transcript can fall outside the bounded historical scan.
        // Keep it recoverable after the full refresh too, without replacing a
        // newer snapshot when that scan did include it.
        let loaded: HashSet<_> = orphans
            .iter()
            .map(|s| s.id.clone())
            .chain(
                groups
                    .iter()
                    .flat_map(|g| g.sessions.iter().map(|s| s.id.clone())),
            )
            .collect();
        orphans.extend(recent.iter().filter(|s| !loaded.contains(&s.id)).cloned());
        (groups, orphans)
    });
    if !discovery_available {
        let _ = sender.send(super::SessionListUpdate::Fresh(result));
        return;
    }
    // Keep local results immediately usable while account/SSH discovery runs.
    let local = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = sender.send(super::SessionListUpdate::Fresh(Err(error)));
            return;
        }
    };
    if sender
        .send(super::SessionListUpdate::Cached(local.clone()))
        .is_err()
    {
        return;
    }
    let result = match refresh() {
        Ok(true) => {
            invalidate_session_list_cache();
            load_sessions_grouped().map(|(groups, mut orphans)| {
                let loaded: HashSet<_> = orphans
                    .iter()
                    .map(|s| s.id.as_str())
                    .chain(
                        groups
                            .iter()
                            .flat_map(|g| g.sessions.iter().map(|s| s.id.as_str())),
                    )
                    .collect();
                let recovered: Vec<_> = recent
                    .into_iter()
                    .filter(|s| !loaded.contains(s.id.as_str()))
                    .collect();
                orphans.extend(recovered);
                (groups, orphans)
            })
        }
        Ok(false) => Ok(local),
        Err(error) => {
            crate::logging::debug(&format!("Mobile resume discovery unavailable: {error:#}"));
            let _ = sender.send(super::SessionListUpdate::CloudUnavailable(
                "Cloud sessions could not be refreshed. Reopen /resume to retry.".into(),
            ));
            Ok(local)
        }
    };
    let _ = sender.send(super::SessionListUpdate::Fresh(result));
}

#[path = "loading/search_text.rs"]
mod search_text;
#[cfg(test)]
use super::search_terms::{push_sampled_search_text, search_query_tokens};
pub(super) use super::transcript_search::source::SourceBuckets;
use super::transcript_search::source::{BucketSink, Source, Walk, message_role_source, push_mark};
#[cfg(test)]
use search_text::raw_value_search_excerpt;
pub(super) use search_text::{
    build_search_index, classified_search_excerpt, classify_json_into, raw_value_display_text,
};

#[cfg(test)]
pub(super) fn session_matches_query(session: &SessionInfo, query: &str) -> bool {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
        return true;
    }

    if session.search_index.contains(&normalized) {
        return true;
    }

    session_transcript_contains_query(session, &normalized)
}

/// Fast in-memory matcher for interactive picker filtering.
///
/// Splits the query into whitespace-separated tokens and requires *every* token
/// to appear somewhere in the session's search index (logical AND, order
/// independent). This is far more forgiving than a single contiguous substring
/// match - `api deploy` now matches a session mentioning "deploy ... api" - while
/// staying cheap: it runs on every keystroke and only does N case-insensitive
/// substring scans over an already-lowercased index.
///
/// This intentionally avoids transcript file I/O. Transcript-backed content can
/// still become searchable after preview load because the picker refreshes the
/// session's cached `search_index` from the loaded preview.
#[cfg(test)]
pub(super) fn session_matches_picker_query(session: &SessionInfo, query: &str) -> bool {
    let tokens = search_query_tokens(query);
    tokens.is_empty()
        || tokens
            .iter()
            .all(|token| session.search_index.contains(token))
}

#[cfg(test)]
fn session_transcript_contains_query(session: &SessionInfo, query_lower: &str) -> bool {
    transcript_paths_for_session(session)
        .into_iter()
        .any(|path| file_contains_case_insensitive_query(&path, query_lower))
}

#[cfg(test)]
fn transcript_paths_for_session(session: &SessionInfo) -> Vec<PathBuf> {
    match &session.resume_target {
        ResumeTarget::JcodeSession { session_id } => {
            let Ok(sessions_dir) = storage::jcode_dir().map(|dir| dir.join("sessions")) else {
                return Vec::new();
            };
            vec![
                sessions_dir.join(format!("{session_id}.json")),
                sessions_dir.join(format!("{session_id}.journal.jsonl")),
            ]
        }
        ResumeTarget::ClaudeCodeSession { session_path, .. }
        | ResumeTarget::CodexSession { session_path, .. }
        | ResumeTarget::PiSession { session_path }
        | ResumeTarget::OpenCodeSession { session_path, .. }
        | ResumeTarget::CursorSession { session_path, .. } => {
            vec![PathBuf::from(session_path)]
        }
    }
}

#[cfg(test)]
fn file_contains_case_insensitive_query(path: &Path, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    if !path.exists() {
        return false;
    }

    if query_lower.is_ascii() {
        return file_contains_ascii_case_insensitive(path, query_lower.as_bytes());
    }

    std::fs::read_to_string(path)
        .ok()
        .map(|content| content.to_lowercase().contains(query_lower))
        .unwrap_or(false)
}

#[cfg(test)]
fn file_contains_ascii_case_insensitive(path: &Path, needle_lower: &[u8]) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let overlap = needle_lower.len().saturating_sub(1);
    let mut carry = Vec::with_capacity(overlap);
    let mut buf = vec![0u8; TRANSCRIPT_SEARCH_CHUNK_BYTES];

    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(read) => read,
            Err(_) => return false,
        };

        let mut window = Vec::with_capacity(carry.len() + read);
        window.extend_from_slice(&carry);
        window.extend_from_slice(&buf[..read]);

        if contains_ascii_case_insensitive_bytes(&window, needle_lower) {
            return true;
        }

        carry.clear();
        let keep = overlap.min(window.len());
        carry.extend_from_slice(&window[window.len().saturating_sub(keep)..]);
    }
}

#[cfg(test)]
fn contains_ascii_case_insensitive_bytes(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.len() > haystack.len() {
        return false;
    }

    haystack.windows(needle_lower.len()).any(|window| {
        window
            .iter()
            .zip(needle_lower.iter())
            .all(|(&hay, &needle)| hay.to_ascii_lowercase() == needle)
    })
}

/// An external transcript preview plus search text classified by source.
pub(super) struct ExternalPreview {
    pub messages: Vec<PreviewMessage>,
    pub search: SourceBuckets,
}

impl ExternalPreview {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            search: SourceBuckets::default(),
        }
    }

    fn non_empty(self) -> Option<Self> {
        (!self.messages.is_empty()).then_some(self)
    }
}

/// Search index whose transcript part comes from classified samples.
pub(super) fn build_classified_search_index(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
    search: &SourceBuckets,
) -> String {
    let mut text = String::new();
    search.render(&mut text);
    build_search_index_from_summary(id, short_name, title, working_dir, save_label, &text)
}

fn build_search_index_from_summary(
    id: &str,
    short_name: &str,
    title: &str,
    working_dir: Option<&str>,
    save_label: Option<&str>,
    transcript_search_text: &str,
) -> String {
    let mut combined =
        search_text::search_index_header(id, short_name, title, working_dir, save_label);

    if !transcript_search_text.is_empty() {
        combined.push(' ');
        combined.push_str(transcript_search_text);
    }

    combined.to_lowercase()
}

#[path = "loading/session_candidates.rs"]
mod session_candidates;
#[allow(unused_imports)]
pub(crate) use session_candidates::*;

#[path = "loading/session_summary.rs"]
mod session_summary;
#[allow(unused_imports)]
pub(crate) use session_summary::*;

fn load_session_summary(path: &Path) -> Result<SessionSummary> {
    let bytes = std::fs::read(path)?;
    let mut summary: SessionSummary = serde_json::from_slice(&bytes)?;

    let journal_path = session::session_journal_path_from_snapshot(path);
    if journal_path.exists() {
        let file = File::open(&journal_path)?;
        let reader = BufReader::new(file);
        for (line_idx, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<SessionJournalSummaryEntry>(trimmed) {
                Ok(entry) => {
                    summary.parent_id = entry.meta.parent_id;
                    summary.title = entry.meta.title;
                    summary.custom_title = entry.meta.custom_title;
                    summary.updated_at = entry.meta.updated_at;
                    summary.last_active_at = entry.meta.last_active_at;
                    summary.working_dir = entry.meta.working_dir;
                    summary.short_name = entry.meta.short_name;
                    summary.provider_key = entry.meta.provider_key;
                    summary.model = entry.meta.model;
                    summary.is_canary = entry.meta.is_canary;
                    summary.is_debug = entry.meta.is_debug;
                    if let Some(saved) = entry.meta.saved {
                        summary.saved = saved;
                    }
                    summary.save_label = entry.meta.save_label;
                    summary.status = entry.meta.status;
                    summary.messages.merge(entry.append_messages);
                }
                Err(err) => {
                    crate::logging::warn(&format!(
                        "Session picker journal parse failed at {} line {}: {}",
                        journal_path.display(),
                        line_idx + 1,
                        err
                    ));
                    break;
                }
            }
        }
    }

    Ok(summary)
}

pub(super) fn build_messages_preview(session: &Session) -> Vec<PreviewMessage> {
    build_messages_preview_with_offset(session).0
}

/// Trailing preview messages plus the number of user prompts dropped from the
/// head, so the preview can number prompts globally like the live chat.
pub(super) fn build_messages_preview_with_offset(
    session: &Session,
) -> (Vec<PreviewMessage>, usize) {
    let rendered = session::render_messages(session);
    let skip = rendered.len().saturating_sub(PREVIEW_MESSAGE_LIMIT);
    let hidden_prompts = rendered[..skip]
        .iter()
        .filter(|msg| msg.role == "user" && !msg.content.trim().is_empty())
        .count();
    let preview = rendered
        .into_iter()
        .skip(skip)
        .map(crate::tui::DisplayMessage::from_rendered_message)
        .map(|msg| PreviewMessage {
            role: msg.role,
            content: msg.content,
            tool_calls: msg.tool_calls,
            tool_data: msg.tool_data,
            duration_secs: msg.duration_secs,
            title: msg.title,
            timestamp: None,
        })
        .collect();
    (preview, hidden_prompts)
}

pub(super) fn crashed_sessions_from_all_sessions(
    sessions: &[SessionInfo],
) -> Option<CrashedSessionsInfo> {
    let recovered_parents: HashSet<&str> = sessions
        .iter()
        .filter(|s| s.id.starts_with("session_recovery_"))
        .filter_map(|s| s.parent_id.as_deref())
        .collect();

    let mut crashed: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| matches!(s.status, SessionStatus::Crashed { .. }))
        .filter(|s| !recovered_parents.contains(s.id.as_str()))
        .collect();
    if crashed.is_empty() {
        return None;
    }

    let total_unrecovered_crashed = crashed.len();
    let crash_timestamp =
        |session: &SessionInfo| session.last_active_at.unwrap_or(session.last_message_time);
    let most_recent = crashed
        .iter()
        .map(|session| crash_timestamp(session))
        .max()?;
    let crash_window = chrono::Duration::seconds(60);
    crashed.retain(|s| {
        let delta = most_recent.signed_duration_since(crash_timestamp(s));
        delta >= chrono::Duration::zero() && delta <= crash_window
    });
    if crashed.is_empty() {
        return None;
    }

    crashed.sort_by(|a, b| b.last_message_time.cmp(&a.last_message_time));

    Some(CrashedSessionsInfo {
        session_ids: crashed.iter().map(|s| s.id.clone()).collect(),
        display_names: crashed.iter().map(|s| s.short_name.clone()).collect(),
        most_recent_crash: most_recent,
        omitted_crashed_count: total_unrecovered_crashed.saturating_sub(crashed.len()),
    })
}

/// Parse a single jcode session snapshot (+ journal) into a [`SessionInfo`],
/// returning `None` for empty/imported sessions or read/parse errors. Pulled out
/// of `load_sessions` so the summary pass can run across a scoped thread pool.
fn parse_jcode_session_info(
    sessions_dir: &Path,
    stem: &str,
    catchup_seen: &crate::catchup::CatchupSeenSnapshot,
) -> Option<SessionInfo> {
    // Imported stems are filtered out by `collect_recent_session_candidates`, but
    // keep the cheap defensive check so this helper is safe to call directly.
    if stem.starts_with("imported_cc_")
        || stem.starts_with("imported_codex_")
        || stem.starts_with("imported_pi_")
        || stem.starts_with("imported_opencode_")
        || stem.starts_with("imported_cursor_")
    {
        return None;
    }

    let path = sessions_dir.join(format!("{stem}.json"));
    let session = load_session_summary(&path).ok()?;
    if session.hidden && !crate::session::Session::side_question_was_promoted(stem) {
        return None;
    }

    let visible_message_count = session.messages.visible_message_count;
    if visible_message_count == 0 {
        return None;
    }

    let short_name = session
        .short_name
        .clone()
        .or_else(|| extract_session_name(stem).map(|s| s.to_string()))
        .unwrap_or_else(|| stem.to_string());
    let icon = session_icon(&short_name);

    let user_message_count = session.messages.user_message_count;
    let assistant_message_count = session.messages.assistant_message_count;
    let estimated_tokens = session.messages.estimated_tokens;

    let status = session.status.clone();
    let needs_catchup = catchup_seen.needs_catchup(stem, session.updated_at, &status);
    let source = classify_session_source(
        stem,
        session.provider_key.as_deref(),
        session.model.as_deref(),
    );

    let todo_summary = crate::todo::load_session_todo_summary(stem);
    let title = session
        .custom_title
        .or(session.title)
        .or_else(|| {
            todo_summary
                .title
                .map(|title| truncate_title_text(&title, 72))
        })
        .unwrap_or_else(|| short_name.clone());
    let mut search_index = build_search_index_from_summary(
        stem,
        &short_name,
        &title,
        session.working_dir.as_deref(),
        session.save_label.as_deref(),
        &{
            session.messages.rendered_search_text()
        },
    );
    if let Some(intention) = todo_summary.user_intention.as_deref() {
        search_index.push(' ');
        search_index.push_str(&intention.to_lowercase());
    }

    Some(SessionInfo {
        id: stem.to_string(),
        parent_id: session.parent_id,
        short_name,
        icon: icon.to_string(),
        title,
        message_count: visible_message_count,
        user_message_count,
        assistant_message_count,
        created_at: session.created_at,
        // Newest message, falling back to the save time for old snapshots
        // whose messages predate per-message timestamps (or empty sessions).
        last_message_time: session
            .messages
            .newest_message_at()
            .unwrap_or(session.updated_at),
        last_active_at: session.last_active_at,
        working_dir: session.working_dir,
        model: session.model,
        provider_key: session.provider_key,
        is_canary: session.is_canary,
        is_debug: session.is_debug,
        saved: session.saved,
        save_label: session.save_label,
        status,
        needs_catchup,
        estimated_tokens,
        first_user_prompt: session.messages.first_user_prompt,
        user_intention: todo_summary.user_intention,
        messages_preview: Vec::new(),
        search_index,
        server_name: None,
        server_icon: None,
        source,
        resume_target: ResumeTarget::JcodeSession {
            session_id: stem.to_string(),
        },
        external_path: None,
        todo_statuses: todo_summary.todo_statuses,
    })
}

pub fn load_sessions() -> Result<Vec<SessionInfo>> {
    load_sessions_inner(false)
}

fn load_sessions_inner(bypass_cache: bool) -> Result<Vec<SessionInfo>> {
    let sessions_dir = storage::jcode_dir()?.join("sessions");
    let scan_limit = session_scan_limit();
    let want_external = include_external_sessions();

    let cache_generation = if let Ok(state) = session_list_cache().lock() {
        if !bypass_cache
            && let Some(entry) = state.entry.as_ref()
            && entry.sessions_dir == sessions_dir
            && entry.scan_limit == scan_limit
            && entry.external_sessions == want_external
            && entry.loaded_at.elapsed() <= SESSION_LIST_CACHE_TTL
        {
            return Ok(entry.sessions.clone());
        }
        state.generation
    } else {
        0
    };

    let candidates = if sessions_dir.exists() {
        // Keep startup responsive by avoiding `session_has_history` here. That helper parses
        // snapshots/journals, and `load_session_summary` below parses the same files again.
        // Instead, gather a recency-ordered candidate window cheaply from metadata and let the
        // single summary pass filter empty sessions while filling up to `scan_limit` entries.
        let mut candidates =
            collect_recent_session_candidates(&sessions_dir, session_candidate_window(scan_limit))?;
        if include_old_saved_sessions_on_initial_load() {
            let mut seen: HashSet<String> = candidates.iter().cloned().collect();
            for stem in collect_saved_session_candidates(&sessions_dir)? {
                if seen.insert(stem.clone()) {
                    candidates.push(stem);
                }
            }
        }
        candidates
    } else {
        Vec::new()
    };

    // Loading the catch-up "seen" state once (instead of per session) avoids
    // re-reading and re-parsing `catchup_seen.json` for every candidate.
    let catchup_seen = crate::catchup::CatchupSeenSnapshot::load();
    let sessions_dir_ref = &sessions_dir;
    let catchup_ref = &catchup_seen;

    let (mut sessions, external_sessions) = std::thread::scope(|scope| {
        // One handle for all five external scans (they fan out internally), so
        // the opt-out is a single branch and external work still overlaps the
        // jcode session parsing below.
        let external_handle =
            scope.spawn(move || load_external_sessions(want_external, scan_limit));

        // Phase 1: walk the recency-ordered candidates in parallel windows until
        // we have collected `scan_limit` non-empty sessions. `boundary` marks the
        // candidate index where the serial fill would start applying the saved
        // gate, so beyond it we only keep saved sessions (Phase 2). Parsing each
        // window in parallel keeps the per-file JSON cost off the critical path.
        //
        // Windows are sized to `scan_limit`: only the final window (the one that
        // crosses `scan_limit`) can over-parse, so wasted work is bounded to a
        // single window's worth of candidates while still parallelizing widely.
        let mut sessions: Vec<SessionInfo> = Vec::new();
        // Debug/canary sessions are hidden in the default picker view. Do not let a
        // burst of self-dev or swarm workers consume the entire recency budget and
        // crowd out ordinary sessions. Keep a separate bounded debug budget so the
        // test-session toggle still has useful recent entries without making the
        // default list appear to jump from a handful of Jcode rows straight to old
        // external transcripts.
        let mut visible_session_count = 0usize;
        let mut debug_session_count = 0usize;
        let mut boundary = candidates.len();
        let window = scan_limit.max(1);
        let mut start = 0;
        'fill: while start < candidates.len() {
            let end = (start + window).min(candidates.len());
            let batch = candidates[start..end].to_vec();
            let parsed = parallel_map(batch, move |stem| {
                parse_jcode_session_info(sessions_dir_ref, &stem, catchup_ref)
            });
            for (offset, parsed_session) in parsed.into_iter().enumerate() {
                if let Some(info) = parsed_session {
                    if info.is_debug {
                        if debug_session_count < scan_limit {
                            debug_session_count += 1;
                            sessions.push(info);
                        }
                    } else {
                        visible_session_count += 1;
                        sessions.push(info);
                    }
                    if visible_session_count >= scan_limit {
                        boundary = start + offset + 1;
                        break 'fill;
                    }
                }
            }
            start = end;
        }

        // Phase 2: beyond the fill boundary the serial loader only keeps saved
        // sessions. Compute the cheap saved tail-gate across the remaining
        // candidates in parallel, then fully parse just the gate-passers.
        if boundary < candidates.len() {
            let tail: Vec<String> = candidates[boundary..].to_vec();
            let gate_passers: Vec<String> = parallel_map(tail, move |stem| {
                let path = sessions_dir_ref.join(format!("{stem}.json"));
                session_snapshot_or_journal_has_saved_metadata(&path).then_some(stem)
            })
            .into_iter()
            .flatten()
            .collect();
            let saved_sessions = parallel_map(gate_passers, move |stem| {
                parse_jcode_session_info(sessions_dir_ref, &stem, catchup_ref)
            });
            sessions.extend(saved_sessions.into_iter().flatten());
        }

        (
            sessions,
            external_handle.join().unwrap_or_else(|_| {
                crate::logging::debug("External session loader thread failed");
                Vec::new()
            }),
        )
    });
    sessions.extend(external_sessions);

    // A moved session's local snapshot stops changing. Keep it discoverable
    // even after it falls outside the recency window, without waking the host
    // or making an SSH connection just to open the picker.
    include_cloud_sessions(&sessions_dir, &catchup_seen, &mut sessions);

    limit_sessions_by_recency(&mut sessions, scan_limit);

    #[cfg(test)]
    run_before_session_list_cache_publish_hook();

    if let Ok(mut state) = session_list_cache().lock()
        && state.generation == cache_generation
    {
        state.entry = Some(SessionListCacheEntry {
            loaded_at: Instant::now(),
            sessions_dir,
            scan_limit,
            external_sessions: want_external,
            sessions: sessions.clone(),
        });
    }

    Ok(sessions)
}

/// Apply a single visible budget after merging every source. Hidden test sessions
/// have their own bounded budget so toggling them remains useful without pushing
/// ordinary conversations out of the most recent results. Saved and cloud
/// sessions compete by recency too, rather than reserving extra visible slots.
fn limit_sessions_by_recency(sessions: &mut Vec<SessionInfo>, limit: usize) {
    sessions.sort_by_cached_key(|s| std::cmp::Reverse(super::session_resume_time(s)));
    let mut visible = 0;
    let mut debug = 0;
    sessions.retain(|session| {
        let count = if session.is_debug {
            &mut debug
        } else {
            &mut visible
        };
        *count += 1;
        *count <= limit
    });
}

#[path = "loading/external_sessions.rs"]
mod external_sessions;
#[allow(unused_imports)]
pub(crate) use external_sessions::*;

#[path = "loading/pi_sessions.rs"]
mod pi_sessions;
use pi_sessions::load_external_pi_sessions;
pub(super) use pi_sessions::load_pi_external_preview;
fn load_external_opencode_sessions(scan_limit: usize) -> Vec<SessionInfo> {
    let Ok(root) = crate::storage::user_home_path(".local/share/opencode/storage/session") else {
        return Vec::new();
    };
    if !root.exists() {
        return Vec::new();
    }

    let paths = collect_recent_files_recursive(&root, "json", scan_limit);
    parallel_map(paths, |path| {
        load_opencode_session_stub(&path).ok().flatten()
    })
    .into_iter()
    .flatten()
    .collect()
}

pub(super) fn load_opencode_external_preview(path: &Path) -> Option<ExternalPreview> {
    let (session, search) = load_opencode_session_info(path).ok().flatten()?;
    Some(ExternalPreview {
        messages: session.messages_preview,
        search,
    })
}

fn load_opencode_session_stub(path: &Path) -> Result<Option<SessionInfo>> {
    let value: serde_json::Value = serde_json::from_reader(File::open(path)?)?;
    let session_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Ok(None);
    }

    let created_at = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(|v| v.as_i64())
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .unwrap_or_else(chrono::Utc::now);
    let last_message_time = value
        .get("time")
        .and_then(|time| time.get("updated"))
        .and_then(|v| v.as_i64())
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .unwrap_or(created_at);
    let working_dir = value
        .get("directory")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let short_name = format!(
        "opencode {}",
        jcode_core::util::truncate_str(&session_id, 8)
    );
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| truncate_title_text(s, 72))
        .unwrap_or_else(|| {
            format!(
                "OpenCode session {}",
                jcode_core::util::truncate_str(&session_id, 8)
            )
        });
    let search_index = build_search_index(
        &format!("opencode:{session_id}"),
        &short_name,
        &title,
        working_dir.as_deref(),
        None,
        &[],
    );

    Ok(Some(SessionInfo {
        id: format!("opencode:{session_id}"),
        parent_id: None,
        short_name,
        icon: "◌".to_string(),
        title,
        message_count: 0,
        user_message_count: 0,
        assistant_message_count: 0,
        created_at,
        last_message_time,
        last_active_at: Some(last_message_time),
        working_dir,
        model: None,
        provider_key: Some("opencode".to_string()),
        is_canary: false,
        is_debug: false,
        saved: false,
        save_label: None,
        status: SessionStatus::Closed,
        needs_catchup: false,
        estimated_tokens: 0,
        first_user_prompt: None,
        messages_preview: Vec::new(),
        search_index,
        server_name: None,
        server_icon: None,
        source: SessionSource::OpenCode,
        resume_target: ResumeTarget::OpenCodeSession {
            session_id,
            session_path: path.to_string_lossy().to_string(),
        },
        external_path: Some(path.to_string_lossy().to_string()),
        user_intention: None,
        todo_statuses: Vec::new(),
    }))
}

fn load_external_cursor_sessions(scan_limit: usize) -> Vec<SessionInfo> {
    let Ok(root) = crate::storage::user_home_path(".cursor/projects") else {
        return Vec::new();
    };
    if !root.exists() {
        return Vec::new();
    }

    let paths = collect_recent_files_recursive(&root, "jsonl", scan_limit);
    parallel_map(paths, |path| load_cursor_session_stub(&path).ok().flatten())
        .into_iter()
        .flatten()
        .collect()
}

pub(super) fn load_cursor_external_preview(path: &Path) -> Option<ExternalPreview> {
    // Only parse the tail of the transcript like the other external CLIs: the
    // preview shows the last ~20 messages, so reparsing large transcripts on
    // every selection change would make picker navigation lag.
    let (text, truncated) = read_file_tail_text(path, EXTERNAL_PREVIEW_TAIL_BYTES)?;
    let mut out = ExternalPreview::new();
    let skip = usize::from(truncated);
    for line in text.lines().skip(skip) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let role = match value
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "user" | "human" => "user",
            "assistant" | "model" => "assistant",
            _ => continue,
        };
        let content = value
            .get("message")
            .and_then(|message| message.get("content"))
            .or_else(|| value.get("content"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let text = crate::import::extract_external_text_from_json_value(&content, false);
        push_preview_message(&mut out.messages, role, text);
        classify_json_into(&mut out.search, &content, message_role_source(role));
    }
    out.non_empty()
}

fn load_cursor_session_stub(path: &Path) -> Result<Option<SessionInfo>> {
    // Cursor nests subagent runs under `agent-transcripts/<parent>/subagents/`.
    // Those are not independently resumable, so skip them in the resume list.
    if crate::import::is_cursor_subagent_transcript(path) {
        return Ok(None);
    }
    // Cursor transcripts have no header line: the session id is the file stem
    // (a UUID) and metadata is enriched from the path / file mtime.
    let session_id = crate::import::cursor_session_id_from_path(path);
    if session_id.is_empty() {
        return Ok(None);
    }

    // A transcript counts as resumable only if it has at least one visible
    // user/assistant message; otherwise skip it (mirrors the other CLIs).
    let mut first_user_text: Option<String> = None;
    let mut has_message = false;
    let file = File::open(path)?;
    for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let role = match value
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "user" | "human" => "user",
            "assistant" | "model" => "assistant",
            _ => continue,
        };
        let content = value
            .get("message")
            .and_then(|message| message.get("content"))
            .or_else(|| value.get("content"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let text = crate::import::extract_external_text_from_json_value(&content, false);
        if text.trim().is_empty() {
            continue;
        }
        has_message = true;
        if first_user_text.is_none() && role == "user" {
            first_user_text = Some(text);
        }
    }
    if !has_message {
        return Ok(None);
    }

    let last_message_time = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_else(|_| chrono::Utc::now());
    let created_at = last_message_time;
    let working_dir = crate::import::cursor_cwd_from_transcript_path(path);
    let short_name = working_dir
        .as_deref()
        .and_then(|dir| Path::new(dir).file_name())
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| format!("cursor {}", jcode_core::util::truncate_str(&session_id, 8)));
    let title = first_user_text
        .as_deref()
        .map(|text| truncate_title_text(text, 72))
        .unwrap_or_else(|| {
            format!(
                "Cursor session {}",
                jcode_core::util::truncate_str(&session_id, 8)
            )
        });
    let search_index = build_search_index(
        &format!("cursor:{session_id}"),
        &short_name,
        &title,
        working_dir.as_deref(),
        None,
        &[],
    );

    Ok(Some(SessionInfo {
        id: format!("cursor:{session_id}"),
        parent_id: None,
        short_name,
        icon: "▮".to_string(),
        title,
        message_count: 0,
        user_message_count: 0,
        assistant_message_count: 0,
        created_at,
        last_message_time,
        last_active_at: Some(last_message_time),
        working_dir,
        model: None,
        provider_key: Some("cursor".to_string()),
        is_canary: false,
        is_debug: false,
        saved: false,
        save_label: None,
        status: SessionStatus::Closed,
        needs_catchup: false,
        estimated_tokens: 0,
        first_user_prompt: first_user_text,
        messages_preview: Vec::new(),
        search_index,
        server_name: None,
        server_icon: None,
        source: SessionSource::Cursor,
        resume_target: ResumeTarget::CursorSession {
            session_id,
            session_path: path.to_string_lossy().to_string(),
        },
        external_path: Some(path.to_string_lossy().to_string()),
        user_intention: None,
        todo_statuses: Vec::new(),
    }))
}

pub fn load_servers() -> Vec<ServerInfo> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async { registry::list_servers().await.unwrap_or_default() })
        })
    } else {
        tokio::runtime::Runtime::new()
            .map(|rt| rt.block_on(async { registry::list_servers().await.unwrap_or_default() }))
            .unwrap_or_default()
    }
}

fn include_cloud_sessions(
    sessions_dir: &Path,
    catchup_seen: &crate::catchup::CatchupSeenSnapshot,
    sessions: &mut Vec<SessionInfo>,
) {
    let Some(entries) = storage::session_leases_dir().and_then(|dir| std::fs::read_dir(dir).ok())
    else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(lease) = storage::read_session_lease(id) else {
            continue;
        };
        let Some(host) = lease
            .away_host
            .as_ref()
            .filter(|host| !host.trim().is_empty())
        else {
            continue;
        };
        if lease.session_id != id {
            continue;
        }
        if !sessions.iter().any(|session| session.id == id) {
            let session = parse_jcode_session_info(sessions_dir, id, catchup_seen)
                .unwrap_or_else(|| cloud_session_stub(&lease));
            sessions.push(session);
        }
        if let Some(session) = sessions.iter_mut().find(|session| session.id == id) {
            // A migrated local snapshot is historical. Verified cloud metadata
            // must win for display, project scoping, sorting, and search.
            if let Some(title) = mobile_inventory::title(id) {
                session.title = title;
                session.working_dir = lease.repo_root.clone();
                if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&lease.updated_at) {
                    let updated_at = updated_at.with_timezone(&chrono::Utc);
                    session.last_message_time = updated_at;
                    session.last_active_at = Some(updated_at);
                    session.needs_catchup =
                        catchup_seen.needs_catchup(id, updated_at, &session.status);
                }
                session.search_index = build_search_index_from_summary(
                    id,
                    &session.short_name,
                    &session.title,
                    session.working_dir.as_deref(),
                    session.save_label.as_deref(),
                    &session.search_index,
                );
            }
            session.server_name = Some(format!("cloud: {host}"));
            session.server_icon = Some("☁".to_string());
            // Host metadata, not transcript text from the preceding section.
            push_mark(&mut session.search_index, Source::Other);
            session
                .search_index
                .push_str(&format!("cloud {}", host.to_lowercase()));
        }
    }
}

/// A migration can leave only a lease or a hidden handoff notice locally.
/// The ownership record is enough to attach without inventing transcript data.
fn cloud_session_stub(lease: &storage::SessionLease) -> SessionInfo {
    let id = &lease.session_id;
    let short_name = extract_session_name(id).unwrap_or(id).to_string();
    let updated_at = chrono::DateTime::parse_from_rfc3339(&lease.updated_at)
        .map(|time| time.with_timezone(&chrono::Utc))
        .unwrap_or_default();
    let title =
        mobile_inventory::title(id).unwrap_or_else(|| format!("{short_name} (cloud session)"));
    let search_index = build_search_index(
        id,
        &short_name,
        &title,
        lease.repo_root.as_deref(),
        None,
        &[],
    );
    SessionInfo {
        id: id.clone(),
        parent_id: None,
        icon: session_icon(&short_name).to_string(),
        short_name,
        title,
        message_count: 0,
        user_message_count: 0,
        assistant_message_count: 0,
        created_at: updated_at,
        last_message_time: updated_at,
        last_active_at: Some(updated_at),
        working_dir: lease.repo_root.clone(),
        model: None,
        provider_key: None,
        is_canary: false,
        is_debug: false,
        saved: false,
        save_label: None,
        status: SessionStatus::Closed,
        needs_catchup: false,
        estimated_tokens: 0,
        first_user_prompt: None,
        messages_preview: Vec::new(),
        search_index,
        server_name: None,
        server_icon: None,
        source: SessionSource::Jcode,
        resume_target: ResumeTarget::JcodeSession {
            session_id: id.clone(),
        },
        external_path: None,
        user_intention: None,
        todo_statuses: Vec::new(),
    }
}

pub fn load_sessions_grouped() -> Result<(Vec<ServerGroup>, Vec<SessionInfo>)> {
    let sessions_dir = storage::jcode_dir()?.join("sessions");
    let scan_limit = session_scan_limit();
    // An explicit picker refresh must see newly closed/created sessions even
    // when another scan populated the five-second in-process cache recently.
    let all_sessions = load_sessions_inner(true)?;
    let servers = load_servers();

    let mut session_to_server: HashMap<String, &ServerInfo> = HashMap::new();
    for server in &servers {
        for session_name in &server.sessions {
            session_to_server.insert(session_name.clone(), server);
        }
    }

    let mut server_sessions: HashMap<String, Vec<SessionInfo>> = HashMap::new();
    let mut cloud_sessions: HashMap<String, Vec<SessionInfo>> = HashMap::new();
    let mut orphan_sessions: Vec<SessionInfo> = Vec::new();

    for mut session in all_sessions {
        // Remote ownership wins over a stale entry in the local live registry.
        if session.server_icon.as_deref() == Some("☁")
            && let Some(name) = session.server_name.clone()
        {
            cloud_sessions.entry(name).or_default().push(session);
            continue;
        }
        if let Some(server) = session_to_server.get(&session.short_name) {
            session.server_name = Some(server.name.clone());
            session.server_icon = Some(server.icon.clone());
            server_sessions
                .entry(server.name.clone())
                .or_default()
                .push(session);
        } else {
            orphan_sessions.push(session);
        }
    }

    let mut groups: Vec<ServerGroup> = servers
        .iter()
        .map(|server| {
            let sessions = server_sessions.remove(&server.name).unwrap_or_default();
            ServerGroup {
                name: server.name.clone(),
                icon: server.icon.clone(),
                version: server.version.clone(),
                git_hash: server.git_hash.clone(),
                is_running: true,
                sessions,
            }
        })
        .collect();

    groups.extend(
        cloud_sessions
            .into_iter()
            .map(|(name, sessions)| ServerGroup {
                name,
                icon: "☁".to_string(),
                version: String::new(),
                git_hash: String::new(),
                // The lease records ownership, not whether the remote daemon is live.
                is_running: false,
                sessions,
            }),
    );

    groups.sort_by(|a, b| {
        let a_latest = a.sessions.iter().map(|s| s.last_message_time).max();
        let b_latest = b.sessions.iter().map(|s| s.last_message_time).max();
        b_latest.cmp(&a_latest)
    });

    write_grouped_session_list_disk_cache(&sessions_dir, scan_limit, &groups, &orphan_sessions);

    Ok((groups, orphan_sessions))
}

/// Load only the sessions for a single external CLI (Codex or Claude Code),
/// returned as orphan [`SessionInfo`] grouped output compatible with
/// `SessionPicker::new_grouped`.
///
/// Kept as a focused test helper for the external transcript importers.
#[cfg(test)]
pub(crate) fn load_external_cli_sessions_grouped(
    cli: crate::tui::session_picker::ExternalCli,
) -> (Vec<ServerGroup>, Vec<SessionInfo>) {
    use crate::tui::session_picker::ExternalCli;
    let scan_limit = session_scan_limit();
    let sessions = match cli {
        ExternalCli::Codex => load_external_codex_sessions(scan_limit),
        ExternalCli::ClaudeCode => load_external_claude_code_sessions(scan_limit),
        ExternalCli::Pi => load_external_pi_sessions(scan_limit),
        ExternalCli::OpenCode => load_external_opencode_sessions(scan_limit),
        ExternalCli::Cursor => load_external_cursor_sessions(scan_limit),
    };
    (Vec::new(), sessions)
}

#[cfg(test)]
#[path = "loading/tests.rs"]
mod tests;

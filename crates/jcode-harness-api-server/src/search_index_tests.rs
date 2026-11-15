//! Index, ranking and tokenization tests for cross-session search.

use super::*;
use jcode_harness_api::SessionSearchRole;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn test_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jcode-search-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("sessions")).expect("create test dir");
    path
}

fn block(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "text", "text": text})
}

fn user_message(id: &str, text: &str, timestamp: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "role": "user",
        "content": [block(text)],
        "timestamp": timestamp,
    })
}

fn assistant_message(id: &str, timestamp: &str, content: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "role": "assistant",
        "content": content,
        "timestamp": timestamp,
    })
}

/// Write a session snapshot the way the runtime stores one.
fn write_session(dir: &Path, id: &str, title: &str, messages: Vec<serde_json::Value>) -> PathBuf {
    let snapshot = serde_json::json!({
        "id": id,
        "title": title,
        "created_at": "2026-01-02T03:04:05.000Z",
        "updated_at": "2026-01-02T04:05:06.000Z",
        "messages": messages,
    });
    let path = dir.join("sessions").join(format!("{id}.json"));
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).expect("write session");
    path
}

fn index_for(dir: &Path) -> SearchIndex {
    let mut index =
        SearchIndex::open_at(&dir.join("search.db"), &dir.join("sessions")).expect("open index");
    index.sync();
    index
}

fn session_ids(results: &SessionSearchResults) -> Vec<String> {
    results
        .messages
        .iter()
        .map(|hit| hit.session_id.clone())
        .collect()
}

#[test]
fn user_and_assistant_text_is_searchable_but_tool_traffic_is_not() {
    let dir = test_dir("tool-traffic");
    // The needle appears in a user prompt, in assistant final text, and inside
    // a tool call's input. Only the two human-readable occurrences may match.
    write_session(
        &dir,
        "session_alpha",
        "Unrelated title",
        vec![
            user_message(
                "m1",
                "please check the flux capacitor",
                "2026-01-02T03:04:06Z",
            ),
            assistant_message(
                "m2",
                "2026-01-02T03:05:00Z",
                serde_json::json!([
                    {"type": "tool_use", "id": "t1", "name": "bash",
                     "input": {"command": "grep -r flux /tmp/capacitor"}},
                ]),
            ),
            assistant_message(
                "m3",
                "2026-01-02T03:06:00Z",
                serde_json::json!([block("the flux capacitor is fine")]),
            ),
        ],
    );

    let mut index = index_for(&dir);
    let results = index.search("flux capacitor", None).unwrap();
    let hits: Vec<SessionSearchRole> = results.messages.iter().map(|hit| hit.role).collect();
    assert_eq!(
        hits,
        vec![SessionSearchRole::User, SessionSearchRole::Assistant],
        "prompt first, then the answer; tool input never indexed"
    );
    // `grep` and `/tmp/capacitor` only exist in the tool input.
    assert!(index.search("grep", None).unwrap().messages.is_empty());
    assert!(index.search("tmp", None).unwrap().messages.is_empty());
}

#[test]
fn user_prompts_outrank_equally_matching_assistant_text() {
    let dir = test_dir("boost");
    write_session(
        &dir,
        "session_boost",
        "Rust",
        vec![
            user_message("m1", "kosmos", "2026-01-02T03:10:00Z"),
            assistant_message(
                "m2",
                "2026-01-02T03:11:00Z",
                serde_json::json!([block("kosmos")]),
            ),
        ],
    );

    let results = index_for(&dir).search("kosmos", None).unwrap();
    assert_eq!(results.messages.len(), 2);
    assert_eq!(results.messages[0].role, SessionSearchRole::User);
    assert_eq!(results.messages[0].message_id.as_deref(), Some("m1"));
}

#[test]
fn titles_are_their_own_section_and_unaffected_by_message_ranking() {
    let dir = test_dir("titles");
    write_session(
        &dir,
        "session_titled",
        "Otters and dictation",
        vec![user_message("m1", "unrelated body", "2026-01-02T03:04:06Z")],
    );
    write_session(
        &dir,
        "session_body",
        "Something else",
        vec![user_message(
            "m1",
            "otters dictation otters dictation",
            "2026-01-02T03:04:06Z",
        )],
    );

    let results = index_for(&dir).search("otters", None).unwrap();
    assert_eq!(results.titles.len(), 1);
    assert_eq!(results.titles[0].session_id, "session_titled");
    assert_eq!(results.titles[0].role, SessionSearchRole::Title);
    assert!(results.titles[0].message_id.is_none());
    assert_eq!(results.titles[0].title, "Otters and dictation");
    // Highlights mark exactly the matched word inside the snippet.
    let highlight = results.titles[0].highlights[0];
    let snippet: String = results.titles[0]
        .snippet
        .chars()
        .skip(highlight.start as usize)
        .take((highlight.end - highlight.start) as usize)
        .collect();
    assert_eq!(snippet.to_lowercase(), "otters");
    assert_eq!(session_ids(&results), vec!["session_body".to_string()]);
}

#[test]
fn cyrillic_and_diacritics_tokenize_with_unicode61() {
    let dir = test_dir("unicode");
    write_session(
        &dir,
        "session_cyr",
        "Совещание по релизу",
        vec![
            user_message("m1", "Проверь ещё раз этот релиз", "2026-01-02T03:04:06Z"),
            assistant_message(
                "m2",
                "2026-01-02T03:05:00Z",
                serde_json::json!([block("café review done")]),
            ),
        ],
    );

    let mut index = index_for(&dir);
    // Cyrillic word, mid-sentence, in the nominative and the inflected form.
    assert_eq!(
        index
            .search("релиз", None)
            .unwrap()
            .messages
            .first()
            .map(|hit| hit.session_id.clone()),
        Some("session_cyr".to_string())
    );
    assert_eq!(index.search("Проверь", None).unwrap().messages.len(), 1);
    // SQLite's `remove_diacritics` does not decompose Cyrillic `ё`, so the
    // index folds it itself: either spelling finds the word, like typists use it.
    assert_eq!(index.search("еще", None).unwrap().messages.len(), 1);
    assert_eq!(index.search("ещё", None).unwrap().messages.len(), 1);
    // Latin diacritics, same rule.
    assert_eq!(index.search("cafe", None).unwrap().messages.len(), 1);
    assert_eq!(index.search("café", None).unwrap().messages.len(), 1);
    // Cyrillic titles are searchable in the title section.
    assert_eq!(index.search("совещание", None).unwrap().titles.len(), 1);
}

#[test]
fn prefix_matching_keeps_typing_responsive() {
    let dir = test_dir("prefix");
    write_session(
        &dir,
        "session_prefix",
        "Debounce",
        vec![user_message(
            "m1",
            "implement the debounce timer",
            "2026-01-02T03:04:06Z",
        )],
    );
    let mut index = index_for(&dir);
    assert_eq!(index.search("debou", None).unwrap().messages.len(), 1);
    assert_eq!(index.search("deb", None).unwrap().messages.len(), 1);
}

#[test]
fn query_text_never_becomes_fts_syntax() {
    let dir = test_dir("syntax");
    write_session(
        &dir,
        "session_syntax",
        "Operators",
        vec![user_message(
            "m1",
            "ask about AND OR NOT NEAR",
            "2026-01-02T03:04:06Z",
        )],
    );
    let mut index = index_for(&dir);
    // Bare FTS5 keywords and punctuation would be a syntax error unquoted.
    for query in [
        "AND",
        "OR",
        "NOT",
        "NEAR",
        "\"quoted\"",
        "-minus",
        "*",
        "(",
        "a OR b",
    ] {
        assert!(
            index.search(query, None).is_ok(),
            "query {query:?} must not fail"
        );
    }
    assert_eq!(index.search("AND", None).unwrap().messages.len(), 1);
    assert!(index.search("   ", None).unwrap().messages.is_empty());
    assert_eq!(index.search("   ", None).unwrap().indexed_sessions, 1);
}

#[test]
fn sync_is_incremental_and_picks_up_appends_and_retitles() {
    let dir = test_dir("incremental");
    let path = write_session(
        &dir,
        "session_live",
        "First title",
        vec![user_message("m1", "hello there", "2026-01-02T03:04:06Z")],
    );
    let mut index = index_for(&dir);

    let first = index.sync();
    assert_eq!(first.indexed, 0, "already synced by index_for");
    assert_eq!(first.unchanged, 1);

    // A second sync with nothing changed parses nothing.
    let idle = index.sync();
    assert_eq!(idle.indexed, 0);
    assert_eq!(idle.scanned, 1);

    // Append through the journal, the way a live session does, and bump mtime.
    let journal = dir.join("sessions/session_live.journal.jsonl");
    fs::write(
        &journal,
        concat!(
            r#"{"meta":{"title":"First title"},"append_messages":[]}"#,
            "\n",
            r#"{"meta":{"title":"First title"},"append_messages":[{"id":"m2","role":"user","content":[{"type":"text","text":"typing about walruses"}],"timestamp":"2026-01-02T03:05:00.000Z"}]}"#,
            "\n"
        ),
    )
    .expect("write journal");
    // Make the mtime clearly newer than the snapshot on coarse filesystems.
    let snapshot = fs::read(&path).unwrap();
    fs::write(&path, snapshot).unwrap();

    let after_append = index.sync();
    assert_eq!(after_append.indexed, 1);
    assert_eq!(index.search("walruses", None).unwrap().messages.len(), 1);

    // A title change is reflected without touching the transcript.
    index.note_title("session_live", "Walrus plans");
    let retitled = index.search("walrus plans", None).unwrap();
    assert_eq!(retitled.titles.len(), 1);
    assert_eq!(retitled.titles[0].title, "Walrus plans");
}

#[test]
fn deleted_sessions_leave_the_index() {
    let dir = test_dir("prune");
    let path = write_session(
        &dir,
        "session_gone",
        "Temporary",
        vec![user_message(
            "m1",
            "ephemeral content",
            "2026-01-02T03:04:06Z",
        )],
    );
    let mut index = index_for(&dir);
    assert_eq!(index.search("ephemeral", None).unwrap().messages.len(), 1);
    fs::remove_file(path).unwrap();
    let stats = index.sync();
    assert_eq!(stats.pruned, 1);
    assert!(index.search("ephemeral", None).unwrap().messages.is_empty());
    assert_eq!(index.indexed_sessions(), 0);
}

#[test]
fn system_scaffolding_is_not_indexed() {
    let dir = test_dir("scaffolding");
    write_session(
        &dir,
        "session_system",
        "Scaffolding",
        vec![
            serde_json::json!({
                "id": "m0",
                "role": "user",
                "content": [block("<system-reminder>\nsecret scaffolding\n</system-reminder>")],
                "display_role": "system",
                "timestamp": "2026-01-02T03:00:00Z",
            }),
            serde_json::json!({
                "id": "m1",
                "role": "user",
                "content": [block("real question about beavers")],
                "timestamp": "2026-01-02T03:04:06Z",
            }),
        ],
    );
    let mut index = index_for(&dir);
    assert!(
        index
            .search("scaffolding", None)
            .unwrap()
            .messages
            .is_empty()
    );
    assert_eq!(index.search("beavers", None).unwrap().messages.len(), 1);
}

#[test]
fn timestamps_parse_rfc3339_with_fractions_and_offsets() {
    assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_timestamp_ms("2026-01-02T03:04:05.500Z"),
        Some(1_767_323_045_500)
    );
    assert_eq!(
        parse_timestamp_ms("2026-01-02T03:04:05+11:00"),
        Some(1_767_323_045_000 - 11 * 3_600_000)
    );
    assert_eq!(
        parse_timestamp_ms("2026-01-02T03:04:05.5-05:30"),
        Some(1_767_323_045_500 + 5 * 3_600_000 + 30 * 60_000)
    );
    assert_eq!(parse_timestamp_ms("not a timestamp"), None);
    assert_eq!(parse_timestamp_ms("2026-13-02T03:04:05Z"), None);
}

#[test]
fn snippet_highlights_are_char_offsets_not_bytes() {
    let raw = format!("{SNIPPET_START}релиз{SNIPPET_END} готов");
    let (text, highlights) = split_highlights(&raw);
    assert_eq!(text, "релиз готов");
    assert_eq!(
        highlights,
        vec![SessionSearchHighlight { start: 0, end: 5 }]
    );
    let marked: String = text
        .chars()
        .skip(highlights[0].start as usize)
        .take((highlights[0].end - highlights[0].start) as usize)
        .collect();
    assert_eq!(marked, "релиз");
}

#[test]
fn limit_caps_each_section_and_reports_truncation() {
    let dir = test_dir("limit");
    let mut messages = Vec::new();
    for i in 0..6 {
        messages.push(user_message(
            &format!("m{i}"),
            "needle here",
            "2026-01-02T03:04:06Z",
        ));
    }
    write_session(&dir, "session_many", "needle title", messages);
    let mut index = index_for(&dir);
    let results = index.search("needle", Some(2)).unwrap();
    assert_eq!(results.messages.len(), 2);
    assert!(results.truncated);
    let all = index.search("needle", None).unwrap();
    assert_eq!(all.messages.len(), 6);
}

#[test]
fn full_text_index_backs_the_query_it_claims_to() {
    // Guards the FTS5 build actually being linked: a plain `LIKE` fallback
    // would pass the behavioural tests but not this one.
    let dir = test_dir("fts5");
    let index = SearchIndex::open_at(&dir.join("search.db"), &dir.join("sessions")).unwrap();
    let answer: String = index
        .connection
        .query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get(0),
        )
        .map(|_: i64| "enabled".to_string())
        .unwrap_or_else(|error| format!("missing: {error}"));
    assert_eq!(answer, "enabled");
}

/// Backfill and query cost against a real corpus.
///
/// Ignored by default because it needs a copy of a real `~/.jcode/sessions`:
///
/// ```text
/// rsync -a ~/.jcode/sessions/ "$JCODE_SEARCH_PERF_DIR/sessions/"
/// JCODE_SEARCH_PERF_DIR=... cargo test -p jcode-harness-api-server --lib \
///     backfill_and_query_latency -- --ignored --nocapture
/// ```
///
/// The point is the shape as much as the numbers: one cold backfill parses
/// every session, every later sync stats them and parses nothing, and a query
/// stays in the low milliseconds because it reads the index, not the files.
#[test]
#[ignore = "needs JCODE_SEARCH_PERF_DIR pointing at a copy of a real sessions dir"]
fn backfill_and_query_latency() {
    let Ok(root) = std::env::var("JCODE_SEARCH_PERF_DIR") else {
        eprintln!("set JCODE_SEARCH_PERF_DIR to a copy of a sessions directory");
        return;
    };
    let root = PathBuf::from(root);
    let sessions = root.join("sessions");
    let mut index = SearchIndex::open_at(&root.join("perf-search.db"), &sessions).unwrap();

    let started = Instant::now();
    let cold = index.sync();
    let backfill = started.elapsed();

    let started = Instant::now();
    let warm = index.sync();
    let incremental = started.elapsed();
    assert_eq!(warm.indexed, 0, "a restart must parse nothing");
    assert_eq!(warm.scanned, cold.scanned);

    let bytes: u64 = std::fs::read_dir(&sessions)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum();
    let messages: i64 = index
        .connection
        .query_row("SELECT COUNT(*) FROM search_messages", [], |row| row.get(0))
        .unwrap();

    let mut latencies = Vec::new();
    for query in [
        "the",
        "session",
        "rust",
        "error",
        "github",
        "tests",
        "проверить",
        "walrus",
    ] {
        for _ in 0..5 {
            let started = Instant::now();
            let results = index.search(query, None).unwrap();
            latencies.push((started.elapsed(), results.messages.len()));
        }
    }
    latencies.sort_by_key(|(elapsed, _)| *elapsed);
    let median = latencies[latencies.len() / 2].0;
    let worst = latencies[latencies.len() - 1].0;
    eprintln!(
        "search perf: {} sessions, {:.1} MB of JSON, {} indexed messages\n\
         cold backfill {:.0}ms, incremental sync {:.2}ms, query median {:.2}ms, worst {:.2}ms",
        cold.indexed,
        bytes as f64 / (1024.0 * 1024.0),
        messages,
        backfill.as_secs_f64() * 1_000.0,
        incremental.as_secs_f64() * 1_000.0,
        median.as_secs_f64() * 1_000.0,
        worst.as_secs_f64() * 1_000.0,
    );
    assert!(cold.indexed > 0, "the corpus must contain sessions");
    // Generous bounds: they catch an accidental full re-parse or a scan per
    // query, not a slow machine.
    assert!(backfill.as_secs() < 60, "cold backfill took {backfill:?}");
    assert!(
        incremental.as_millis() < 500,
        "an unchanged sync took {incremental:?}"
    );
    assert!(median.as_millis() < 250, "median query took {median:?}");
}
